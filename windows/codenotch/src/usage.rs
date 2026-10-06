//! Claude usage adapter (official), implemented from the upstream Codenotch's documented behaviour.
//! Endpoint: GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1 (the flag adds the unused-resets block)
//! Headers: Authorization: Bearer <token>; anthropic-beta: oauth-2025-04-20; 15 s timeout
//! Rules (upstream's discipline):
//!   - the credential comes from Claude Code's own store (Windows: ~/.claude/.credentials.json), read only
//!   - accounts, plural: ~/.claude and every ~/.claude-<slug> holding a credential. That layout is not invented
//!     here — it is what CLAUDE_CONFIG_DIR points a shell at, and what the Mac app already reads several accounts
//!     by. Each account's windows carry its name in `group`, so the card stacks them exactly as Antigravity's
//!     model families stack, and a machine with one account produces byte-for-byte the old reading
//!   - 401 → re-read the credential once and retry (Claude Code may have just refreshed the token) → still
//!     failing means needsAuth; 403 is access denied, not proof of lost authentication
//!   - 429 → back off 60 s × 2^n capped at 15 min, Retry-After only raises it, even past the cap; the deadline is persisted
//!   - an expired token is never sent: the endpoint answers it with 429 + Retry-After ≈ 3600, not 401, so sending it
//!     reads as "rate limited" for as long as the token stays stale (upstream's credentialExpired, no network)
//!   - the token is renewed by running the standalone `claude -p` with an empty stdin shortly before it expires
//!     (upstream's ClaudeTokenRefresher). Only that CLI writes ~/.claude/.credentials.json — Claude Code inside the
//!     desktop app renews its own copy elsewhere — so without this the file rots eight hours after the last CLI run
//!   - never invent a percentage on failure: keep the last reading marked stale, and the UI shows how old it is
//!
//! Reply (snake_case): { limits:[{kind,percent,resets_at}], five_hour:{utilization,resets_at}, seven_day:{...} }
//! limits is the forward-compatible main shape; five_hour/seven_day are merged in as a fallback (a window that just rolled over disappears from limits).

use crate::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// `cedar_ember=1` opts into the unused-resets block, as upstream asks
const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1";
const POLL_ACTIVE_SECS: u64 = 60;
const POLL_IDLE_SECS: u64 = 300;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 900;
/// Renew when this close to expiry. Must stay under Claude Code's own five minutes: its start-up renews the token
/// only when now + 300 s >= expiresAt, so launching any earlier is a no-op that would be judged a failure
const RENEW_MARGIN_MS: u64 = 4 * 60 * 1000;
const RENEW_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// A token that did not renew is tried again, each wait twice the last, never more than an hour apart
const RENEW_RETRY_CAP_MS: u64 = 60 * 60 * 1000;
const RENEW_TIMEOUT_SECS: u64 = 30;
const EXPIRED_NOTE: &str = "Credential expired — run claude once in a terminal to renew it";

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Immediate refresh from the tray or a command
pub fn request_refresh() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sleep in slices so request_refresh can interrupt it
fn sleep_interruptible(total_secs: u64) {
    for _ in 0..total_secs {
        if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const CRED_NAMES: [&str; 2] = [".credentials.json", "credentials.json"];

/// One Claude Code account, as its config directory. `slug` is None for the default ~/.claude and
/// Some("work") for ~/.claude-work.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    dir: PathBuf,
    slug: Option<String>,
}

impl Profile {
    fn name(&self) -> String {
        self.slug.clone().unwrap_or_else(|| "default".into())
    }

    /// The heading the card files this account's windows under. The plan is what tells two accounts
    /// apart at a glance ("max" against "pro"); the slug is what stays unique when both plans match.
    fn group(&self, plan: Option<&str>) -> String {
        match plan {
            Some(p) if !p.is_empty() => format!("{} · {p}", self.name()),
            _ => self.name(),
        }
    }
}

fn has_credential(dir: &Path) -> bool {
    CRED_NAMES.iter().any(|n| dir.join(n).is_file())
}

/// Every account on the machine: the default first, then ~/.claude-<slug> in name order. A secondary
/// directory counts only once it holds a credential, so a half-made one never shows up on the card as
/// an account waiting to be signed in.
fn profiles() -> Vec<Profile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut out = vec![Profile { dir: home.join(".claude"), slug: None }];
    let mut extra: Vec<Profile> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(slug) = name.strip_prefix(".claude-") else {
                continue;
            };
            let dir = e.path();
            if slug.is_empty() || !dir.is_dir() || !has_credential(&dir) {
                continue;
            }
            extra.push(Profile { dir, slug: Some(slug.to_string()) });
        }
    }
    extra.sort_by(|a, b| a.slug.cmp(&b.slug));
    out.append(&mut extra);
    out
}

/// Every account directory, for anything that watches a profile's files (the session watcher)
pub fn profile_dirs() -> Vec<PathBuf> {
    profiles().into_iter().map(|p| p.dir).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    /// 0.0–1.0 (fraction used)
    pub used: f64,
    /// Reset time, ms epoch (None = unknown)
    pub resets_at: Option<u64>,
    /// Pure count window (no published denominator, e.g. Antigravity's requests today) — the cell shows ~N and the ring draws only its track
    #[serde(default)]
    pub count: Option<i64>,
    /// The number is ours, not the vendor's (upstream fidelity=.derived) — the card adds a ~ prefix
    #[serde(default)]
    pub derived: bool,
    /// The heading the window sits under on the card, for a provider that reports the same windows
    /// for several things (Antigravity: a 5-hour and a weekly lane per model family). None = ungrouped
    #[serde(default)]
    pub group: Option<String>,
    /// The window's full length in seconds, where the provider states or implies it (upstream
    /// `LimitWindow.duration`). With `resets_at` it is what the card's usage pace compares the
    /// share used against. None = unknown, and then no pace is shown rather than a guessed one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    /// A money balance rather than a quota (upstream `UsageMoneyBreakdown`): the card draws it as
    /// spent / remaining / funded. `used` still carries spent ÷ funded, for the ring and the tray
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub money: Option<Money>,
}

/// Amounts as the provider reported them, in `currency`'s major unit (dollars, not cents)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Money {
    pub currency: String,
    pub spent: f64,
    pub remaining: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageSnapshot {
    /// ok | stale | needsAuth | backoff | error
    pub status: String,
    pub windows: Vec<LimitWindow>,
    pub fetched_at: u64,
    pub note: String,
    #[serde(default)]
    pub backoff_until: u64,
    /// The account's named tier ("Max", "Plus", "Pro"…), shown under the card's title as on the
    /// Mac. None where the provider names none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Token statistics from the provider's profile (Codex only, upstream `CodexTokenUsage`):
    /// the card lists them and draws the last 30 days as bars. None = nothing to show
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    /// Unused rate-limit resets on the account (upstream `UsageResetCredits`). None = the
    /// provider did not say, which is not the same as zero
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCredits>,
}

/// Unused resets as the provider listed them. `available_count` is the provider's own total and
/// is trusted over the list, which Codex can truncate; the card takes expired ones off at render
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResetCredits {
    pub available_count: u64,
    #[serde(default)]
    pub credits: Vec<ResetCredit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResetCredit {
    #[serde(default)]
    pub id: String,
    /// "available" is the only status that counts
    #[serde(default)]
    pub status: String,
    /// ms epoch; None = does not expire as far as anyone said
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// How many resets this one entry stands for (a Claude grant can hold several)
    #[serde(default = "one")]
    pub count: u64,
}

fn one() -> u64 {
    1
}

/// Claude's `cedar_ember` block → unused resets, upstream `ClaudeResetCredits.credits(at:)`.
/// `{eligible, ineligible_reason, grants:[{id, resets_left, starts_at, ends_at, paused}]}`.
/// OAuth is refused this surface even for an eligible account (`ineligible_reason: "surface"`):
/// that is unknown, not "none left", so it is None. A grant that cannot be read is skipped
fn claude_reset_credits(v: &serde_json::Value, now: u64) -> Option<ResetCredits> {
    let block = v.get("cedar_ember").filter(|x| x.is_object())?;
    let eligible = block.get("eligible").and_then(|x| x.as_bool())?;
    if !eligible && block.get("ineligible_reason").and_then(|x| x.as_str()) == Some("surface") {
        return None;
    }
    let grants = block.get("grants").and_then(|x| x.as_array())?;
    let mut credits = Vec::new();
    if eligible {
        for g in grants {
            let (Some(left), Some(starts), Some(ends)) = (
                g.get("resets_left").and_then(|x| x.as_u64()),
                g.get("starts_at").and_then(parse_reset),
                g.get("ends_at").and_then(parse_reset),
            ) else {
                continue;
            };
            let paused = g.get("paused").and_then(|x| x.as_bool()).unwrap_or(false);
            if left > 0 && !paused && starts <= now && ends > now {
                credits.push(ResetCredit {
                    id: g.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    status: "available".into(),
                    expires_at: Some(ends),
                    count: left,
                });
            }
        }
    }
    let available_count = credits.iter().try_fold(0u64, |a, c| a.checked_add(c.count))?;
    Some(ResetCredits { available_count, credits })
}

/// Lifetime figures and per-day tokens, exactly as the provider published them. Each figure
/// is optional on its own: a missing one is shown as a dash, never as zero
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    #[serde(default)]
    pub lifetime_tokens: Option<u64>,
    #[serde(default)]
    pub peak_daily_tokens: Option<u64>,
    #[serde(default)]
    pub longest_turn_secs: Option<f64>,
    #[serde(default)]
    pub current_streak_days: Option<u64>,
    #[serde(default)]
    pub longest_streak_days: Option<u64>,
    /// One entry per day that has one, keyed by its calendar day ("2026-10-06"). A day the
    /// provider has not published yet is absent, which the card reads as pending, not zero
    #[serde(default)]
    pub daily: Vec<DailyTokens>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DailyTokens {
    pub date: String,
    pub tokens: u64,
}

/// The plan the way the Mac card writes it (`ClaudeOAuthProvider.planName`): an all-lowercase
/// wire name ("max", "plus") gets a capital, anything else ("Pro+", "SuperGrok") is left alone,
/// and blank is no plan
pub fn plan_name(raw: Option<&str>) -> Option<String> {
    let plan = raw.map(str::trim).filter(|p| !p.is_empty())?;
    if !plan.chars().all(char::is_lowercase) {
        return Some(plan.to_string());
    }
    let mut c = plan.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str())
}

fn store_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("usage.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        // The status it was saved with is the status it comes back with: a reading persisted a
        // minute before a restart is a minute old, not stale, and `fetched_at` came back with it,
        // so whatever reads this can tell the difference on its own.
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

#[derive(Default)]
struct Credential {
    token: String,
    /// ms epoch (None = the file names no expiry)
    expires_at: Option<u64>,
    /// "max" | "pro" | … as the credential names it, for the card's account heading
    plan: Option<String>,
}

impl Credential {
    fn expired(&self, now: u64) -> bool {
        self.expires_at.map(|e| e <= now).unwrap_or(false)
    }
}

/// Reads Claude Code's OAuth credential.
fn read_credentials(dir: &Path) -> Option<Credential> {
    for name in CRED_NAMES {
        let p = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let oauth = v.get("claudeAiOauth").unwrap_or(&v);
        if let Some(tok) = oauth.get("accessToken").and_then(|x| x.as_str()) {
            // An empty token is signed out, not expired: fall through to the next
            // candidate file rather than report a credential that cannot be used.
            if tok.trim().is_empty() {
                continue;
            }
            let expires_at = oauth.get("expiresAt").and_then(|x| x.as_f64()).map(|ms| ms as u64);
            let plan = oauth.get("subscriptionType").and_then(|x| x.as_str()).map(String::from);
            return Some(Credential { token: tok.to_string(), expires_at, plan });
        }
    }
    None
}

/// For doctor: credential probe report (prints no secret values)
pub fn probe_credentials() -> String {
    let cli = match find_cli() {
        Some(p) => format!("renews via {}", p.display()),
        None => "no standalone claude CLI found to renew it".into(),
    };
    let list = profiles();
    if list.is_empty() {
        return format!("credential: no home directory to read ~/.claude from; {cli}");
    }
    let lines: Vec<String> = list
        .iter()
        .map(|p| match read_credentials(&p.dir) {
            Some(c) => format!(
                "credential[{}]: found (token {} chars, {}, plan {})",
                p.name(),
                c.token.len(),
                if c.expired(now_ms()) { "expired" } else { "valid" },
                c.plan.as_deref().unwrap_or("?")
            ),
            None => format!(
                "credential[{}]: {} not found (needsAuth; the desktop app may use another store — signing in once with the Claude Code CLI creates it)",
                p.name(),
                p.dir.join(CRED_NAMES[0]).display()
            ),
        })
        .collect();
    format!("{}; {cli}", lines.join("
  "))
}

// ---------------- token renewal (upstream's ClaudeTokenRefresher) ----------------

/// Anything under these belongs to the desktop app: its bundled Claude Code keeps its token in the desktop app's
/// own store and never writes ~/.claude/.credentials.json, so renewing with it would change nothing here
fn is_desktop_owned(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    s.contains("\\anthropicclaude\\") || s.contains("\\claude\\claude-code\\") || s.contains("\\windowsapps\\")
}

/// The command file names to try for a CLI, most specific first. Windows needs the
/// native exe before the .cmd shim; elsewhere there is only the bare name.
pub(crate) fn command_names(stem: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![format!("{stem}.exe"), format!("{stem}.cmd")]
    } else {
        vec![stem.to_string()]
    }
}

/// The standalone Claude Code command: its own installer's location first, then global npm/pnpm/Volta, then PATH
pub(crate) fn find_cli() -> Option<std::path::PathBuf> {
    let names = command_names("claude");
    let mut v = Vec::new();
    let push_all = |dir: std::path::PathBuf, v: &mut Vec<std::path::PathBuf>| {
        for n in &names {
            v.push(dir.join(n));
        }
    };
    if let Some(h) = dirs::home_dir() {
        push_all(h.join(".local").join("bin"), &mut v);
    }
    if let Some(d) = dirs::config_dir() {
        push_all(d.join("npm"), &mut v);
    }
    if let Some(d) = dirs::data_local_dir() {
        push_all(d.join("pnpm"), &mut v);
    }
    if let Some(h) = dirs::home_dir() {
        push_all(h.join(".volta").join("bin"), &mut v);
        // npm's global prefix and nvm's per-version bin are where a Linux install usually lands
        push_all(h.join(".npm-global").join("bin"), &mut v);
        push_all(h.join(".bun").join("bin"), &mut v);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push_all(dir, &mut v);
        }
    }
    v.into_iter().find(|p| p.is_file() && !is_desktop_owned(p))
}

/// Whether a launch is worth making. Pure, so every branch is testable without a clock or a subprocess.
/// grok.rs times its CLI launches by the same rule.
pub(crate) fn should_renew(
    expires_at: Option<u64>,
    now: u64,
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    failures: u32,
) -> bool {
    // Nothing read yet: never launch on a guess
    let Some(exp) = expires_at else { return false };
    // Plenty of time left — also where launching would do nothing, because the CLI's own gate has not opened
    if exp > now + RENEW_MARGIN_MS {
        return false;
    }
    let Some(t) = last_attempt else { return true };
    // A launch that failed to move the expiry leaves the same value here. One failed launch — asleep, offline, a
    // busy CLI — must not freeze the ring until someone opens a terminal, so the same token is tried again, but
    // on a doubling wait, so a token that cannot renew does not become a launch every tick
    let wait = if attempted_for == Some(exp) { retry_wait_ms(failures) } else { RENEW_COOLDOWN_MS };
    now.saturating_sub(t) >= wait
}

fn retry_wait_ms(failures: u32) -> u64 {
    RENEW_COOLDOWN_MS.saturating_mul(1u64 << failures.min(16)).min(RENEW_RETRY_CAP_MS)
}

/// `claude -p` with a null stdin starts up (which is where it renews an aged token), then exits non-zero for want
/// of a prompt: no conversation, no transcript. Output goes nowhere — a token could in principle be echoed into it.
fn run_renewal(cli: &std::path::Path, dir: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(cli);
    cmd.arg("-p").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Launched from inside a Claude Code session, the child would take the host's auth and leave the file alone
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_") {
            cmd.env_remove(k.as_ref());
        }
    }
    // Which account gets renewed is said here, never inherited: CLAUDE_CONFIG_DIR is not CLAUDE_CODE_*, so it
    // survives the loop above, and a Codenotch started from a shell pointed at another account used to renew
    // that one while the account on screen stayed expired.
    cmd.env("CLAUDE_CONFIG_DIR", dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(RENEW_TIMEOUT_SECS);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[derive(Default)]
struct Renewer {
    attempted_for: Option<u64>,
    last_attempt: Option<u64>,
    /// Launches in a row that left `attempted_for` where it was
    failures: u32,
}

impl Renewer {
    /// Renews if the token is about to expire. Some(true) = the expiry moved; judged on the outcome, never on the
    /// exit status, because refusing the empty prompt is a non-zero exit and a successful renewal at the same time
    fn maybe_renew(&mut self, cred: &Credential, dir: &Path, who: &str) -> Option<bool> {
        let _auth = crate::claude_auth::try_acquire()?;
        let now = now_ms();
        if !should_renew(cred.expires_at, now, self.attempted_for, self.last_attempt, self.failures) {
            return None;
        }
        if self.attempted_for != cred.expires_at {
            self.failures = 0;
        }
        self.last_attempt = Some(now);
        self.attempted_for = cred.expires_at;
        self.failures = self.failures.saturating_add(1);
        let Some(cli) = find_cli() else {
            crate::applog(&format!(
                "claude[{who}]: token about to expire and no standalone claude CLI found to renew it"
            ));
            return Some(false);
        };
        if let Err(e) = run_renewal(&cli, dir) {
            crate::applog(&format!("claude[{who}]: token renewal could not start ({}): {e}", cli.display()));
            return Some(false);
        }
        let after = read_credentials(dir).and_then(|c| c.expires_at);
        let renewed = matches!((after, cred.expires_at), (Some(a), Some(b)) if a > b);
        crate::applog(&if renewed {
            format!("claude[{who}]: token renewed via {}", cli.display())
        } else {
            format!("claude[{who}]: ran {} but the token expiry did not move", cli.display())
        });
        Some(renewed)
    }
}

fn parse_reset(v: &serde_json::Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "seven_day" | "weekly_all" => "Weekly (all models)".into(),
        "seven_day_opus" | "weekly_opus" => "Weekly (Opus)".into(),
        "weekly_scoped" => "Weekly (model-scoped)".into(),
        other => {
            // Forward compatibility: an unknown kind gets a readable label
            let mut s = other.replace('_', " ");
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

/// The window's length, as upstream's `UsageResponse.duration(forKind:)`: the session is five
/// hours, every weekly kind a week, anything else unknown
fn duration_for(kind: &str) -> Option<u64> {
    match kind {
        "session" | "five_hour" => Some(5 * 3600),
        k if k.starts_with("weekly") || k.starts_with("seven_day") => Some(7 * 86400),
        _ => None,
    }
}

/// `spend` as a money window, or None where the seat has no credit spending. Upstream's
/// `spendWindow`: the share is the two amounts divided rather than `spend.percent`, which is
/// rounded to whole percent and would put the bar visibly off the figures beside it. Amounts
/// arrive in minor units with their own exponent (20000 with exponent 2 is 200.00).
fn spend_window(v: &serde_json::Value) -> Option<LimitWindow> {
    let spend = v.get("spend").filter(|x| x.is_object())?;
    if spend.get("enabled").and_then(|x| x.as_bool()) == Some(false) {
        return None;
    }
    let amount = |key: &str| -> Option<(f64, Option<String>)> {
        let a = spend.get(key)?;
        let minor = a.get("amount_minor").and_then(|x| x.as_f64())?;
        let exponent = a.get("exponent").and_then(|x| x.as_i64()).unwrap_or(2);
        let currency = a.get("currency").and_then(|x| x.as_str()).map(String::from);
        Some((minor / 10f64.powi(exponent as i32), currency))
    };
    let (used, used_currency) = amount("used")?;
    let (limit, limit_currency) = amount("limit")?;
    if !(limit > 0.0) || !used.is_finite() {
        return None;
    }
    Some(LimitWindow {
        id: "spend".into(),
        label: "Spend limit".into(),
        used: (used / limit).clamp(0.0, 1.0),
        // No reset time: the response carries none for this block, and a balance still says
        // what it says without one
        money: Some(Money {
            currency: limit_currency.or(used_currency).unwrap_or_else(|| "USD".into()),
            spent: used,
            remaining: (limit - used).max(0.0),
        }),
        ..Default::default()
    })
}

fn parse_response(v: &serde_json::Value) -> Vec<LimitWindow> {
    let mut out: Vec<LimitWindow> = Vec::new();
    if let Some(arr) = v.get("limits").and_then(|x| x.as_array()) {
        for l in arr {
            let Some(kind) = l.get("kind").and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(pct) = l.get("percent").and_then(|x| x.as_f64()) else {
                continue;
            };
            let resets = l.get("resets_at").and_then(parse_reset);
            if resets.is_none() {
                continue; // upstream rule: a window without a reset time is not shown
            }
            out.push(LimitWindow {
                id: kind.to_string(),
                label: label_for(kind),
                used: (pct / 100.0).clamp(0.0, 1.0),
                resets_at: resets,
                duration: duration_for(kind),
                ..Default::default()
            });
        }
    }
    // Fallback merge: a window that just rolled over disappears from limits while the named field remains.
    // In practice the kinds in limits are weekly_all/weekly_scoped, not seven_day — deduplicating by id
    // alone would add the seven_day fallback a second time (the card showed "Weekly all" and
    // "Weekly (all models)" as twins). Three dedupe rules: id alias / same resets_at and percentage / same label.
    let aliases: [(&str, &str, &[&str]); 2] = [
        ("five_hour", "session", &["session", "five_hour"]),
        ("seven_day", "seven_day", &["seven_day", "weekly_all", "weekly"]),
    ];
    for (field, id, alias) in aliases {
        let Some(w) = v.get(field) else { continue };
        let Some(u) = w.get("utilization").and_then(|x| x.as_f64()) else { continue };
        let used = (u / 100.0).clamp(0.0, 1.0);
        let resets_at = w.get("resets_at").and_then(parse_reset);
        let label = label_for(id);
        let dup = out.iter().any(|x| {
            alias.contains(&x.id.as_str())
                || x.label == label
                || (resets_at.is_some()
                    && x.resets_at.map(|r| r / 1000) == resets_at.map(|r| r / 1000)
                    && (x.used - used).abs() < 0.005)
        });
        if dup {
            continue;
        }
        out.push(LimitWindow { id: id.into(), label, used, resets_at, duration: duration_for(id), ..Default::default() });
    }
    // An Enterprise seat reports only this: no limits and both named windows null, so without
    // it such a seat has no reading at all. Nothing else in the reply is read, on purpose:
    // several codenamed objects carry dollars and resets and look like windows, but what they
    // limit is not published (upstream's reasoning, kept as it is)
    if let Some(spend) = spend_window(v) {
        out.push(spend);
    }
    // session always comes first (upstream display order); the balance stays last
    out.sort_by_key(|w| match w.id.as_str() {
        "session" => 0,
        "spend" => 2,
        _ => 1,
    });
    out
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64), // suggested wait in seconds (the Retry-After before the floor is applied)
    Other(String),
}

/// What one read of the endpoint yields
struct Reading {
    windows: Vec<LimitWindow>,
    resets: Option<ResetCredits>,
}

fn fetch_once(token: &str) -> Result<Reading, FetchErr> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r
                .into_json()
                .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
            Ok(Reading { windows: parse_response(&v), resets: claude_reset_credits(&v, now_ms()) })
        }
        Err(ureq::Error::Status(401, _)) => Err(FetchErr::NeedsAuth),
        Err(ureq::Error::Status(403, _)) => Err(FetchErr::Other(
            "Claude HTTP 403: access denied. Check network or account access; sign-in may still be valid.".into())),
        Err(ureq::Error::Status(429, r)) => {
            let ra = r
                .header("retry-after")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            Err(FetchErr::RateLimited(ra))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn backoff_secs(consecutive: u32, retry_after_floor: u64) -> u64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1u64 << consecutive.min(4));
    // The server's Retry-After is honoured in full: with expired tokens no longer
    // sent, a long one is a real rate limit, and retrying early only earns another.
    exp.clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS).max(retry_after_floor)
}

fn set_and_broadcast(app: &AppHandle, mutate: impl FnOnce(&mut UsageSnapshot)) {
    let st = app.state::<AppState>();
    let snap = {
        let mut u = st.usage.lock().unwrap();
        mutate(&mut u);
        u.clone()
    };
    persist(&snap);
    let _ = app.emit("usage", &snap);
}

/// What one account contributes to the shared reading. Kept across ticks so a refresh that fails for
/// one account keeps showing that account's last good windows, and never blanks the other one.
#[derive(Default)]
struct Account {
    renewer: Renewer,
    consecutive_429: u32,
    backoff_until: u64,
    windows: Vec<LimitWindow>,
    status: String,
    note: String,
    fetched_at: u64,
    /// subscriptionType from the credential, as last read
    plan: Option<String>,
    /// Unused resets from the last good read
    resets: Option<ResetCredits>,
}

fn key(p: &Profile) -> String {
    p.dir.to_string_lossy().to_string()
}

/// The account's windows as they go on the card: its name in `group`, and for a secondary account an
/// id suffixed with the slug -- which is what keeps `by_id("session")` in the notch meaning the default
/// account's session and not whichever account answered first.
fn decorate(mut windows: Vec<LimitWindow>, p: &Profile, group: Option<&str>) -> Vec<LimitWindow> {
    for w in &mut windows {
        if let Some(g) = group {
            w.group = Some(g.to_string());
        }
        if let Some(slug) = &p.slug {
            w.id = format!("{}@{slug}", w.id);
        }
    }
    windows
}

/// Hands each account back the windows it contributed before the restart: a secondary account's ids
/// carry `@slug`, so both accounts come back from disk instead of only the default one.
fn split_persisted(snap: &UsageSnapshot, order: &[Profile]) -> HashMap<String, Vec<LimitWindow>> {
    let mut out: HashMap<String, Vec<LimitWindow>> = HashMap::new();
    for w in &snap.windows {
        let owner = order.iter().find(|p| match &p.slug {
            Some(sl) => w.id.ends_with(&format!("@{sl}")),
            None => !w.id.contains('@'),
        });
        if let Some(p) = owner {
            out.entry(key(p)).or_default().push(w.clone());
        }
    }
    out
}

/// One reading out of every account's, in profile order. The status is the best news any account has:
/// a second account that needs signing in must not dim a first one that just answered.
fn aggregate(order: &[Profile], accounts: &HashMap<String, Account>) -> UsageSnapshot {
    let rank = |s: &str| match s {
        "ok" => 0,
        "stale" => 1,
        "error" => 2,
        _ => 3, // needsAuth, and anything not set yet
    };
    let multi = order.len() > 1;
    let mut snap = UsageSnapshot::default();
    let mut notes: Vec<String> = Vec::new();
    let mut best = 4;
    for p in order {
        let Some(a) = accounts.get(&key(p)) else {
            continue;
        };
        snap.windows.extend(a.windows.iter().cloned());
        snap.fetched_at = snap.fetched_at.max(a.fetched_at);
        if !a.status.is_empty() && rank(a.status.as_str()) < best {
            best = rank(a.status.as_str());
            snap.status = a.status.clone();
        }
        if !a.note.is_empty() {
            notes.push(if multi { format!("{}: {}", p.name(), a.note) } else { a.note.clone() });
        }
        // The soonest deadline is the one worth waking for
        if a.backoff_until > 0 && (snap.backoff_until == 0 || a.backoff_until < snap.backoff_until) {
            snap.backoff_until = a.backoff_until;
        }
    }
    if snap.status.is_empty() {
        snap.status = "needsAuth".into();
    }
    snap.note = notes.join(" · ");
    // One account names its plan under the card's title, as on the Mac. Several already carry
    // theirs in each cell's heading (`Profile::group`), so a shared subtitle would only repeat one
    if !multi {
        let first = order.first().and_then(|p| accounts.get(&key(p)));
        snap.plan = first.and_then(|a| plan_name(a.plan.as_deref()));
        snap.reset_credits = first.and_then(|a| a.resets.clone());
    }
    snap
}

/// One account's turn: renew if the token is aging, then read it, exactly as the single-account loop did.
fn poll_account(p: &Profile, acc: &mut Account, group: Option<&str>) {
    let who = p.name();
    // Ahead of the back-off: renewing never touches the usage endpoint, and a fresh token deserves a fresh try
    if let Some(cred) = read_credentials(&p.dir) {
        if acc.renewer.maybe_renew(&cred, &p.dir, &who) == Some(true) {
            acc.consecutive_429 = 0;
            acc.backoff_until = 0;
        }
    }
    // No requests inside this account's back-off window
    if acc.backoff_until > now_ms() {
        return;
    }
    let cred = read_credentials(&p.dir);
    acc.plan = cred.as_ref().and_then(|c| c.plan.clone());
    match cred {
        None => {
            acc.status = "needsAuth".into();
            acc.note = "No Claude Code credential found".into();
        }
        // Expired is not signed out: keep the last reading, dimmed and dated, and send nothing
        Some(cred) if cred.expired(now_ms()) => {
            acc.status = if acc.windows.is_empty() { "needsAuth" } else { "stale" }.into();
            acc.note = EXPIRED_NOTE.into();
        }
        Some(cred) => {
            let token = cred.token;
            // On 401 re-read the credential and retry once (Claude Code may have just refreshed it)
            let result = match fetch_once(&token) {
                Err(FetchErr::NeedsAuth) => match read_credentials(&p.dir) {
                    Some(c2) if c2.token != token => fetch_once(&c2.token),
                    _ => Err(FetchErr::NeedsAuth),
                },
                other => other,
            };
            match result {
                Ok(reading) => {
                    crate::claude_auth::usage_succeeded();
                    acc.consecutive_429 = 0;
                    acc.status = "ok".into();
                    acc.windows = decorate(reading.windows, p, group);
                    acc.resets = reading.resets;
                    acc.fetched_at = now_ms();
                    acc.note.clear();
                    acc.backoff_until = 0;
                }
                Err(FetchErr::NeedsAuth) => {
                    acc.status = "needsAuth".into();
                    acc.note = "Credential rejected (switched accounts?)".into();
                }
                Err(FetchErr::RateLimited(ra)) => {
                    acc.consecutive_429 += 1;
                    let wait = backoff_secs(acc.consecutive_429 - 1, ra);
                    // The status is left alone: a refused refresh says nothing about the reading we are
                    // holding, which is exactly as old as it was a moment ago. Marking it stale here
                    // dimmed the ring on the first 429, which on Windows is often the first minute of a
                    // rate limit. Age decides, as it does on the Mac (`UsageStore` keeps the previous
                    // status until `staleAfter`), and the note says why it is not moving.
                    acc.note = format!("Rate limited, retrying in {wait}s");
                    acc.backoff_until = now_ms() + wait * 1000;
                }
                Err(FetchErr::Other(msg)) => {
                    // No reading at all is an error worth showing; a reading we could not refresh is
                    // just a reading, and its own age is what makes it stale.
                    if acc.windows.is_empty() {
                        acc.status = "error".into();
                    }
                    acc.note = msg;
                }
            }
        }
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        // Broadcast the persisted old reading at startup (stale beats blank)
        let persisted = {
            let st = app.state::<AppState>();
            let snap = st.usage.lock().unwrap().clone();
            let _ = app.emit("usage", &snap);
            snap
        };
        let mut accounts: HashMap<String, Account> = HashMap::new();
        for (k, windows) in split_persisted(&persisted, &profiles()) {
            accounts.entry(k).or_default().windows = windows;
        }
        loop {
            // A sign-in the user started owns the credential until it finishes. Polling through it
            // reads a file being rewritten and reports a signed-out account mid-login.
            if crate::claude_auth::state().busy {
                sleep_interruptible(2);
                continue;
            }
            // Re-read the list each tick: an account signed into or removed while this runs needs no restart
            let order = profiles();
            let multi = order.len() > 1;
            for p in &order {
                let group = if multi {
                    Some(p.group(read_credentials(&p.dir).and_then(|c| c.plan).as_deref()))
                } else {
                    None
                };
                let acc = accounts.entry(key(p)).or_default();
                poll_account(p, acc, group.as_deref());
            }
            accounts.retain(|k, _| order.iter().any(|p| key(p) == *k));
            let snap = aggregate(&order, &accounts);
            let backoff_until = snap.backoff_until;
            set_and_broadcast(&app, |u| *u = snap);
            // 60 s while a session is active, 300 s otherwise (upstream throttling discipline)
            let active = {
                let st = app.state::<AppState>();
                let store = st.store.lock().unwrap();
                let s = store.snapshot("en", "en", false, false);
                !s.sessions.is_empty()
            };
            let base = if active { POLL_ACTIVE_SECS } else { POLL_IDLE_SECS };
            // A back-off deadline sooner than the next tick is what we wake for, as the single-account
            // loop did when it slept the window out in slices
            let now = now_ms();
            let secs = if backoff_until > now {
                ((backoff_until - now) / 1000).clamp(1, base.min(30))
            } else {
                base
            };
            sleep_interruptible(secs);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXP: u64 = 1_000_000_000;

    #[test]
    fn renews_only_inside_the_margin() {
        assert!(!should_renew(None, EXP, None, None, 0), "never launch on a guess");
        assert!(!should_renew(Some(EXP), EXP - RENEW_MARGIN_MS - 1, None, None, 0), "plenty of time left");
        assert!(should_renew(Some(EXP), EXP - RENEW_MARGIN_MS, None, None, 0));
        assert!(should_renew(Some(EXP), EXP + 3_600_000, None, None, 0), "already expired still renews");
    }

    #[test]
    fn a_new_token_waits_out_the_cooldown() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP + 5), now, Some(EXP), Some(now - 1000), 1), "cooldown holds a new token back");
        assert!(should_renew(Some(EXP + 5), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1));
    }

    #[test]
    fn a_failed_token_is_retried_on_a_doubling_wait() {
        let now = EXP + 1;
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_COOLDOWN_MS), 1), "no retry at the plain cooldown");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 1), "retried after twice the cooldown");
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - 2 * RENEW_COOLDOWN_MS), 2), "the wait doubles");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - 4 * RENEW_COOLDOWN_MS), 2));
        assert!(!should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS + 1), 30), "never a tight loop");
        assert!(should_renew(Some(EXP), now, Some(EXP), Some(now - RENEW_RETRY_CAP_MS), 30), "but never more than an hour apart");
    }

    #[test]
    fn retry_after_never_exceeds_the_cap() {
        assert_eq!(backoff_secs(0, 3600), 3600);
        assert_eq!(backoff_secs(0, 0), BACKOFF_BASE_SECS);
        assert_eq!(backoff_secs(1, 300), 300);
        assert_eq!(backoff_secs(9, 0), BACKOFF_CAP_SECS);
    }

    #[test]
    fn desktop_bundled_cli_is_refused() {
        use std::path::Path;
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Local\AnthropicClaude\app-1.2.3\claude.exe")));
        assert!(is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\Claude\claude-code\2.1.0\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\.local\bin\claude.exe")));
        assert!(!is_desktop_owned(Path::new(r"C:\Users\u\AppData\Roaming\npm\claude.cmd")));
    }

    #[test]
    #[ignore = "Runs the installed standalone claude CLI; opt in for integration verification"]
    fn live_renewal_runs_the_standalone_cli() {
        let cli = find_cli().expect("a standalone claude CLI");
        assert!(!is_desktop_owned(&cli));
        let p = profiles().into_iter().next().expect("a profile");
        let before = read_credentials(&p.dir).and_then(|c| c.expires_at);
        let t = std::time::Instant::now();
        run_renewal(&cli, &p.dir).expect("spawned");
        assert!(t.elapsed() < Duration::from_secs(RENEW_TIMEOUT_SECS), "returned before the timeout");
        let after = read_credentials(&p.dir).and_then(|c| c.expires_at);
        assert!(after >= before, "the expiry never moves backwards");
        eprintln!("cli: {}", cli.display());
    }

    fn prof(slug: Option<&str>) -> Profile {
        Profile {
            dir: PathBuf::from(match slug {
                Some(s) => format!("/home/u/.claude-{s}"),
                None => "/home/u/.claude".to_string(),
            }),
            slug: slug.map(String::from),
        }
    }

    fn win(id: &str) -> LimitWindow {
        LimitWindow { id: id.into(), label: "Current session".into(), used: 0.5, ..Default::default() }
    }

    #[test]
    fn one_account_reads_exactly_as_before() {
        let w = decorate(vec![win("session")], &prof(None), None);
        assert_eq!(w[0].id, "session", "the only account keeps its ids");
        assert_eq!(w[0].group, None, "and stays ungrouped, so its card is the card that shipped");
    }

    #[test]
    fn a_second_account_is_suffixed_and_grouped() {
        let w = decorate(vec![win("session")], &prof(Some("work")), Some("work · pro"));
        assert_eq!(w[0].id, "session@work", "so by_id(\"session\") still means the default account");
        assert_eq!(w[0].group.as_deref(), Some("work · pro"));
    }

    #[test]
    fn the_group_pairs_the_name_with_the_plan() {
        assert_eq!(prof(None).group(Some("max")), "default · max");
        assert_eq!(prof(Some("work")).group(None), "work");
        assert_eq!(prof(Some("work")).group(Some("")), "work", "an empty plan adds no separator");
    }

    #[test]
    fn persisted_windows_go_back_to_the_account_that_made_them() {
        let order = vec![prof(None), prof(Some("work"))];
        let snap = UsageSnapshot {
            windows: vec![win("session"), win("session@work"), win("weekly@gone")],
            ..Default::default()
        };
        let split = split_persisted(&snap, &order);
        assert_eq!(split[&key(&order[0])].len(), 1);
        assert_eq!(split[&key(&order[1])][0].id, "session@work");
        assert_eq!(split.len(), 2, "windows from an account that is gone are dropped");
    }

    #[test]
    fn status_is_the_best_news_any_account_has() {
        let order = vec![prof(None), prof(Some("work"))];
        let mut accounts: HashMap<String, Account> = HashMap::new();
        accounts.insert(
            key(&order[0]),
            Account { status: "ok".into(), windows: vec![win("session")], fetched_at: 10, ..Default::default() },
        );
        accounts.insert(
            key(&order[1]),
            Account {
                status: "needsAuth".into(),
                note: "No Claude Code credential found".into(),
                ..Default::default()
            },
        );
        let snap = aggregate(&order, &accounts);
        assert_eq!(snap.status, "ok", "a signed-out second account must not dim the first");
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.fetched_at, 10);
        assert!(snap.note.starts_with("work: "), "the note names the account: {}", snap.note);
    }

    #[test]
    fn the_soonest_back_off_is_the_one_waited_out() {
        let order = vec![prof(None), prof(Some("work"))];
        let mut accounts: HashMap<String, Account> = HashMap::new();
        accounts.insert(key(&order[0]), Account { backoff_until: 900, ..Default::default() });
        accounts.insert(key(&order[1]), Account { backoff_until: 300, ..Default::default() });
        assert_eq!(aggregate(&order, &accounts).backoff_until, 300);
    }

    #[test]
    fn the_default_account_is_first_and_always_listed() {
        // Discovery reads the real home, so this asserts only what holds on any machine
        let list = profiles();
        assert!(!list.is_empty(), "the default account is listed even with no credential");
        assert_eq!(list[0].slug, None, "and comes first, so it owns the notch");
        let slugs: Vec<Option<String>> = list.iter().skip(1).map(|p| p.slug.clone()).collect();
        let mut sorted = slugs.clone();
        sorted.sort();
        assert_eq!(slugs, sorted, "secondary accounts are listed in name order");
        assert!(list.iter().skip(1).all(|p| p.slug.is_some()), "only the default account has no slug");
    }

    #[test]
    fn expired_is_judged_against_now() {
        let c = Credential { token: "t".into(), expires_at: Some(EXP), ..Default::default() };
        assert!(c.expired(EXP));
        assert!(!c.expired(EXP - 1));
        assert!(!Credential { token: "t".into(), expires_at: None, ..Default::default() }.expired(EXP));
    }

    #[test]
    fn plans_are_cased_as_the_mac_writes_them() {
        assert_eq!(plan_name(Some("max")).as_deref(), Some("Max"));
        assert_eq!(plan_name(Some("Pro+")).as_deref(), Some("Pro+"), "an already-cased name is left alone");
        assert_eq!(plan_name(Some("pro_plus")).as_deref(), Some("pro_plus"), "only an all-lowercase word gets a capital");
        assert_eq!(plan_name(Some("  ")), None);
        assert_eq!(plan_name(None), None);
    }

    #[test]
    fn one_account_names_its_plan_and_several_do_not() {
        let one = vec![prof(None)];
        let mut accounts: HashMap<String, Account> = HashMap::new();
        accounts.insert(key(&one[0]), Account { status: "ok".into(), plan: Some("max".into()), ..Default::default() });
        assert_eq!(aggregate(&one, &accounts).plan.as_deref(), Some("Max"));
        let two = vec![prof(None), prof(Some("work"))];
        accounts.insert(key(&two[1]), Account { status: "ok".into(), plan: Some("pro".into()), ..Default::default() });
        assert_eq!(aggregate(&two, &accounts).plan, None, "each cell's heading already carries its plan");
    }

    #[test]
    fn windows_carry_their_length() {
        let v = serde_json::json!({
            "limits": [
                {"kind": "session", "percent": 10, "resets_at": "2026-10-06T12:00:00Z"},
                {"kind": "weekly_all", "percent": 20, "resets_at": "2026-10-10T12:00:00Z"},
                {"kind": "something_new", "percent": 30, "resets_at": "2026-10-10T12:00:00Z"}
            ]
        });
        let ws = parse_response(&v);
        let d = |id: &str| ws.iter().find(|w| w.id == id).and_then(|w| w.duration);
        assert_eq!(d("session"), Some(5 * 3600));
        assert_eq!(d("weekly_all"), Some(7 * 86400));
        assert_eq!(d("something_new"), None, "an unknown kind gets no invented length, so no pace");
    }

    #[test]
    fn spend_is_read_from_minor_units_and_sorted_last() {
        let v = serde_json::json!({
            "five_hour": {"utilization": 5, "resets_at": "2026-10-06T12:00:00Z"},
            "spend": {
                "enabled": true,
                "percent": 1,
                "used": {"amount_minor": 297, "currency": "USD", "exponent": 2},
                "limit": {"amount_minor": 20000, "currency": "USD", "exponent": 2}
            }
        });
        let ws = parse_response(&v);
        assert_eq!(ws.first().map(|w| w.id.as_str()), Some("session"));
        let spend = ws.last().unwrap();
        assert_eq!(spend.id, "spend");
        assert!((spend.used - 0.01485).abs() < 1e-9, "the amounts divided, not the rounded percent");
        assert_eq!(spend.resets_at, None);
        let m = spend.money.as_ref().unwrap();
        assert_eq!(m.currency, "USD");
        assert!((m.spent - 2.97).abs() < 1e-9);
        assert!((m.remaining - 197.03).abs() < 1e-9);
    }

    #[test]
    fn a_seat_without_spending_draws_no_balance() {
        let disabled = serde_json::json!({"spend": {"enabled": false,
            "used": {"amount_minor": 0}, "limit": {"amount_minor": 0}}});
        assert!(parse_response(&disabled).is_empty());
        let zero_limit = serde_json::json!({"spend": {"used": {"amount_minor": 0}, "limit": {"amount_minor": 0}}});
        assert!(parse_response(&zero_limit).is_empty(), "0 of 0 would be an invention");
        let malformed = serde_json::json!({"spend": "on"});
        assert!(parse_response(&malformed).is_empty());
    }

    fn ember(eligible: bool, reason: Option<&str>, grants: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"cedar_ember": {"eligible": eligible, "ineligible_reason": reason, "grants": grants}})
    }

    #[test]
    fn claude_resets_count_only_live_grants() {
        let now = 1_791_300_000_000; // 2026-10-06
        let v = ember(true, None, serde_json::json!([
            {"id": "a", "resets_left": 2, "starts_at": "2026-10-01T00:00:00Z", "ends_at": "2026-10-20T00:00:00Z", "paused": false},
            {"id": "paused", "resets_left": 5, "starts_at": "2026-10-01T00:00:00Z", "ends_at": "2026-10-20T00:00:00Z", "paused": true},
            {"id": "ended", "resets_left": 1, "starts_at": "2026-09-01T00:00:00Z", "ends_at": "2026-10-01T00:00:00Z", "paused": false},
            {"id": "used", "resets_left": 0, "starts_at": "2026-10-01T00:00:00Z", "ends_at": "2026-10-20T00:00:00Z", "paused": false},
            {"id": "broken"}
        ]));
        let r = claude_reset_credits(&v, now).unwrap();
        assert_eq!(r.available_count, 2);
        assert_eq!(r.credits.len(), 1);
        assert_eq!(r.credits[0].count, 2);
        assert_eq!(r.credits[0].expires_at, parse_reset(&serde_json::json!("2026-10-20T00:00:00Z")));
    }

    #[test]
    fn a_refused_surface_is_unknown_not_none_left() {
        let now = 1_791_300_000_000;
        assert_eq!(claude_reset_credits(&ember(false, Some("surface"), serde_json::json!([])), now), None);
        assert_eq!(claude_reset_credits(&serde_json::json!({"cedar_ember": null}), now), None);
        assert_eq!(claude_reset_credits(&serde_json::json!({}), now), None);
        let other = claude_reset_credits(&ember(false, Some("plan"), serde_json::json!([])), now).unwrap();
        assert_eq!(other.available_count, 0, "ineligible for any other reason is a real none");
    }
}
