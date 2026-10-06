//! Plans, per-token prices and the exchange rate, as upstream's PlanCatalog and PriceTable.
//!
//! - plans.json: subscription plans and their monthly list prices. Copied from the app on first
//!   run into the config folder, where it can be edited; nothing about plans lives in code.
//! - prices: USD per million tokens per model family, for API-billed accounts. Bundled defaults,
//!   refreshed once a day from OpenRouter's public list, and only while an account is API-billed.
//! - rate: 1 USD in the user's currency, refreshed once a day from open.er-api.com when that
//!   currency is not USD. No account or key is involved in either; nothing about usage is sent.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PLANS_DEFAULT: &str = include_str!("../../costs/plans.json");
const PRICES_DEFAULT: &str = include_str!("../../costs/prices-default.json");
const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const FX_URL: &str = "https://open.er-api.com/v6/latest/USD";
const DAY_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub name: String,
    pub prices: HashMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    pub model: String,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// What is kept between runs: the fetched prices and rate, with when they were fetched
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    prices: Vec<ModelPrice>,
    #[serde(default)]
    rate: f64,
    #[serde(default)]
    rate_currency: String,
    #[serde(default)]
    prices_updated_at: Option<i64>,
    #[serde(default)]
    rate_updated_at: Option<i64>,
}

#[derive(Clone)]
pub struct Pricing {
    dir: PathBuf,
    pub plans: HashMap<String, Plan>,
    pub currency: String,
    saved: Saved,
}

impl Pricing {
    pub fn load(dir: &Path) -> Pricing {
        let _ = std::fs::create_dir_all(dir);
        let plans_path = dir.join("plans.json");
        if !plans_path.exists() {
            let _ = std::fs::write(&plans_path, PLANS_DEFAULT);
        }
        let plans = std::fs::read_to_string(&plans_path)
            .ok()
            .and_then(|t| parse_plans(&t))
            .or_else(|| parse_plans(PLANS_DEFAULT))
            .unwrap_or_default();
        let currency = local_currency();
        let mut saved: Saved = std::fs::read_to_string(dir.join("prices.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if saved.prices.is_empty() {
            saved.prices = parse_prices(PRICES_DEFAULT);
        }
        // A rate for another currency (the locale changed) is no rate at all
        if saved.rate_currency != currency {
            saved.rate = 0.0;
            saved.rate_updated_at = None;
        }
        if currency == "USD" {
            saved.rate = 1.0;
        }
        Pricing { dir: dir.to_path_buf(), plans, currency, saved }
    }

    fn save(&self) {
        if let Ok(t) = serde_json::to_string_pretty(&self.saved) {
            let _ = std::fs::write(self.dir.join("prices.json"), t);
        }
    }

    /// 1 USD in the local currency; None until known
    pub fn rate(&self) -> Option<f64> {
        (self.saved.rate > 0.0).then_some(self.saved.rate)
    }

    pub fn plans_path(&self) -> PathBuf {
        self.dir.join("plans.json")
    }

    pub fn model_count(&self) -> usize {
        self.saved.prices.len()
    }

    /// A plan's name, or a readable form of its tier when the catalog does not list it
    pub fn plan_name(&self, tier: &str) -> String {
        if let Some(p) = self.plans.get(tier) {
            return p.name.clone();
        }
        let raw = tier.replace("default_claude_", "").replace('_', " ");
        let mut c = raw.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }

    /// The plan's monthly list price in the local currency: listed in it, else its USD price at
    /// the day's rate. None when neither is known (an Enterprise seat lists none)
    pub fn monthly(&self, tier: &str) -> Option<f64> {
        let plan = self.plans.get(tier)?;
        if let Some(local) = plan.prices.get(&self.currency) {
            return Some(*local);
        }
        Some(plan.prices.get("USD")? * self.rate()?)
    }

    /// One turn at API prices, in the local currency. None when the model has no price or the rate
    /// is not known: a blank is better than a number in the wrong money
    pub fn api_cost(&self, model: &str, t: [i64; 4]) -> Option<f64> {
        let p = price_for(&self.saved.prices, model)?;
        let usd = (t[0] as f64 * p.input + t[1] as f64 * p.output + t[2] as f64 * p.cache_read + t[3] as f64 * p.cache_write)
            / 1_000_000.0;
        Some(usd * self.rate()?)
    }

    /// Fetch what is a day old. Blocking; called from the costs thread
    pub fn refresh_if_due(&mut self, now: i64, want_prices: bool) {
        let due = |at: Option<i64>| at.is_none_or(|t| now - t > DAY_SECS);
        let mut changed = false;
        if self.currency != "USD" && due(self.saved.rate_updated_at) {
            match fetch_json(FX_URL).and_then(|v| v.pointer(&format!("/rates/{}", self.currency)).and_then(|x| x.as_f64())) {
                Some(r) if r > 0.0 => {
                    self.saved.rate = r;
                    self.saved.rate_currency = self.currency.clone();
                    self.saved.rate_updated_at = Some(now);
                    changed = true;
                }
                _ => crate::applog(&format!("costs: no USD→{} rate from open.er-api.com", self.currency)),
            }
        }
        if want_prices && due(self.saved.prices_updated_at) {
            let fetched = fetch_json(MODELS_URL).map(|v| parse_openrouter(&v)).unwrap_or_default();
            if fetched.is_empty() {
                crate::applog("costs: no model prices from OpenRouter");
            } else {
                let mut by: HashMap<String, ModelPrice> = self.saved.prices.drain(..).map(|p| (p.model.clone(), p)).collect();
                for p in fetched.into_iter().filter(|p| p.input > 0.0 || p.output > 0.0) {
                    by.insert(p.model.clone(), p);
                }
                let mut list: Vec<ModelPrice> = by.into_values().collect();
                list.sort_by(|a, b| a.model.cmp(&b.model));
                self.saved.prices = list;
                self.saved.prices_updated_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }
}

fn fetch_json(url: &str) -> Option<serde_json::Value> {
    ureq::get(url)
        .set("User-Agent", concat!("codenotch/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .call()
        .ok()?
        .into_json()
        .ok()
}

fn parse_plans(text: &str) -> Option<HashMap<String, Plan>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let list = v.get("plans")?.as_object()?;
    Some(
        list.iter()
            .map(|(tier, d)| {
                let prices = d
                    .get("prices")
                    .and_then(|x| x.as_object())
                    .map(|m| m.iter().filter_map(|(c, n)| Some((c.clone(), n.as_f64()?))).collect())
                    .unwrap_or_default();
                let name = d.get("name").and_then(|x| x.as_str()).unwrap_or(tier).to_string();
                (tier.clone(), Plan { name, prices })
            })
            .collect(),
    )
}

fn parse_prices(text: &str) -> Vec<ModelPrice> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("prices")?.clone()).ok())
        .unwrap_or_default()
}

/// OpenRouter lists USD per token; keep Anthropic's and OpenAI's models as USD per million, named
/// as the CLIs name them ("anthropic/claude-fable-5.1" → "claude-fable-5-1")
fn parse_openrouter(v: &serde_json::Value) -> Vec<ModelPrice> {
    let Some(list) = v.get("data").and_then(|x| x.as_array()) else { return Vec::new() };
    list.iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            if id.contains(':') || !(id.starts_with("anthropic/") || id.starts_with("openai/")) {
                return None;
            }
            let pr = m.get("pricing")?;
            let num = |k: &str| {
                pr.get(k)
                    .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64()))
                    .unwrap_or(0.0)
                    * 1e6
            };
            Some(ModelPrice {
                model: id.rsplit('/').next()?.replace('.', "-"),
                input: num("prompt"),
                output: num("completion"),
                cache_read: num("input_cache_read"),
                cache_write: num("input_cache_write"),
            })
        })
        .collect()
}

/// The longest listed model name the turn's model starts with
fn price_for<'a>(prices: &'a [ModelPrice], model: &str) -> Option<&'a ModelPrice> {
    prices.iter().filter(|p| model.starts_with(&p.model)).max_by_key(|p| p.model.len())
}

/// The user's currency, from the monetary locale's territory ("fr_FR.UTF-8" → EUR). USD when the
/// locale does not say, as the Mac falls back
pub fn local_currency() -> String {
    let raw = ["LC_ALL", "LC_MONETARY", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .unwrap_or_default();
    currency_for_locale(&raw).to_string()
}

pub fn currency_for_locale(locale: &str) -> &'static str {
    let territory = locale
        .split(['.', '@'])
        .next()
        .and_then(|l| l.split(['_', '-']).nth(1))
        .unwrap_or("")
        .to_ascii_uppercase();
    const EURO: [&str; 20] =
        ["AT", "BE", "CY", "DE", "EE", "ES", "FI", "FR", "GR", "HR", "IE", "IT", "LT", "LU", "LV", "MT", "NL", "PT", "SI", "SK"];
    if EURO.contains(&territory.as_str()) {
        return "EUR";
    }
    match territory.as_str() {
        "GB" => "GBP",
        "CH" | "LI" => "CHF",
        "CA" => "CAD",
        "AU" => "AUD",
        "NZ" => "NZD",
        "JP" => "JPY",
        "CN" => "CNY",
        "TW" => "TWD",
        "HK" => "HKD",
        "SG" => "SGD",
        "KR" => "KRW",
        "IN" => "INR",
        "BR" => "BRL",
        "MX" => "MXN",
        "RU" => "RUB",
        "UA" => "UAH",
        "PL" => "PLN",
        "CZ" => "CZK",
        "HU" => "HUF",
        "RO" => "RON",
        "SE" => "SEK",
        "NO" => "NOK",
        "DK" => "DKK",
        "TR" => "TRY",
        "IL" => "ILS",
        "ZA" => "ZAR",
        _ => "USD",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_locale_names_the_currency() {
        assert_eq!(currency_for_locale("fr_FR.UTF-8"), "EUR");
        assert_eq!(currency_for_locale("pt_BR.UTF-8"), "BRL");
        assert_eq!(currency_for_locale("en_GB"), "GBP");
        assert_eq!(currency_for_locale("de_CH.UTF-8@euro"), "CHF");
        assert_eq!(currency_for_locale("C.UTF-8"), "USD");
        assert_eq!(currency_for_locale(""), "USD");
    }

    #[test]
    fn the_bundled_catalog_and_prices_read() {
        let plans = parse_plans(PLANS_DEFAULT).unwrap();
        assert_eq!(plans["default_claude_pro"].prices["USD"], 20.0);
        assert_eq!(plans["plus"].name, "Plus");
        assert!(!parse_prices(PRICES_DEFAULT).is_empty());
    }

    #[test]
    fn a_plan_is_priced_in_the_local_currency() {
        let dir = std::env::temp_dir().join(format!("codenotch-pricing-{}", std::process::id()));
        let mut p = Pricing::load(&dir);
        p.currency = "EUR".into();
        p.saved.rate = 0.0;
        assert_eq!(p.monthly("default_claude_pro"), None, "no rate yet: no number");
        p.saved.rate = 0.9;
        assert!((p.monthly("default_claude_pro").unwrap() - 18.0).abs() < 1e-9);
        p.currency = "BRL".into();
        assert_eq!(p.monthly("default_claude_max_5x"), Some(550.0), "a listed local price wins");
        assert_eq!(p.monthly("default_claude_enterprise"), None);
        assert_eq!(p.plan_name("default_claude_max_20x"), "Max 20x");
        assert_eq!(p.plan_name("default_claude_ai"), "Ai");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn api_turns_are_priced_by_the_longest_matching_model() {
        let prices = vec![
            ModelPrice { model: "claude-sonnet-5".into(), input: 2.0, output: 10.0, cache_read: 0.2, cache_write: 2.5 },
            ModelPrice { model: "claude-sonnet".into(), input: 99.0, output: 99.0, cache_read: 99.0, cache_write: 99.0 },
        ];
        assert_eq!(price_for(&prices, "claude-sonnet-5-5-20260901").unwrap().input, 2.0);
        assert!(price_for(&prices, "gpt-6").is_none());
    }

    #[test]
    fn openrouter_names_match_the_clis() {
        let v = serde_json::json!({"data": [
            {"id": "anthropic/claude-fable-5.1", "pricing": {"prompt": "0.00001", "completion": "0.00005"}},
            {"id": "anthropic/claude-fable-5.1:thinking", "pricing": {"prompt": "1"}},
            {"id": "google/gemini", "pricing": {"prompt": "1"}}
        ]});
        let p = parse_openrouter(&v);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].model, "claude-fable-5-1");
        assert!((p[0].input - 10.0).abs() < 1e-9 && (p[0].output - 50.0).abs() < 1e-9);
    }
}
