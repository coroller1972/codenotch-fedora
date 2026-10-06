//! The Activity window's data, as upstream's TimelinePane loads it: every session of a range, per
//! account, with its project, its span, its turns and tokens, and what it is estimated to have cost.
//!
//! Plan-billed money follows upstream's CostEstimator: a session in a weekly period seen by
//! Codenotch is worth a week of the plan (monthly ÷ 4.35) × the share of the allowance that period
//! used, split over the period's work by token weight. A session outside every period has no price
//! rather than a borrowed one. API-billed sessions are priced turn by turn.

use super::{accounts, choices, monthly_price, pricing::Pricing, store::Window, Account, Choice, Store, PRICING, WEEKS_PER_MONTH};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::AppHandle;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub session_id: String,
    pub account_id: String,
    pub project: String,
    pub name: String,
    pub model: String,
    /// Unix seconds
    pub first: i64,
    pub last: i64,
    pub turns: i64,
    pub tokens: i64,
    pub cost: Option<f64>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountName {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Data {
    pub currency: String,
    pub accounts: Vec<AccountName>,
    pub rows: Vec<Row>,
}

pub fn account_name(id: &str) -> String {
    match id.split_once('@') {
        _ if id == "codex" => "Codex".into(),
        Some((_, slug)) => format!("Claude ({slug})"),
        None => "Claude".into(),
    }
}

/// A plan-billed session's money: its share of the period's work, of what that period cost
fn plan_cost(periods: &[(i64, i64, f64, f64)], monthly: f64, at: i64, weight: f64) -> Option<f64> {
    let (_, _, used, period_weight) = periods.iter().find(|p| at >= p.0 && at < p.1)?;
    if *period_weight <= 0.0 || monthly <= 0.0 {
        return None;
    }
    Some(monthly / WEEKS_PER_MONTH * used / 100.0 * weight / period_weight)
}

#[tauri::command]
pub fn get_activity_timeline(app: AppHandle, from: i64, to: i64) -> Data {
    let pricing = PRICING.lock().unwrap().clone();
    timeline(from, to, pricing, &|a, choice, p| monthly_price(&app, a, choice, p).0)
}

/// The rows of [from, to] for every account; `monthly` says what a plan-billed account pays a month
fn timeline(from: i64, to: i64, pricing: Option<Pricing>, monthly: &dyn Fn(&Account, &Choice, &Pricing) -> Option<f64>) -> Data {
    let now = chrono::Utc::now().timestamp();
    let list: Vec<_> = accounts().into_iter().filter(|a| a.db_path().exists()).collect();
    let map = choices();
    let currency = pricing.as_ref().map(|p| p.currency.clone()).unwrap_or_else(|| "USD".into());
    let mut rows = Vec::new();
    for a in &list {
        let Some(store) = Store::open(&a.db_path()) else { continue };
        let choice = map.get(&a.id).cloned().unwrap_or_default();
        let price = |model: &str, t: [i64; 4]| pricing.as_ref().and_then(|p| p.api_cost(model, t));
        let sessions = store.sessions(from, to, &price);
        let monthly = pricing.as_ref().and_then(|p| monthly(a, &choice, p)).unwrap_or(0.0);
        let periods: Vec<(i64, i64, f64, f64)> = store
            .periods(Window::Weekly)
            .into_iter()
            .map(|(start, end, used)| (start, end, used, store.weights(start, end.min(now)).1))
            .collect();
        for s in sessions {
            let cost = if choice.is_api() { s.api_cost } else { plan_cost(&periods, monthly, s.first, s.weight) };
            rows.push(Row {
                title: title(a.provider, &s.session_id, &s.cwd),
                name: super::short_name(&s.project),
                session_id: s.session_id,
                account_id: a.id.clone(),
                project: s.project,
                model: s.model,
                first: s.first,
                last: s.last,
                turns: s.turns,
                tokens: s.tokens,
                cost,
            });
        }
    }
    rows.sort_by_key(|r| r.first);
    Data {
        currency,
        accounts: list.iter().map(|a| AccountName { id: a.id.clone(), name: account_name(&a.id) }).collect(),
        rows,
    }
}

static TITLES: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

/// A session's title, as a person would name it: Claude's first request from its transcript, or
/// the title Codex gave the thread. Read once per session, locally, and only shown in this window
fn title(provider: &str, sid: &str, cwd: &str) -> Option<String> {
    if let Some(hit) = TITLES.lock().unwrap().get_or_insert_with(HashMap::new).get(sid) {
        return hit.clone();
    }
    let found = if provider == "codex" { codex_title(sid) } else { claude_title(sid, cwd) };
    TITLES.lock().unwrap().get_or_insert_with(HashMap::new).insert(sid.to_string(), found.clone());
    found
}

fn claude_title(sid: &str, cwd: &str) -> Option<String> {
    let folder: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let roots: Vec<PathBuf> = crate::usage::cost_accounts().into_iter().map(|(_, dir)| dir.join("projects")).collect();
    for root in roots {
        let Ok(f) = std::fs::File::open(root.join(&folder).join(format!("{sid}.jsonl"))) else { continue };
        let mut head = String::new();
        let _ = f.take(256 * 1024).read_to_string(&mut head);
        for line in head.lines().filter(|l| l.contains("\"type\":\"user\"")) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let content = v.pointer("/message/content");
            let text = match content {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Array(parts)) => parts
                    .iter()
                    .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => continue,
            };
            if let Some(name) = crate::activity::readable_name(&text) {
                return Some(name);
            }
        }
    }
    None
}

fn codex_title(sid: &str) -> Option<String> {
    let home = crate::codex::codex_home()?;
    let mut dbs: Vec<PathBuf> = std::fs::read_dir(&home)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("state_") && n.to_string_lossy().ends_with(".sqlite")))
        .collect();
    dbs.sort();
    let db = rusqlite::Connection::open_with_flags(dbs.last()?, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = db.busy_timeout(std::time::Duration::from_millis(50));
    let (title, first): (String, String) = db
        .query_row(
            "SELECT COALESCE(title,''), COALESCE(first_user_message,'') FROM threads WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    crate::activity::readable_name(&title).or_else(|| crate::activity::readable_name(&first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_worth_its_share_of_its_period() {
        // A week that used 43.5 % of the allowance on a 100/month plan cost 10; a session holding a
        // quarter of that week's work cost 2.5
        let periods = [(0, 1000, 43.5, 400.0)];
        let c = plan_cost(&periods, 100.0, 10, 100.0).unwrap();
        assert!((c - 100.0 / WEEKS_PER_MONTH * 0.435 / 4.0).abs() < 1e-9);
        assert_eq!(plan_cost(&periods, 100.0, 2000, 100.0), None, "outside every period: no borrowed price");
        assert_eq!(plan_cost(&periods, 0.0, 10, 100.0), None);
    }

    #[test]
    fn accounts_are_named_as_the_card_names_them() {
        assert_eq!(account_name("claude"), "Claude");
        assert_eq!(account_name("claude@work"), "Claude (work)");
        assert_eq!(account_name("codex"), "Codex");
    }

    #[test]
    #[ignore = "Reads this machine's real cost databases and transcripts; writes the week to CODENOTCH_ACTIVITY_OUT"]
    fn live_timeline() {
        let pricing = Pricing::load(&super::super::settings_dir());
        let tier = |a: &Account, p: &Pricing| {
            if a.provider == "codex" { Some("pro".to_string()) } else { crate::usage::plan_tier(&a.config_dir, |t| p.plans.contains_key(t)) }
        };
        let now = chrono::Utc::now().timestamp();
        let data = timeline(now - 7 * 86400, now, Some(pricing), &|a, c, p| {
            if c.monthly_price > 0.0 { Some(c.monthly_price) } else { tier(a, p).and_then(|t| p.monthly(&t)) }
        });
        eprintln!("{} sessions; priced {}", data.rows.len(), data.rows.iter().filter(|r| r.cost.is_some()).count());
        if let Ok(out) = std::env::var("CODENOTCH_ACTIVITY_OUT") {
            std::fs::write(out, serde_json::to_string(&data).unwrap()).unwrap();
        }
    }
}
