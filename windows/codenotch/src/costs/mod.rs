//! Cost per project, ported from upstream's Costs layer.
//!
//! The limit only says how much is gone. Each rise of a usage window is paired with the local turns
//! of the same interval, so the card can say which project spent it, and what that share is worth.
//! Everything comes from files Claude Code and Codex already write, and from the readings the app
//! already takes: no extra request about usage, and nothing leaves the machine.
//!
//! One SQLite database per account (Claude's, each `~/.claude-<slug>`, and Codex's). A background
//! thread indexes the transcripts and records every new usage reading; the card asks for a view.

mod indexer;
mod pricing;
mod store;

use crate::AppState;
use indexer::{Format, Indexer};
use pricing::Pricing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use store::{Store, Window};
use tauri::{AppHandle, Emitter, Manager};

const TICK_SECS: u64 = 60;
/// A week of a plan is its monthly price ÷ 4.35
const WEEKS_PER_MONTH: f64 = 365.25 / 12.0 / 7.0;
/// Rows the card lists before the rest are merged into one
const ROWS: usize = 4;

static PRICING: Mutex<Option<Pricing>> = Mutex::new(None);
static WAKE: AtomicBool = AtomicBool::new(false);

/// One login as the cost layer sees it
#[derive(Debug, Clone)]
struct Account {
    /// The card's cell id: "claude", "claude@work", "codex"
    id: String,
    provider: &'static str,
    config_dir: PathBuf,
}

impl Account {
    fn transcripts(&self) -> PathBuf {
        self.config_dir.join(if self.provider == "codex" { "sessions" } else { "projects" })
    }
    fn format(&self) -> Format {
        if self.provider == "codex" { Format::Codex } else { Format::Claude }
    }
    fn db_path(&self) -> PathBuf {
        let safe: String = self.id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        data_dir().join(format!("agentcost-{safe}.sqlite"))
    }
}

fn accounts() -> Vec<Account> {
    let mut out: Vec<Account> = crate::usage::cost_accounts()
        .into_iter()
        .map(|(id, dir)| Account { id, provider: "claude", config_dir: dir })
        .collect();
    if let Some(home) = crate::codex::codex_home() {
        out.push(Account { id: "codex".into(), provider: "codex", config_dir: home });
    }
    out
}

/// Databases: machine-local data, not settings
fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("codenotch").join("costs")
}

/// Plans, prices and per-account choices: settings the user may edit
fn settings_dir() -> PathBuf {
    crate::config::config_path().with_file_name("costs")
}

// ---------------- Per-account choices ----------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Choice {
    /// "subscription" (default) or "api"
    #[serde(default)]
    billing: String,
    /// What the user actually pays a month, in the local currency; 0 = the catalog's list price
    #[serde(default)]
    monthly_price: f64,
}

impl Choice {
    fn is_api(&self) -> bool {
        self.billing == "api"
    }
}

fn choices() -> HashMap<String, Choice> {
    std::fs::read_to_string(settings_dir().join("accounts.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_choices(map: &HashMap<String, Choice>) {
    let dir = settings_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(t) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(dir.join("accounts.json"), t);
    }
}

// ---------------- The background pass ----------------

struct Live {
    store: Store,
    indexer: Indexer,
    /// The reading time of the last usage snapshot sampled, so one reading is recorded once
    sampled_at: u64,
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        crate::activity::lower_thread_priority();
        *PRICING.lock().unwrap() = Some(Pricing::load(&settings_dir()));
        let mut live: HashMap<String, Live> = HashMap::new();
        loop {
            let list = accounts();
            let mut changed = false;
            for a in &list {
                if !live.contains_key(&a.id) {
                    let indexer = Indexer::new(a.transcripts(), a.format());
                    if !indexer.exists() {
                        continue;
                    }
                    let Some(store) = Store::open(&a.db_path()) else {
                        crate::applog(&format!("costs: could not open the database for {}", a.id));
                        continue;
                    };
                    live.insert(a.id.clone(), Live { store, indexer, sampled_at: 0 });
                }
                let l = live.get_mut(&a.id).unwrap();
                let t = std::time::Instant::now();
                let written = l.indexer.scan(&mut l.store);
                if written > 0 {
                    crate::applog(&format!("costs: {} new turns for {} in {} ms", written, a.id, t.elapsed().as_millis()));
                    changed = true;
                }
                if let Some(snap) = snapshot_for(&app, a) {
                    if snap.status == "ok" && snap.fetched_at > l.sampled_at {
                        l.sampled_at = snap.fetched_at;
                        for (w, pct, resets) in samples(a, &snap) {
                            l.store.record_sample(w, pct, resets, (snap.fetched_at / 1000) as i64);
                        }
                        changed = true;
                    }
                }
            }
            live.retain(|id, _| list.iter().any(|a| &a.id == id));
            let want_prices = choices().values().any(Choice::is_api);
            // On a copy: the fetch can take seconds, and the card must not wait on it
            let copy = PRICING.lock().unwrap().clone();
            if let Some(mut p) = copy {
                p.refresh_if_due(chrono::Utc::now().timestamp(), want_prices);
                *PRICING.lock().unwrap() = Some(p);
            }
            if changed {
                let _ = app.emit("costs", ());
            }
            for _ in 0..TICK_SECS {
                if WAKE.swap(false, Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}

/// The account's own reading, with ids as they would be on their own ("session", not "session@work")
fn snapshot_for(app: &AppHandle, a: &Account) -> Option<crate::usage::UsageSnapshot> {
    let st = app.state::<AppState>();
    if a.provider == "codex" {
        return Some(st.codex.lock().unwrap().clone());
    }
    let mut snap = st.usage.lock().unwrap().clone();
    let suffix = a.id.strip_prefix("claude").unwrap_or("").to_string(); // "" or "@work"
    snap.windows.retain(|w| match suffix.is_empty() {
        true => !w.id.contains('@'),
        false => w.id.ends_with(&suffix),
    });
    for w in &mut snap.windows {
        if let Some(i) = w.id.find('@') {
            w.id.truncate(i);
        }
    }
    Some(snap)
}

/// The windows that measure the allowance: the rolling session and the all-models week. Codex
/// names them by position, and on some plans its primary window is the week, so its length decides
fn samples(a: &Account, snap: &crate::usage::UsageSnapshot) -> Vec<(Window, f64, Option<i64>)> {
    snap.windows
        .iter()
        .filter(|w| w.count.is_none() && w.money.is_none())
        .filter_map(|w| {
            let window = match (a.provider, w.duration, w.id.as_str()) {
                ("codex", Some(18000), _) => Window::Session,
                ("codex", Some(604800), _) => Window::Weekly,
                ("codex", _, _) => return None,
                (_, _, "session" | "five_hour") => Window::Session,
                (_, _, "seven_day" | "weekly_all" | "weekly") => Window::Weekly,
                _ => return None,
            };
            Some((window, w.used * 100.0, w.resets_at.map(|ms| (ms / 1000) as i64)))
        })
        .collect()
}

// ---------------- The card's view ----------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    /// What the card writes: the project's folder, kept with its parent when the folder alone
    /// says nothing ("nodes/src")
    pub name: String,
    pub path: String,
    pub pct: f64,
    pub cost: Option<f64>,
    pub unexplained: bool,
    /// > 0 for the row that merges the rest
    pub merged: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// "unavailable" (no transcripts), "waiting" (nothing measured yet) or "ready"
    pub state: &'static str,
    /// The range shown, which may differ from the one asked: a week needs limit readings
    pub range: String,
    /// True when percentages are a share of the work, not of the allowance
    pub share: bool,
    pub quota_backed: bool,
    pub currency: String,
    pub rows: Vec<Row>,
    /// Tokens per day from the transcripts: the chart a Claude card shows as Codex's does
    pub token_usage: Option<crate::usage::TokenUsage>,
}

fn local_midnight(days_back: i64) -> i64 {
    use chrono::{Duration as D, Local, TimeZone};
    let d = Local::now().date_naive() - D::days(days_back);
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

fn month_start() -> i64 {
    use chrono::{Datelike, Local, TimeZone};
    let today = Local::now().date_naive();
    let first = today.with_day(1).unwrap_or(today);
    Local.from_local_datetime(&first.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

/// What the account pays a month in the local currency, and its plan's catalog key
fn monthly_price(app: &AppHandle, a: &Account, choice: &Choice, pricing: &Pricing) -> (Option<f64>, Option<String>) {
    let tier = plan_tier(app, a, pricing);
    if choice.monthly_price > 0.0 {
        return (Some(choice.monthly_price), tier);
    }
    (tier.as_deref().and_then(|t| pricing.monthly(t)), tier)
}

fn plan_tier(app: &AppHandle, a: &Account, pricing: &Pricing) -> Option<String> {
    if a.provider == "codex" {
        let st = app.state::<AppState>();
        let plan = st.codex.lock().unwrap().plan.clone()?;
        return Some(plan.trim().to_lowercase());
    }
    crate::usage::plan_tier(&a.config_dir, |t| pricing.plans.contains_key(t))
}

fn compute(app: &AppHandle, id: &str, range: &str) -> View {
    let guard = PRICING.lock().unwrap();
    let currency = guard.as_ref().map(|p| p.currency.clone()).unwrap_or_else(|| "USD".into());
    let store = accounts()
        .into_iter()
        .find(|a| a.id == id)
        .filter(|a| a.db_path().exists())
        .and_then(|a| Some((Store::open(&a.db_path())?, a)));
    let Some((store, a)) = store else {
        return View { state: "unavailable", range: range.into(), share: range != "week", quota_backed: false, currency, rows: Vec::new(), token_usage: None };
    };
    let choice = choices().get(&a.id).cloned().unwrap_or_default();
    let monthly = guard.as_ref().and_then(|p| monthly_price(app, &a, &choice, p).0);
    view(&store, range, choice.is_api(), monthly, guard.as_ref(), currency, chrono::Utc::now().timestamp())
}

/// The card's view of one account, from its database alone: which projects, what share, what it
/// is worth. `monthly` is what the plan costs a month (None: no money for a plan-billed account)
fn view(store: &Store, range: &str, api: bool, monthly: Option<f64>, pricing: Option<&Pricing>, currency: String, now: i64) -> View {
    let quota_backed = store.has_samples(Window::Weekly);
    let range = if range == "week" && !quota_backed { "month" } else { range };
    let (rows, from) = match range {
        "week" => {
            let start = store.period_start(Window::Weekly);
            (store.current_period(Window::Weekly), if start > 0 { start } else { now - Window::Weekly.secs() })
        }
        "today" => {
            let from = local_midnight(0);
            (store.share(from, now), from)
        }
        _ => {
            let from = month_start();
            (store.share(from, now), from)
        }
    };
    let mut per_point: Option<f64> = None;
    let costs: HashMap<String, f64> = if api {
        let mut by: HashMap<String, f64> = HashMap::new();
        if let Some(pricing) = pricing {
            for (project, model, t) in store.turns(from, now) {
                if let Some(c) = pricing.api_cost(&model, t) {
                    *by.entry(project).or_default() += c;
                }
            }
        }
        by
    } else {
        match monthly.filter(|m| *m > 0.0) {
            None => HashMap::new(),
            Some(monthly) if quota_backed => {
                // A project that used 4 % of the weekly allowance spent 4 % of a week of the plan
                let weekly = monthly / WEEKS_PER_MONTH;
                if range == "week" {
                    per_point = Some(weekly / 100.0);
                }
                store.attributed_pct(Window::Weekly, from, now).into_iter().map(|(p, pct)| (p, weekly * pct / 100.0)).collect()
            }
            Some(monthly) => {
                // No rolling limit to anchor on: the month's price over the month's work
                let (_, month_total) = store.weights(month_start() - 1, now);
                let (by, _) = store.weights(from - 1, now);
                if month_total > 0.0 { by.into_iter().map(|(p, w)| (p, monthly * w / month_total)).collect() } else { HashMap::new() }
            }
        }
    };
    let priced: Vec<(String, f64, Option<f64>)> = rows
        .into_iter()
        .map(|(p, pct)| {
            let cost = if p == store::UNEXPLAINED { None } else { per_point.map(|v| v * pct).or_else(|| costs.get(&p).copied()) };
            (p, pct, cost)
        })
        .collect();
    let rows = presentable(priced);
    View {
        state: if rows.is_empty() { "waiting" } else { "ready" },
        range: range.to_string(),
        share: range != "week",
        quota_backed,
        currency,
        rows,
        token_usage: token_usage(&store.daily_tokens(), &chrono::Local::now().date_naive()),
    }
}

/// The top rows by name, the rest merged into one, what nothing local explains always last
fn presentable(rows: Vec<(String, f64, Option<f64>)>) -> Vec<Row> {
    let unexplained: f64 = rows.iter().filter(|r| r.0 == store::UNEXPLAINED).map(|r| r.1).sum();
    let known: Vec<_> = rows.into_iter().filter(|r| r.0 != store::UNEXPLAINED && r.1 > 0.0001).collect();
    let mut out: Vec<Row> = known
        .iter()
        .take(ROWS)
        .map(|(p, pct, cost)| Row { name: short_name(p), path: p.clone(), pct: *pct, cost: *cost, unexplained: false, merged: 0 })
        .collect();
    let rest = &known[known.len().min(ROWS)..];
    if !rest.is_empty() {
        let costs: Vec<f64> = rest.iter().filter_map(|r| r.2).collect();
        out.push(Row {
            name: String::new(),
            path: store::OTHER.into(),
            pct: rest.iter().map(|r| r.1).sum(),
            cost: (!costs.is_empty()).then(|| costs.iter().sum()),
            unexplained: false,
            merged: rest.len(),
        });
    }
    if unexplained > 0.0001 {
        out.push(Row { name: String::new(), path: store::UNEXPLAINED.into(), pct: unexplained, cost: None, unexplained: true, merged: 0 });
    }
    out
}

/// Folder names that say nothing alone keep their parent ("nodes/src", not "src")
fn short_name(path: &str) -> String {
    const GENERIC: [&str; 39] = [
        "src", "source", "sources", "lib", "libs", "app", "apps", "core", "common", "shared", "packages", "package",
        "modules", "components", "dist", "build", "out", "bin", "scripts", "assets", "public", "static", "docs", "doc",
        "test", "tests", "spec", "api", "server", "client", "web", "windows", "macos", "ios", "android", "main", "index",
        "utils", "util",
    ];
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => path.to_string(),
        [.., parent, last] if GENERIC.contains(&last.to_lowercase().as_str()) => format!("{parent}/{last}"),
        [.., last] => last.to_string(),
    }
}

/// The per-day chart and its summary, in the shape Codex's server publishes, with streaks over the
/// calendar days that had any use
fn token_usage(days: &[(String, i64)], today: &chrono::NaiveDate) -> Option<crate::usage::TokenUsage> {
    if days.is_empty() {
        return None;
    }
    let dates: Vec<chrono::NaiveDate> = days.iter().filter_map(|(d, _)| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).collect();
    let (mut longest, mut run) = (0u64, 0u64);
    let mut prev: Option<chrono::NaiveDate> = None;
    for d in &dates {
        run = if prev.and_then(|p| p.succ_opt()) == Some(*d) { run + 1 } else { 1 };
        longest = longest.max(run);
        prev = Some(*d);
    }
    let current = match dates.last() {
        Some(last) if *last == *today || last.succ_opt() == Some(*today) => run,
        _ => 0,
    };
    Some(crate::usage::TokenUsage {
        lifetime_tokens: Some(days.iter().map(|d| d.1.max(0) as u64).sum()),
        peak_daily_tokens: days.iter().map(|d| d.1.max(0) as u64).max(),
        longest_turn_secs: None,
        current_streak_days: Some(current),
        longest_streak_days: Some(longest),
        daily: days
            .iter()
            .map(|(d, t)| crate::usage::DailyTokens { date: d.clone(), tokens: (*t).max(0) as u64 })
            .collect(),
    })
}

// ---------------- Commands ----------------

#[tauri::command]
pub fn get_costs(app: AppHandle, id: String, range: String) -> View {
    compute(&app, &id, &range)
}

#[derive(Debug, Serialize)]
pub struct AccountSettings {
    id: String,
    billing: String,
    monthly_price: f64,
    plan: Option<String>,
    /// The catalog's monthly price in the local currency, shown as the field's placeholder
    list_price: Option<f64>,
    indexed: bool,
}

#[derive(Debug, Serialize)]
pub struct Settings {
    currency: String,
    rate: Option<f64>,
    models: usize,
    plans_file: String,
    accounts: Vec<AccountSettings>,
}

#[tauri::command]
pub fn get_cost_settings(app: AppHandle) -> Settings {
    let map = choices();
    let guard = PRICING.lock().unwrap();
    let fallback;
    let pricing = match guard.as_ref() {
        Some(p) => p,
        None => {
            fallback = Pricing::load(&settings_dir());
            &fallback
        }
    };
    let accounts = accounts()
        .into_iter()
        .filter(|a| a.transcripts().is_dir())
        .map(|a| {
            let c = map.get(&a.id).cloned().unwrap_or_default();
            let tier = plan_tier(&app, &a, pricing);
            AccountSettings {
                billing: if c.is_api() { "api".into() } else { "subscription".into() },
                monthly_price: c.monthly_price,
                plan: tier.as_deref().map(|t| pricing.plan_name(t)),
                list_price: tier.as_deref().and_then(|t| pricing.monthly(t)),
                indexed: a.db_path().exists(),
                id: a.id,
            }
        })
        .collect();
    Settings {
        currency: pricing.currency.clone(),
        rate: pricing.rate(),
        models: pricing.model_count(),
        plans_file: pricing.plans_path().to_string_lossy().to_string(),
        accounts,
    }
}

#[tauri::command]
pub fn set_cost_billing(app: AppHandle, id: String, billing: String) {
    let mut map = choices();
    map.entry(id).or_default().billing = if billing == "api" { "api".into() } else { String::new() };
    save_choices(&map);
    WAKE.store(true, Ordering::Relaxed); // prices are fetched once an account is API-billed
    let _ = app.emit("costs", ());
}

#[tauri::command]
pub fn set_cost_price(app: AppHandle, id: String, price: f64) {
    let mut map = choices();
    map.entry(id).or_default().monthly_price = if price.is_finite() && price > 0.0 { price } else { 0.0 };
    save_choices(&map);
    let _ = app.emit("costs", ());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_folders_keep_their_parent() {
        assert_eq!(short_name("/home/u/work/codenotch"), "codenotch");
        assert_eq!(short_name("/home/u/nodes/src"), "nodes/src");
        assert_eq!(short_name("C:\\Users\\u\\app\\windows"), "app/windows");
    }

    #[test]
    fn the_list_keeps_four_names_then_merges_and_ends_with_elsewhere() {
        let rows = (0..6).map(|i| (format!("/p{i}"), 10.0 - i as f64, Some(1.0))).chain([(store::UNEXPLAINED.to_string(), 3.0, None)]).collect();
        let out = presentable(rows);
        assert_eq!(out.len(), 6);
        assert_eq!(out[4].merged, 2);
        assert!((out[4].pct - 11.0).abs() < 1e-9 && out[4].cost == Some(2.0));
        assert!(out[5].unexplained && out[5].pct == 3.0);
    }

    #[test]
    fn streaks_count_calendar_days_with_use() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let days: Vec<(String, i64)> = ["2026-09-01", "2026-09-02", "2026-09-03", "2026-10-05", "2026-10-06"]
            .iter()
            .map(|d| (d.to_string(), 10))
            .collect();
        let u = token_usage(&days, &today).unwrap();
        assert_eq!((u.current_streak_days, u.longest_streak_days), (Some(2), Some(3)));
        assert_eq!(u.lifetime_tokens, Some(50));
        let stale = token_usage(&days[..3], &today).unwrap();
        assert_eq!(stale.current_streak_days, Some(0), "a streak that ended before yesterday is over");
        assert!(token_usage(&[], &today).is_none());
    }

    #[test]
    fn codex_windows_are_told_apart_by_length() {
        let a = Account { id: "codex".into(), provider: "codex", config_dir: PathBuf::new() };
        let w = |id: &str, duration: Option<u64>| crate::usage::LimitWindow { id: id.into(), used: 0.5, duration, ..Default::default() };
        let snap = crate::usage::UsageSnapshot { windows: vec![w("primary", Some(604800)), w("spark", None)], ..Default::default() };
        let s = samples(&a, &snap);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, Window::Weekly, "a weekly primary is the week, not a session");
        let c = Account { id: "claude".into(), provider: "claude", config_dir: PathBuf::new() };
        let snap = crate::usage::UsageSnapshot { windows: vec![w("session", None), w("seven_day", None), w("weekly_scoped", None)], ..Default::default() };
        let kinds: Vec<Window> = samples(&c, &snap).into_iter().map(|s| s.0).collect();
        assert_eq!(kinds, [Window::Session, Window::Weekly]);
    }

    #[test]
    fn a_weekly_share_is_worth_that_share_of_a_week_of_the_plan() {
        let s = Store::memory();
        let now = chrono::Utc::now().timestamp();
        s.record_sample(Window::Weekly, 0.0, Some(now + 86400), now - 3600);
        s.record_sample(Window::Weekly, 4.35, Some(now + 86400), now - 60);
        let v = view(&s, "week", false, Some(100.0), None, "EUR".into(), now);
        assert_eq!(v.state, "ready");
        assert_eq!(v.rows.len(), 1);
        assert!(v.rows[0].unexplained, "no local turns: elsewhere");
        assert_eq!(v.rows[0].cost, None, "what nothing local explains is not priced");
        let none = view(&Store::memory(), "week", false, Some(100.0), None, "EUR".into(), now);
        assert_eq!((none.range.as_str(), none.state), ("month", "waiting"), "no weekly readings: the month instead");
    }

    #[test]
    #[ignore = "Reads this machine's real cost databases; opt in to look at the numbers"]
    fn live_view() {
        let pricing = Pricing::load(&settings_dir());
        for a in accounts().into_iter().filter(|a| a.db_path().exists()) {
            let store = Store::open(&a.db_path()).unwrap();
            let tier = if a.provider == "codex" { Some("pro".to_string()) } else { crate::usage::plan_tier(&a.config_dir, |t| pricing.plans.contains_key(t)) };
            let monthly = tier.as_deref().and_then(|t| pricing.monthly(t));
            for range in ["today", "week", "month"] {
                let v = view(&store, range, false, monthly, Some(&pricing), pricing.currency.clone(), chrono::Utc::now().timestamp());
                let rows: Vec<String> = v.rows.iter().map(|r| format!("{} {:.1}% {:?}", if r.unexplained { "Elsewhere" } else { &r.name }, r.pct, r.cost.map(|c| (c * 100.0).round() / 100.0))).collect();
                eprintln!("{} [{tier:?} {monthly:?}/month] {range}->{} {}: {rows:?}", a.id, v.range, v.state);
            }
        }
    }
}
