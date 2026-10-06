//! SQLite store for token usage and limit attribution, ported from upstream's CostStore.
//!
//! - Only numbers plus cwd/branch/model are stored; no message content ever reaches it.
//! - Turns are deduplicated by key: Claude Code writes one turn several times while streaming,
//!   each write larger, so a conflict keeps the maximum of every count.
//! - A quota reading is compared with the previous one of its window. A rise is split across the
//!   turns since the last attributed point by token weight; a fall is a reset and starts a period.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

pub const UNEXPLAINED: &str = "__unexplained__";
pub const OTHER: &str = "__other__";

// Relative weights that split one rise across turns: cached input costs about a tenth of fresh
// input, output several times more. Small errors do not accumulate across intervals.
pub const K_OUTPUT: f64 = 5.0;
pub const K_CACHE_WRITE: f64 = 1.25;
pub const K_CACHE_READ: f64 = 0.1;

/// Which limit window an attribution belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Session,
    Weekly,
}

impl Window {
    pub fn key(self) -> &'static str {
        match self {
            Window::Session => "session",
            Window::Weekly => "weekly",
        }
    }
    pub fn secs(self) -> i64 {
        match self {
            Window::Session => 5 * 3600,
            Window::Weekly => 7 * 86400,
        }
    }
}

/// One assistant turn. Numbers only, never content
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Event {
    pub ts: i64,
    pub session_id: String,
    pub dedupe_key: String,
    pub project: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

pub fn weight(input: i64, output: i64, cache_read: i64, cache_write: i64) -> f64 {
    input as f64 + output as f64 * K_OUTPUT + cache_read as f64 * K_CACHE_READ + cache_write as f64 * K_CACHE_WRITE
}

/// One session's turns in a range, summed
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionAgg {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    /// The model of the session's latest turn
    pub model: String,
    pub first: i64,
    pub last: i64,
    pub turns: i64,
    pub tokens: i64,
    pub weight: f64,
    pub api_cost: Option<f64>,
}

/// Where a file was read up to
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cursor {
    pub inode: u64,
    pub size: u64,
    pub offset: u64,
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Option<Store> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Connection::open(path).ok()?;
        let s = Store { db };
        s.migrate().ok()?;
        Some(s)
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        let s = Store { db: Connection::open_in_memory().unwrap() };
        s.migrate().unwrap();
        s
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let _ = self.db.pragma_update(None, "journal_mode", "WAL");
        let _ = self.db.pragma_update(None, "synchronous", "NORMAL");
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_cursor(
               path TEXT PRIMARY KEY, inode INTEGER NOT NULL, size INTEGER NOT NULL, offset INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS usage_event(
               id INTEGER PRIMARY KEY, dedupe_key TEXT NOT NULL UNIQUE, ts INTEGER NOT NULL,
               session_id TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT NOT NULL, branch TEXT,
               model TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
               cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_event_ts ON usage_event(ts);
             CREATE TABLE IF NOT EXISTS quota_sample(
               id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, window TEXT NOT NULL, pct REAL NOT NULL, resets_at INTEGER);
             CREATE INDEX IF NOT EXISTS ix_sample_window_ts ON quota_sample(window, ts);
             CREATE TABLE IF NOT EXISTS attribution(
               id INTEGER PRIMARY KEY, t0 INTEGER NOT NULL, t1 INTEGER NOT NULL, window TEXT NOT NULL,
               project TEXT NOT NULL, delta_pct REAL NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_attr_window_t1 ON attribution(window, t1);
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS period_boundary(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, window TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS ix_boundary_window_ts ON period_boundary(window, ts);",
        )
    }

    // ---------------- Files ----------------

    pub fn cursor(&self, path: &str) -> Option<Cursor> {
        self.db
            .query_row("SELECT inode, size, offset FROM file_cursor WHERE path = ?1", [path], |r| {
                Ok(Cursor { inode: r.get::<_, i64>(0)? as u64, size: r.get::<_, i64>(1)? as u64, offset: r.get::<_, i64>(2)? as u64 })
            })
            .optional()
            .ok()
            .flatten()
    }

    /// One file's events and its new cursor in a single transaction, so an interrupted pass never
    /// leaves the cursor ahead of the rows
    pub fn commit(&mut self, events: &[Event], path: &str, cursor: Cursor) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO usage_event(dedupe_key, ts, session_id, project, cwd, branch, model, input, output, cache_read, cache_write)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(dedupe_key) DO UPDATE SET
                   ts = MAX(usage_event.ts, excluded.ts), input = MAX(usage_event.input, excluded.input),
                   output = MAX(usage_event.output, excluded.output),
                   cache_read = MAX(usage_event.cache_read, excluded.cache_read),
                   cache_write = MAX(usage_event.cache_write, excluded.cache_write),
                   model = CASE WHEN usage_event.model = 'codex' THEN excluded.model ELSE usage_event.model END",
            )?;
            for e in events {
                st.execute(params![
                    e.dedupe_key, e.ts, e.session_id, e.project, e.cwd, e.branch, e.model,
                    e.input, e.output, e.cache_read, e.cache_write
                ])?;
            }
            tx.execute(
                "INSERT INTO file_cursor(path, inode, size, offset) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(path) DO UPDATE SET inode=excluded.inode, size=excluded.size, offset=excluded.offset",
                params![path, cursor.inode as i64, cursor.size as i64, cursor.offset as i64],
            )?;
        }
        tx.commit()
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        let _ = self.db.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    /// Every file is read again from its start on the next pass; rows, readings and attributions stay
    pub fn forget_files(&self) {
        let _ = self.db.execute("DELETE FROM file_cursor", []);
    }

    pub fn event_count(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) FROM usage_event", [], |r| r.get(0)).unwrap_or(0)
    }

    // ---------------- Quota samples and attribution ----------------

    fn last_sample(&self, w: Window) -> Option<(i64, f64, Option<i64>)> {
        self.db
            .query_row(
                "SELECT ts, pct, resets_at FROM quota_sample WHERE window=?1 ORDER BY ts DESC LIMIT 1",
                [w.key()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn has_samples(&self, w: Window) -> bool {
        self.last_sample(w).is_some()
    }

    /// One reading of a window, and the attribution of any rise since the previous one
    pub fn record_sample(&self, w: Window, pct: f64, resets_at: Option<i64>, ts: i64) {
        let prev = self.last_sample(w);
        // A reading no newer than the last one says nothing new (a restart re-reads the saved one)
        if prev.is_some_and(|(t, _, _)| ts <= t) {
            return;
        }
        let _ = self.db.execute(
            "INSERT INTO quota_sample(ts, window, pct, resets_at) VALUES(?1,?2,?3,?4)",
            params![ts, w.key(), pct, resets_at],
        );
        let Some((_, prev_pct, _)) = prev else { return };
        let delta = pct - prev_pct;
        if delta < 0.0 {
            let _ = self.db.execute("INSERT INTO period_boundary(ts, window) VALUES(?1,?2)", params![ts, w.key()]);
            return;
        }
        if delta > 0.0 {
            let t0 = self.anchor(w);
            self.attribute(w, delta, t0, ts);
        }
    }

    fn max_of(&self, sql: &str, w: Window) -> Option<i64> {
        self.db.query_row(sql, [w.key()], |r| r.get::<_, Option<i64>>(0)).ok().flatten()
    }

    /// The last point already accounted for. A reading that did not move leaves it where it was:
    /// the endpoint reports whole percents, so the next rise belongs to every turn since the last
    /// attributed one. With nothing attributed, the first reading of the window: what happened
    /// before Codenotch was watching is not ours to explain
    fn anchor(&self, w: Window) -> i64 {
        let t0 = self
            .max_of("SELECT MAX(t1) FROM attribution WHERE window=?1", w)
            .unwrap_or(0)
            .max(self.max_of("SELECT MAX(ts) FROM period_boundary WHERE window=?1", w).unwrap_or(0));
        if t0 > 0 {
            return t0;
        }
        self.max_of("SELECT MIN(ts) FROM quota_sample WHERE window=?1", w).unwrap_or(0)
    }

    fn attribute(&self, w: Window, delta: f64, t0: i64, t1: i64) {
        let (by_project, total) = self.weights(t0, t1);
        let insert = |project: &str, pct: f64| {
            let _ = self.db.execute(
                "INSERT INTO attribution(t0, t1, window, project, delta_pct) VALUES(?1,?2,?3,?4,?5)",
                params![t0, t1, w.key(), project, pct],
            );
        };
        if total <= 0.0 {
            // Nothing local explains it: the web app, another machine. Shown, not hidden
            insert(UNEXPLAINED, delta);
            return;
        }
        for (project, wt) in by_project {
            insert(&project, delta * wt / total);
        }
    }

    /// Token weight per project for the turns in (from, to]
    pub fn weights(&self, from: i64, to: i64) -> (HashMap<String, f64>, f64) {
        let mut by: HashMap<String, f64> = HashMap::new();
        let mut total = 0.0;
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT project, input, output, cache_read, cache_write FROM usage_event WHERE ts > ?1 AND ts <= ?2",
        ) {
            if let Ok(rows) = st.query_map(params![from, to], |r| {
                Ok((r.get::<_, String>(0)?, weight(r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            }) {
                for (p, wt) in rows.flatten() {
                    *by.entry(p).or_default() += wt;
                    total += wt;
                }
            }
        }
        (by, total)
    }

    /// Where the current period of a window began: the vendor's reset minus the window's length
    /// when a reading carried one, else the last reset Codenotch saw
    pub fn period_start(&self, w: Window) -> i64 {
        let seen = self.max_of("SELECT MAX(ts) FROM period_boundary WHERE window=?1", w).unwrap_or(0);
        let from_reset = self.last_sample(w).and_then(|(_, _, r)| r).map(|r| r - w.secs()).unwrap_or(0);
        seen.max(from_reset)
    }

    /// Each project's share of the current period, in percent of the limit, largest first.
    /// Readings only cover the stretches Codenotch was running; the rest of what the card reports
    /// is spread over the period's turns, so the rows add up to the card's own percentage
    pub fn current_period(&self, w: Window) -> Vec<(String, f64)> {
        let start = self.period_start(w);
        let mut totals: HashMap<String, f64> = HashMap::new();
        if let Ok(mut st) = self
            .db
            .prepare_cached("SELECT project, SUM(delta_pct) FROM attribution WHERE window=?1 AND t1 > ?2 GROUP BY project")
        {
            if let Ok(rows) = st.query_map(params![w.key(), start], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
                for (p, v) in rows.flatten() {
                    *totals.entry(p).or_default() += v;
                }
            }
        }
        if let Some((ts, pct, _)) = self.last_sample(w) {
            let gap = pct - totals.values().sum::<f64>();
            if gap > 0.25 && start > 0 {
                let (by, total) = self.weights(start, ts);
                if total > 0.0 {
                    for (p, wt) in by {
                        *totals.entry(p).or_default() += gap * wt / total;
                    }
                } else {
                    *totals.entry(UNEXPLAINED.into()).or_default() += gap;
                }
            }
        }
        sorted(totals)
    }

    /// Each project's share of the work since `since`, in percent summing to 100
    pub fn share(&self, since: i64, now: i64) -> Vec<(String, f64)> {
        let (by, total) = self.weights(since, now);
        if total <= 0.0 {
            return Vec::new();
        }
        sorted(by.into_iter().map(|(p, w)| (p, w / total * 100.0)).collect())
    }

    /// Percent of a window's allowance attributed to each project over (from, to]
    pub fn attributed_pct(&self, w: Window, from: i64, to: i64) -> HashMap<String, f64> {
        let mut out = HashMap::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT project, SUM(delta_pct) FROM attribution WHERE window=?1 AND t1 > ?2 AND t1 <= ?3 GROUP BY project",
        ) {
            if let Ok(rows) = st.query_map(params![w.key(), from, to], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Per-turn token counts with their model, for pricing at API rates
    pub fn turns(&self, from: i64, to: i64) -> Vec<(String, String, [i64; 4])> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT project, model, input, output, cache_read, cache_write FROM usage_event WHERE ts >= ?1 AND ts <= ?2",
        ) {
            if let Ok(rows) = st.query_map(params![from, to], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, [r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?]))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// One aggregate per session for the turns in [from, to], oldest first. `price` prices one turn
    /// at API rates; a session's API cost is the sum of the turns it could price
    pub fn sessions(&self, from: i64, to: i64, price: &dyn Fn(&str, [i64; 4]) -> Option<f64>) -> Vec<SessionAgg> {
        let mut by: HashMap<String, SessionAgg> = HashMap::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT session_id, project, cwd, model, input, output, cache_read, cache_write, ts
             FROM usage_event WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts",
        ) {
            if let Ok(rows) = st.query_map(params![from, to], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    [r.get::<_, i64>(4)?, r.get(5)?, r.get(6)?, r.get(7)?],
                    r.get::<_, i64>(8)?,
                ))
            }) {
                for (sid, project, cwd, model, t, ts) in rows.flatten() {
                    let a = by.entry(sid.clone()).or_insert_with(|| SessionAgg {
                        session_id: sid,
                        project,
                        cwd,
                        first: ts,
                        last: ts,
                        ..Default::default()
                    });
                    a.weight += weight(t[0], t[1], t[2], t[3]);
                    a.tokens += t.iter().sum::<i64>();
                    a.turns += 1;
                    a.first = a.first.min(ts);
                    a.last = a.last.max(ts);
                    if let Some(c) = price(&model, t) {
                        *a.api_cost.get_or_insert(0.0) += c;
                    }
                    a.model = model; // the latest turn's
                }
            }
        }
        let mut out: Vec<SessionAgg> = by.into_values().collect();
        out.sort_by_key(|a| (a.first, a.session_id.clone()));
        out
    }

    /// The window's periods seen so far, oldest first: (start, end, highest reading in it). A
    /// period is known by the reset time its readings carried, and starts one window before it
    pub fn periods(&self, w: Window) -> Vec<(i64, i64, f64)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT resets_at, MAX(pct) FROM quota_sample WHERE window=?1 AND resets_at IS NOT NULL GROUP BY resets_at ORDER BY resets_at",
        ) {
            if let Ok(rows) = st.query_map([w.key()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))) {
                out.extend(rows.flatten().map(|(end, pct)| (end - w.secs(), end, pct)));
            }
        }
        out
    }

    /// Tokens per local calendar day ("YYYY-MM-DD", oldest first)
    pub fn daily_tokens(&self) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.db.prepare_cached(
            "SELECT date(ts, 'unixepoch', 'localtime') AS d, SUM(input + output + cache_read + cache_write)
             FROM usage_event GROUP BY d ORDER BY d",
        ) {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }
}

fn sorted(map: HashMap<String, f64>) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: &str, ts: i64, project: &str, input: i64, output: i64) -> Event {
        Event {
            ts,
            session_id: "s".into(),
            dedupe_key: key.into(),
            project: project.into(),
            cwd: project.into(),
            model: "claude-sonnet-5".into(),
            input,
            output,
            ..Default::default()
        }
    }

    fn commit(s: &mut Store, events: &[Event]) {
        s.commit(events, "f", Cursor { inode: 1, size: 1, offset: 1 }).unwrap();
    }

    #[test]
    fn a_streamed_turn_counts_once_at_its_final_size() {
        let mut s = Store::memory();
        commit(&mut s, &[ev("r:1", 10, "/a", 100, 5), ev("r:1", 11, "/a", 100, 50)]);
        commit(&mut s, &[ev("r:1", 12, "/a", 100, 20)]);
        assert_eq!(s.event_count(), 1);
        let (by, _) = s.weights(0, 100);
        assert_eq!(by["/a"], 100.0 + 50.0 * K_OUTPUT, "the maximum of each count");
    }

    #[test]
    fn a_rise_is_split_by_token_weight_since_the_last_point() {
        let mut s = Store::memory();
        s.record_sample(Window::Weekly, 10.0, None, 100);
        commit(&mut s, &[ev("a", 150, "/a", 300, 0), ev("b", 160, "/b", 100, 0)]);
        s.record_sample(Window::Weekly, 10.0, None, 200); // unchanged: the anchor stays behind
        commit(&mut s, &[ev("c", 250, "/a", 0, 0)]);
        s.record_sample(Window::Weekly, 14.0, None, 300);
        let rows: HashMap<_, _> = s.current_period(Window::Weekly).into_iter().collect();
        assert!((rows["/a"] - 3.0).abs() < 1e-9 && (rows["/b"] - 1.0).abs() < 1e-9, "{rows:?}");
    }

    #[test]
    fn a_rise_nothing_local_explains_is_shown_as_elsewhere() {
        let s = Store::memory();
        s.record_sample(Window::Session, 1.0, None, 100);
        s.record_sample(Window::Session, 3.0, None, 200);
        assert_eq!(s.current_period(Window::Session), vec![(UNEXPLAINED.to_string(), 2.0)]);
    }

    #[test]
    fn a_fall_is_a_reset_and_starts_a_new_period() {
        let mut s = Store::memory();
        s.record_sample(Window::Session, 1.0, None, 100);
        commit(&mut s, &[ev("a", 150, "/old", 100, 0)]);
        s.record_sample(Window::Session, 40.0, None, 200);
        s.record_sample(Window::Session, 0.0, None, 300);
        commit(&mut s, &[ev("b", 350, "/new", 100, 0)]);
        s.record_sample(Window::Session, 5.0, None, 400);
        assert_eq!(s.current_period(Window::Session), vec![("/new".to_string(), 5.0)]);
    }

    #[test]
    fn what_codenotch_did_not_see_is_spread_over_the_period() {
        let mut s = Store::memory();
        // The vendor's reset puts the period's start at 1000; the first reading already says 20 %
        commit(&mut s, &[ev("a", 1100, "/a", 100, 0), ev("b", 1200, "/b", 300, 0)]);
        s.record_sample(Window::Session, 20.0, Some(1000 + 5 * 3600), 1300);
        let rows: HashMap<_, _> = s.current_period(Window::Session).into_iter().collect();
        assert!((rows["/a"] - 5.0).abs() < 1e-9 && (rows["/b"] - 15.0).abs() < 1e-9, "{rows:?}");
    }

    #[test]
    fn an_old_reading_read_again_is_not_a_new_sample() {
        let s = Store::memory();
        s.record_sample(Window::Weekly, 10.0, None, 100);
        s.record_sample(Window::Weekly, 50.0, None, 100);
        s.record_sample(Window::Weekly, 50.0, None, 90);
        assert_eq!(s.last_sample(Window::Weekly).map(|x| x.1), Some(10.0));
    }

    #[test]
    fn sessions_sum_their_turns_and_periods_follow_the_resets() {
        let mut s = Store::memory();
        let mut a = ev("a", 100, "/p", 10, 1);
        a.session_id = "one".into();
        let mut b = ev("b", 160, "/p", 20, 2);
        b.session_id = "one".into();
        b.model = "claude-opus-5".into();
        let mut c = ev("c", 130, "/q", 5, 0);
        c.session_id = "two".into();
        commit(&mut s, &[a, b, c]);
        let price = |m: &str, t: [i64; 4]| (m == "claude-opus-5").then_some(t[0] as f64);
        let list = s.sessions(0, 1000, &price);
        assert_eq!(list.iter().map(|x| x.session_id.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!((list[0].first, list[0].last, list[0].turns, list[0].tokens), (100, 160, 2, 33));
        assert_eq!(list[0].model, "claude-opus-5", "the latest turn's model");
        assert_eq!(list[0].api_cost, Some(20.0), "only the turns that could be priced");
        assert_eq!(list[1].api_cost, None);
        s.record_sample(Window::Weekly, 5.0, Some(700_000), 100);
        s.record_sample(Window::Weekly, 9.0, Some(700_000), 200);
        s.record_sample(Window::Weekly, 1.0, Some(1_300_000), 300);
        assert_eq!(s.periods(Window::Weekly), vec![(700_000 - 604_800, 700_000, 9.0), (1_300_000 - 604_800, 1_300_000, 1.0)]);
    }
}
