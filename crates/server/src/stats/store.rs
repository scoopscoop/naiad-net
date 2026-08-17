//! `stats.db` — DDL, write helpers, rollup + prune, odometer bumps, query.
//!
//! Items here are called by later tasks in the stats module tree (Tasks 3–6).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Minute-tier retention: 48 hours.
const MINUTE_RETENTION_SECS: i64 = 48 * 3600;
/// Hour-tier retention: 90 days.
const HOUR_RETENTION_SECS: i64 = 90 * 86400;

/// stats.db schema DDL (spec §Data model). Four tall metric tables + odometers
/// + meta. Applied idempotently on every open.
const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS samples_minute (
    ts_minute INTEGER NOT NULL,
    metric    TEXT    NOT NULL,
    label     TEXT    NOT NULL DEFAULT '',
    value     REAL    NOT NULL,
    PRIMARY KEY (ts_minute, metric, label)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_min_ts ON samples_minute(ts_minute);

CREATE TABLE IF NOT EXISTS rollup_hour (
    ts_hour INTEGER NOT NULL,
    metric  TEXT    NOT NULL,
    label   TEXT    NOT NULL DEFAULT '',
    v_min   REAL    NOT NULL,
    v_max   REAL    NOT NULL,
    v_avg   REAL    NOT NULL,
    n       INTEGER NOT NULL,
    PRIMARY KEY (ts_hour, metric, label)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_hour_ts ON rollup_hour(ts_hour);

CREATE TABLE IF NOT EXISTS rollup_day (
    ts_day INTEGER NOT NULL,
    metric TEXT    NOT NULL,
    label  TEXT    NOT NULL DEFAULT '',
    v_min  REAL    NOT NULL,
    v_max  REAL    NOT NULL,
    v_avg  REAL    NOT NULL,
    n      INTEGER NOT NULL,
    PRIMARY KEY (ts_day, metric, label)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS odometers (
    name    TEXT PRIMARY KEY,
    i_value INTEGER NOT NULL DEFAULT 0,
    t_value TEXT    NOT NULL DEFAULT ''
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS stats_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
"#;

// ── Types ─────────────────────────────────────────────────────────────────────

/// Single writer connection to stats.db (guarded; all writes come from the
/// flush task and the two samplers, never a request handler).
pub struct StatsDb {
    conn: Mutex<Connection>,
}

/// A closed minute, flattened for persistence. Produced by the flush task from
/// a swapped-out `MinuteBucket` (Task 4). Percentiles are pre-computed per
/// endpoint so `store.rs` needs no histogram knowledge.
pub(crate) struct MinuteWrite {
    pub ts_minute: i64,
    /// (endpoint, status_class, count)
    pub requests: Vec<(String, &'static str, u64)>,
    /// (endpoint, summed buffered bytes)
    pub bytes: Vec<(String, u64)>,
    /// (endpoint, p50_ms, p95_ms, p99_ms)
    pub latency: Vec<(String, f64, f64, f64)>,
    /// total requests this minute (for req_total odometer)
    pub total_requests: u64,
    /// total buffered bytes this minute (for bytes_shipped_total odometer)
    pub total_bytes: u64,
    /// streaming responses (no Content-Length) this minute
    pub streamed: u64,
}

/// The time-range used by the stats API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Range {
    /// Last 24 hours — source: `samples_minute`.
    H24,
    /// Last 7 days — source: `rollup_hour`.
    D7,
    /// Last 90 days — source: `rollup_hour`.
    D90,
    /// All time — source: `rollup_day`.
    All,
}

impl Range {
    /// Parse a `?range=` query parameter. Anything unrecognised clamps to
    /// `H24` (24 h). No 400 — this is an operator-only tool.
    pub(crate) fn parse(s: Option<&str>) -> Range {
        match s.unwrap_or("24h") {
            "7d" => Range::D7,
            "90d" => Range::D90,
            "all" => Range::All,
            _ => Range::H24,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Range::H24 => "24h",
            Range::D7 => "7d",
            Range::D90 => "90d",
            Range::All => "all",
        }
    }
}

// ── Serde shapes (JSON contract for GET /api/stats) ──────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OdometerBusiestHour {
    pub count: i64,
    pub at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OdometerTopPrefix {
    pub count: i64,
    pub key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Odometers {
    pub req_total: i64,
    pub bytes_shipped_total: i64,
    pub busiest_hour: OdometerBusiestHour,
    pub top_prefix: OdometerTopPrefix,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct LatMs {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct ByEndpoint {
    pub requests: HashMap<String, i64>,
    pub bytes: i64,
    pub lat_ms: LatMs,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StatsPayload {
    pub generated_at: i64,
    pub range: String,
    pub server_version: String,
    pub uptime_secs: u64,
    pub odometers: Odometers,
    /// Time-series data: metric → `[(ts, value), …]`
    pub series: HashMap<String, Vec<(i64, f64)>>,
    /// Per-endpoint request stats.
    pub by_endpoint: HashMap<String, ByEndpoint>,
}

/// Convenience alias: the `series` map returned by `query_payload`.
type SeriesMap = HashMap<String, Vec<(i64, f64)>>;
/// Convenience alias: the `by_endpoint` map returned by `query_payload`.
type EndpointMap = HashMap<String, ByEndpoint>;

// ── StatsDb implementation ────────────────────────────────────────────────────

impl StatsDb {
    /// Open (creating if absent) stats.db in WAL mode, best-effort durability.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening stats db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "user_version", 1)?;
        conn.execute_batch(DDL).context("applying stats.db DDL")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Upsert one scalar sample row.
    pub fn write_sample(
        &self,
        ts_minute: i64,
        metric: &str,
        label: &str,
        value: f64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO samples_minute(ts_minute, metric, label, value) VALUES (?1,?2,?3,?4)
             ON CONFLICT(ts_minute, metric, label) DO UPDATE SET value = excluded.value",
            rusqlite::params![ts_minute, metric, label, value],
        )?;
        Ok(())
    }

    /// Write a closed minute's aggregates in one transaction.
    pub(crate) fn write_minute(&self, mw: &MinuteWrite) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO samples_minute(ts_minute, metric, label, value) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(ts_minute, metric, label) DO UPDATE SET value = excluded.value",
            )?;
            for (ep, class, count) in &mw.requests {
                ins.execute(rusqlite::params![
                    mw.ts_minute,
                    "requests",
                    format!("{ep} {class}"),
                    *count as f64
                ])?;
            }
            for (ep, bytes) in &mw.bytes {
                ins.execute(rusqlite::params![
                    mw.ts_minute,
                    "bytes_served",
                    ep,
                    *bytes as f64
                ])?;
            }
            for (ep, p50, p95, p99) in &mw.latency {
                ins.execute(rusqlite::params![mw.ts_minute, "lat_p50", ep, *p50])?;
                ins.execute(rusqlite::params![mw.ts_minute, "lat_p95", ep, *p95])?;
                ins.execute(rusqlite::params![mw.ts_minute, "lat_p99", ep, *p99])?;
            }
        }
        // Odometers: monotonic all-time counters.
        tx.execute(
            "INSERT INTO odometers(name, i_value) VALUES ('req_total', ?1)
             ON CONFLICT(name) DO UPDATE SET i_value = i_value + ?1",
            rusqlite::params![mw.total_requests as i64],
        )?;
        tx.execute(
            "INSERT INTO odometers(name, i_value) VALUES ('bytes_shipped_total', ?1)
             ON CONFLICT(name) DO UPDATE SET i_value = i_value + ?1",
            rusqlite::params![mw.total_bytes as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Fold minute→hour and hour→day, update record-holder odometers, prune
    /// stale rows. Idempotent: `INSERT … ON CONFLICT DO NOTHING` guards all
    /// folds. Advances `stats_meta['last_rollup_hour']` on success.
    pub(crate) fn roll_and_prune(&self, now: i64) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;

        // 1. minute → hour: fold every full hour with no rollup_hour row yet.
        //    A "full hour" is one strictly before the current hour boundary.
        let cur_hour = now / 3600 * 3600;
        tx.execute(
            "INSERT INTO rollup_hour(ts_hour, metric, label, v_min, v_max, v_avg, n)
             SELECT (ts_minute/3600)*3600 AS h, metric, label,
                    MIN(value), MAX(value), AVG(value), COUNT(*)
             FROM samples_minute
             WHERE ts_minute < ?1
             GROUP BY h, metric, label
             ON CONFLICT(ts_hour, metric, label) DO NOTHING",
            rusqlite::params![cur_hour],
        )?;

        // 2. hour → day: n-weighted average, MIN(v_min)/MAX(v_max).
        let cur_day = now / 86400 * 86400;
        tx.execute(
            "INSERT INTO rollup_day(ts_day, metric, label, v_min, v_max, v_avg, n)
             SELECT (ts_hour/86400)*86400 AS d, metric, label,
                    MIN(v_min), MAX(v_max),
                    SUM(v_avg * n) / CAST(SUM(n) AS REAL), SUM(n)
             FROM rollup_hour
             WHERE ts_hour < ?1
             GROUP BY d, metric, label
             ON CONFLICT(ts_day, metric, label) DO NOTHING",
            rusqlite::params![cur_day],
        )?;

        // 3. odometers busiest_hour / top_prefix: whole-table scan with a
        //    monotonic guard (c > prev) so the record only ever ratchets up.
        //    busiest_hour = max over all rollup_hour rows of SUM(v_avg*n)
        //    for 'requests' within each hour.
        let best: Option<(i64, i64)> = tx
            .query_row(
                "SELECT ts_hour, CAST(SUM(v_avg * n) AS INTEGER) AS c
                 FROM rollup_hour WHERE metric = 'requests'
                 GROUP BY ts_hour ORDER BY c DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((h, c)) = best {
            let prev: i64 = tx
                .query_row(
                    "SELECT i_value FROM odometers WHERE name = 'busiest_hour'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if c > prev {
                tx.execute(
                    "INSERT INTO odometers(name, i_value, t_value) VALUES ('busiest_hour', ?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET i_value = ?1, t_value = ?2",
                    rusqlite::params![c, h.to_string()],
                )?;
            }
        }

        // top_prefix = endpoint with the highest all-time request total.
        // Scans the full samples_minute table; ratchets up monotonically.
        let top: Option<(String, i64)> = tx
            .query_row(
                "SELECT substr(label, 1, instr(label || ' ', ' ') - 1) AS ep,
                        CAST(SUM(value) AS INTEGER) AS c
                 FROM samples_minute WHERE metric = 'requests'
                 GROUP BY ep ORDER BY c DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((ep, c)) = top {
            let prev: i64 = tx
                .query_row(
                    "SELECT i_value FROM odometers WHERE name = 'top_prefix'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if c > prev {
                tx.execute(
                    "INSERT INTO odometers(name, i_value, t_value) VALUES ('top_prefix', ?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET i_value = ?1, t_value = ?2",
                    rusqlite::params![c, ep],
                )?;
            }
        }

        // 4. prune.
        tx.execute(
            "DELETE FROM samples_minute WHERE ts_minute < ?1",
            rusqlite::params![now - MINUTE_RETENTION_SECS],
        )?;
        tx.execute(
            "DELETE FROM rollup_hour WHERE ts_hour < ?1",
            rusqlite::params![now - HOUR_RETENTION_SECS],
        )?;
        // Record the last processed hour boundary. Currently write-only
        // bookkeeping: resumability is provided structurally by the full-window
        // idempotent fold (ON CONFLICT DO NOTHING), not by reading this value.
        // A future optimisation could use it to skip already-rolled windows.
        tx.execute(
            "INSERT INTO stats_meta(key, value) VALUES ('last_rollup_hour', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![cur_hour.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

// ── Test-only inspection helpers ──────────────────────────────────────────────

#[cfg(test)]
impl StatsDb {
    /// Read a single rollup_hour row for testing.
    pub(crate) fn hour_row(
        &self,
        metric: &str,
        label: &str,
        ts_hour: i64,
    ) -> anyhow::Result<(f64, f64, f64, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT v_min, v_max, v_avg, n FROM rollup_hour
             WHERE ts_hour=?1 AND metric=?2 AND label=?3",
            rusqlite::params![ts_hour, metric, label],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .context("hour_row not found")
    }

    /// Directly insert a rollup_hour row (for test setup).
    /// `vals` = `(v_min, v_max, v_avg, n)`.
    pub(crate) fn insert_hour_row(
        &self,
        metric: &str,
        label: &str,
        ts_hour: i64,
        (v_min, v_max, v_avg, n): (f64, f64, f64, i64),
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR REPLACE INTO rollup_hour(ts_hour, metric, label, v_min, v_max, v_avg, n)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![ts_hour, metric, label, v_min, v_max, v_avg, n],
        )?;
        Ok(())
    }

    /// Directly insert a rollup_day row (for test setup).
    /// `vals` = `(v_min, v_max, v_avg, n)`.
    pub(crate) fn insert_day_row(
        &self,
        metric: &str,
        label: &str,
        ts_day: i64,
        (v_min, v_max, v_avg, n): (f64, f64, f64, i64),
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR REPLACE INTO rollup_day(ts_day, metric, label, v_min, v_max, v_avg, n)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![ts_day, metric, label, v_min, v_max, v_avg, n],
        )?;
        Ok(())
    }

    /// Read a single samples_minute value for testing (None if absent).
    pub(crate) fn minute_row(&self, ts: i64, metric: &str, label: &str) -> Option<f64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT value FROM samples_minute WHERE ts_minute=?1 AND metric=?2 AND label=?3",
            rusqlite::params![ts, metric, label],
            |r| r.get(0),
        )
        .ok()
    }

    /// Count rows in samples_minute.
    pub(crate) fn minute_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM samples_minute", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Count rows in rollup_hour.
    pub(crate) fn hour_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM rollup_hour", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Read a single rollup_day row for testing.
    pub(crate) fn day_row(
        &self,
        metric: &str,
        label: &str,
        ts_day: i64,
    ) -> anyhow::Result<(f64, f64, f64, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT v_min, v_max, v_avg, n FROM rollup_day
             WHERE ts_day=?1 AND metric=?2 AND label=?3",
            rusqlite::params![ts_day, metric, label],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .context("day_row not found")
    }

    /// Count rows in rollup_day.
    pub(crate) fn day_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM rollup_day", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Read the `i_value` of a named odometer (0 if absent).
    pub(crate) fn odometer_i(&self, name: &str) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT i_value FROM odometers WHERE name=?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// Read the `t_value` of a named odometer (empty string if absent).
    pub(crate) fn odometer_t(&self, name: &str) -> String {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT t_value FROM odometers WHERE name=?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .unwrap_or_default()
    }
}

// ── query_payload ─────────────────────────────────────────────────────────────

/// Produce the documented JSON payload from `stats.db`. The `range` determines
/// the source table and time window:
/// - `H24` → `samples_minute`, last 24 h relative to `generated_at`
/// - `D7`  → `rollup_hour`,    last 7 d
/// - `D90` → `rollup_hour`,    last 90 d
/// - `All` → `rollup_day`,     unfiltered
///
/// For rollup ranges, `by_endpoint` request and bytes totals use the
/// `v_avg * n` weighted sum so they represent actual counts, not averages.
/// Fully wired to the HTTP handler in Task 6.
#[allow(dead_code)] // test-only convenience; HTTP handler calls query_payload_conn directly
pub(crate) fn query_payload(
    db: &StatsDb,
    range: Range,
    server_version: &str,
    uptime_secs: u64,
    generated_at: i64,
) -> anyhow::Result<StatsPayload> {
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    query_payload_conn(&conn, range, server_version, uptime_secs, generated_at)
}

/// Produce the documented JSON payload from a raw SQLite connection.
/// Called by the HTTP handler, which holds a dedicated read-only connection.
pub(crate) fn query_payload_conn(
    conn: &Connection,
    range: Range,
    server_version: &str,
    uptime_secs: u64,
    generated_at: i64,
) -> anyhow::Result<StatsPayload> {
    let (series, by_endpoint) = match range {
        Range::H24 => query_series_minute(conn, generated_at - 86400)?,
        Range::D7 => query_series_rollup(conn, "rollup_hour", "ts_hour", generated_at - 7 * 86400)?,
        Range::D90 => {
            query_series_rollup(conn, "rollup_hour", "ts_hour", generated_at - 90 * 86400)?
        }
        Range::All => query_series_rollup(conn, "rollup_day", "ts_day", 0)?,
    };

    // Odometers.
    let req_total: i64 = conn
        .query_row(
            "SELECT i_value FROM odometers WHERE name='req_total'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let bytes_shipped_total: i64 = conn
        .query_row(
            "SELECT i_value FROM odometers WHERE name='bytes_shipped_total'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let (bh_count, bh_at): (i64, i64) = conn
        .query_row(
            "SELECT i_value, CAST(t_value AS INTEGER) FROM odometers WHERE name='busiest_hour'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    let (tp_count, tp_key): (i64, String) = conn
        .query_row(
            "SELECT i_value, t_value FROM odometers WHERE name='top_prefix'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, String::new()));

    Ok(StatsPayload {
        generated_at,
        range: range.as_str().to_owned(),
        server_version: server_version.to_owned(),
        uptime_secs,
        odometers: Odometers {
            req_total,
            bytes_shipped_total,
            busiest_hour: OdometerBusiestHour {
                count: bh_count,
                at: bh_at,
            },
            top_prefix: OdometerTopPrefix {
                count: tp_count,
                key: tp_key,
            },
        },
        series,
        by_endpoint,
    })
}

/// Build (series, by_endpoint) from `samples_minute` (Range::H24).
/// Only rows with `ts_minute >= since` are included.
fn query_series_minute(conn: &Connection, since: i64) -> anyhow::Result<(SeriesMap, EndpointMap)> {
    let mut series: SeriesMap = HashMap::new();
    let mut by_ep: EndpointMap = HashMap::new();

    let mut stmt = conn.prepare(
        "SELECT ts_minute, metric, label, value
         FROM samples_minute WHERE ts_minute >= ?1 ORDER BY ts_minute",
    )?;
    let mut rows = stmt.query(rusqlite::params![since])?;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let metric: String = row.get(1)?;
        let label: String = row.get(2)?;
        let value: f64 = row.get(3)?;
        // For minute data, value IS the count/raw reading — use as both
        // series value and weighted count.
        accumulate_row(&metric, &label, ts, value, value, &mut series, &mut by_ep);
    }
    Ok((series, by_ep))
}

/// Build (series, by_endpoint) from a rollup table (Range::D7/D90/All).
/// Only rows with `{ts_col} >= since` are included (pass 0 for unfiltered).
/// For `requests`/`bytes_served` in `by_endpoint`, uses `v_avg * n` so the
/// total reflects actual counts rather than per-sample averages.
fn query_series_rollup(
    conn: &Connection,
    table: &str,
    ts_col: &str,
    since: i64,
) -> anyhow::Result<(SeriesMap, EndpointMap)> {
    let mut series: SeriesMap = HashMap::new();
    let mut by_ep: EndpointMap = HashMap::new();

    let sql = format!(
        "SELECT {ts_col}, metric, label, v_avg, n
         FROM {table} WHERE {ts_col} >= ?1 ORDER BY {ts_col}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params![since])?;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let metric: String = row.get(1)?;
        let label: String = row.get(2)?;
        let v_avg: f64 = row.get(3)?;
        let n: i64 = row.get(4)?;
        // For rollup data: series point = v_avg (the average for that window);
        // count/bytes total = v_avg * n (the weighted sum over the window).
        accumulate_row(
            &metric,
            &label,
            ts,
            v_avg,
            v_avg * n as f64,
            &mut series,
            &mut by_ep,
        );
    }
    Ok((series, by_ep))
}

/// Route one DB row into `series` (scalar/unlabeled metrics) or `by_endpoint`
/// (request/bytes/latency endpoint metrics).
///
/// `series_value` is the plotted value (v_avg for rollup, raw value for minute).
/// `count_value` is the weighted total for count metrics: `v_avg * n` for
/// rollup ranges so `by_endpoint.requests` and `.bytes` sum actual counts, not
/// per-sample averages.
fn accumulate_row(
    metric: &str,
    label: &str,
    ts: i64,
    series_value: f64,
    count_value: f64,
    series: &mut SeriesMap,
    by_ep: &mut EndpointMap,
) {
    match metric {
        "requests" => {
            // label = "<endpoint> <class>"
            if let Some((ep, class)) = label.rsplit_once(' ') {
                let ep_entry = by_ep.entry(ep.to_owned()).or_default();
                *ep_entry.requests.entry(class.to_owned()).or_insert(0) +=
                    count_value.round() as i64;
            }
        }
        "bytes_served" => {
            // label = "<endpoint>"
            if !label.is_empty() {
                let ep_entry = by_ep.entry(label.to_owned()).or_default();
                ep_entry.bytes += count_value.round() as i64;
            }
        }
        "lat_p50" => {
            if !label.is_empty() {
                let ep_entry = by_ep.entry(label.to_owned()).or_default();
                ep_entry.lat_ms.p50 = series_value;
            }
        }
        "lat_p95" => {
            if !label.is_empty() {
                let ep_entry = by_ep.entry(label.to_owned()).or_default();
                ep_entry.lat_ms.p95 = series_value;
            }
        }
        "lat_p99" => {
            if !label.is_empty() {
                let ep_entry = by_ep.entry(label.to_owned()).or_default();
                ep_entry.lat_ms.p99 = series_value;
            }
        }
        _ => {
            // Scalar metric: label is '' for system/store metrics.
            // Append label when non-empty (e.g. a per-interface counter).
            let key = if label.is_empty() {
                metric.to_owned()
            } else {
                format!("{metric}/{label}")
            };
            series.entry(key).or_default().push((ts, series_value));
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, StatsDb) {
        let d = tempfile::tempdir().unwrap();
        let db = StatsDb::open(&d.path().join("stats.db")).unwrap();
        (d, db)
    }

    #[test]
    fn rollup_folds_two_full_hours_with_idempotency() {
        let (_d, db) = open_temp();
        // Two full hours of cpu_pct minute samples: hour A = [10,20,30], hour B = [40,60].
        let hour_a = 1_000_000i64 / 3600 * 3600; // a clean hour boundary
        for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
            db.write_sample(hour_a + i as i64 * 60, "cpu_pct", "", *v)
                .unwrap();
        }
        let hour_b = hour_a + 3600;
        for (i, v) in [40.0, 60.0].iter().enumerate() {
            db.write_sample(hour_b + i as i64 * 60, "cpu_pct", "", *v)
                .unwrap();
        }
        // now = well past hour_b so both hours are complete.
        let now = hour_b + 7200;
        db.roll_and_prune(now).unwrap();
        let (mn, mx, avg, n) = db.hour_row("cpu_pct", "", hour_a).unwrap();
        assert_eq!((mn, mx, n), (10.0, 30.0, 3));
        assert!((avg - 20.0).abs() < 1e-9);
        // Idempotent: a second run must not change or duplicate anything.
        db.roll_and_prune(now).unwrap();
        let (_, _, _, n2) = db.hour_row("cpu_pct", "", hour_a).unwrap();
        assert_eq!(n2, 3, "second roll_and_prune must not double-count");

        // hour → day n-weighted v_avg: roll past the day boundary so both
        // hours (which are in the same UTC day as hour_a) get folded.
        // day_avg = SUM(v_avg*n)/SUM(n) = (20.0*3 + 50.0*2) / 5 = 32.0.
        // This exercises the load-bearing SUM(v_avg*n)/SUM(n) fold formula.
        let day_ts = hour_a / 86400 * 86400;
        let next_day_now = day_ts + 86400 + 7200; // well into the next UTC day
        db.roll_and_prune(next_day_now).unwrap();
        let (_, _, day_avg, day_n) = db
            .day_row("cpu_pct", "", day_ts)
            .expect("day row must exist after rolling past the day boundary");
        assert_eq!(day_n, 5, "day row must aggregate both hours' sample counts");
        assert!(
            (day_avg - 32.0).abs() < 1e-9,
            "day v_avg must be n-weighted: (20*3 + 50*2)/5 = 32.0, got {day_avg}"
        );
    }

    #[test]
    fn prune_deletes_stale_minutes_and_hours_but_never_days() {
        let (_d, db) = open_temp();
        // Use 100d + 30 min so cur_hour = 100*86400 and "now - 10 min" sits
        // inside the current (still-open) hour and is NOT folded to rollup_hour.
        // If now were exactly on an hour boundary the recent-minute row would
        // land in the preceding hour and create an extra hour row.
        let now = 100 * 86400 + 1800; // 30 min past midnight of day 100
        db.write_sample(now - 49 * 3600, "cpu_pct", "", 1.0)
            .unwrap(); // >48h → pruned
        db.write_sample(now - 10 * 60, "cpu_pct", "", 2.0).unwrap(); // recent → kept (in cur hour)
        db.insert_hour_row("cpu_pct", "", now - 91 * 86400, (1.0, 1.0, 1.0, 1))
            .unwrap(); // >90d → pruned
        db.insert_hour_row("cpu_pct", "", now - 86400, (2.0, 2.0, 2.0, 1))
            .unwrap(); // recent → kept
        db.insert_day_row("cpu_pct", "", now - 400 * 86400, (3.0, 3.0, 3.0, 1))
            .unwrap(); // ancient day → kept forever
        db.roll_and_prune(now).unwrap();
        assert_eq!(db.minute_count(), 1, "only the stale minute row is pruned");
        assert_eq!(
            db.hour_count(),
            2,
            "stale hour pruned; recent + newly-rolled H49 kept"
        );
        // rollup_day is NEVER pruned. roll_and_prune also folds hour rows into
        // day rows, so the total will be >= 1 (ancient row plus new day rows).
        assert!(
            db.day_count() >= 1,
            "rollup_day is never pruned — ancient row must survive (got {})",
            db.day_count()
        );
        // The specifically-seeded ancient day row must still be present: a
        // regression that only prunes old day rows would still pass the >= 1
        // check above, so we look up the exact row to prove nothing was deleted.
        let ancient_ts = now - 400 * 86400;
        let (_, _, ancient_avg, ancient_n) = db
            .day_row("cpu_pct", "", ancient_ts)
            .expect("the seeded ancient day row (now - 400d) must not be pruned");
        assert_eq!(ancient_n, 1, "ancient day row count must be intact");
        assert!(
            (ancient_avg - 3.0).abs() < 1e-9,
            "ancient day row value must be intact"
        );
    }

    #[test]
    fn busiest_hour_and_top_prefix_update_only_on_record() {
        let (_d, db) = open_temp();
        let hour = 5_000_000i64 / 3600 * 3600;
        // Seed a closed hour of requests: /repo/buckets 2xx = 100.
        db.write_sample(hour, "requests", "/repo/buckets 2xx", 100.0)
            .unwrap();
        db.roll_and_prune(hour + 7200).unwrap();
        assert_eq!(db.odometer_i("busiest_hour"), 100);
        assert_eq!(db.odometer_t("top_prefix"), "/repo/buckets");
        // A quieter later hour must NOT lower the record.
        let hour2 = hour + 3600;
        db.write_sample(hour2, "requests", "/repo/buckets 2xx", 10.0)
            .unwrap();
        db.roll_and_prune(hour2 + 7200).unwrap();
        assert_eq!(db.odometer_i("busiest_hour"), 100, "record must not drop");
    }

    #[test]
    fn write_minute_inserts_rows_and_accumulates_odometers() {
        let (_d, db) = open_temp();
        let ts = 1_800_000i64;

        // First call: /repo/buckets with 2xx requests, bytes, and latency.
        db.write_minute(&MinuteWrite {
            ts_minute: ts,
            requests: vec![("/repo/buckets".to_owned(), "2xx", 50)],
            bytes: vec![("/repo/buckets".to_owned(), 1024)],
            latency: vec![("/repo/buckets".to_owned(), 10.0, 50.0, 100.0)],
            total_requests: 50,
            total_bytes: 1024,
            streamed: 0,
        })
        .unwrap();

        // "{ep} {class}" label encoding for requests.
        assert_eq!(
            db.minute_row(ts, "requests", "/repo/buckets 2xx"),
            Some(50.0),
            "request label must be '<endpoint> <class>'"
        );
        // bytes_served label is just the endpoint.
        assert_eq!(
            db.minute_row(ts, "bytes_served", "/repo/buckets"),
            Some(1024.0)
        );
        // Latency rows use endpoint label only.
        assert_eq!(db.minute_row(ts, "lat_p50", "/repo/buckets"), Some(10.0));
        assert_eq!(db.minute_row(ts, "lat_p95", "/repo/buckets"), Some(50.0));
        assert_eq!(db.minute_row(ts, "lat_p99", "/repo/buckets"), Some(100.0));

        // Odometers after the first call.
        assert_eq!(db.odometer_i("req_total"), 50);
        assert_eq!(db.odometer_i("bytes_shipped_total"), 1024);

        // Second call for the same ts_minute: upsert replaces values;
        // odometers accumulate (add, not replace).
        db.write_minute(&MinuteWrite {
            ts_minute: ts,
            requests: vec![("/repo/buckets".to_owned(), "2xx", 75)],
            bytes: vec![("/repo/buckets".to_owned(), 2048)],
            latency: vec![("/repo/buckets".to_owned(), 12.0, 55.0, 110.0)],
            total_requests: 75,
            total_bytes: 2048,
            streamed: 0,
        })
        .unwrap();

        // Upsert: value replaced (not summed in the row).
        assert_eq!(
            db.minute_row(ts, "requests", "/repo/buckets 2xx"),
            Some(75.0),
            "duplicate write must upsert (replace) the sample value"
        );
        assert_eq!(
            db.minute_row(ts, "lat_p50", "/repo/buckets"),
            Some(12.0),
            "latency upsert must replace the value"
        );

        // Odometers accumulate across both calls: 50+75=125, 1024+2048=3072.
        assert_eq!(
            db.odometer_i("req_total"),
            125,
            "req_total must accumulate across write_minute calls"
        );
        assert_eq!(
            db.odometer_i("bytes_shipped_total"),
            3072,
            "bytes_shipped_total must accumulate across write_minute calls"
        );
    }

    /// Regression test for Defect 1: ts_minute unit mismatch.
    ///
    /// Builds a `MinuteAccumulator` with a REAL `now` timestamp (unix seconds
    /// floored to 60), records synthetic requests, swaps, writes to a real
    /// `StatsDb`, then queries `Range::H24` at the same real `now` and asserts
    /// `by_endpoint` is NON-EMPTY and the `requests` series appears.
    ///
    /// Before the fix, `MinuteAccumulator::new` and the flush path both used
    /// `epoch_secs / 60` (minute-epoch ~60× smaller than unix seconds), which
    /// placed every row ~60× below the 24h window floor and produced an empty
    /// `by_endpoint`.
    #[test]
    fn write_minute_and_query_payload_24h_with_real_timestamps() {
        use crate::stats::middleware::MinuteAccumulator;

        let (_d, db) = open_temp();

        // Use a real now value (unix seconds floored to the minute).
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let now_minute = now_secs / 60 * 60;

        // Build accumulator with the CORRECT semantics (seconds-floored).
        let accum = MinuteAccumulator::new(now_minute);
        accum.record("/repo/buckets", 200, 15, Some(1024), false);
        accum.record("/repo/buckets", 200, 20, Some(512), false);
        accum.record("/repo/snapshot", 200, 5, None, true);

        // Swap at the same minute.
        let mw = accum.swap(now_minute);

        // Write to the real DB.
        db.write_minute(&mw).unwrap();

        // Query: generated_at = now_secs, range = H24.
        // The window is [now_secs - 86400, now_secs], so now_minute is inside it.
        let payload = query_payload(&db, Range::H24, "0.2.132-test", 0, now_secs).unwrap();

        assert!(
            !payload.by_endpoint.is_empty(),
            "by_endpoint must be non-empty — ts_minute must be inside the 24h window \
             (got by_endpoint = {:?}, now_minute = {now_minute}, now_secs = {now_secs})",
            payload.by_endpoint
        );

        let buckets_ep = payload
            .by_endpoint
            .get("/repo/buckets")
            .expect("/repo/buckets must appear in by_endpoint");
        assert!(
            buckets_ep.requests.get("2xx").copied().unwrap_or(0) > 0,
            "/repo/buckets must have 2xx request counts"
        );
    }

    #[test]
    fn query_payload_smoke() {
        let (_d, db) = open_temp();

        // ── Range::All (rollup_day) ──────────────────────────────────────────
        db.insert_day_row("cpu_pct", "", 86400, (10.0, 20.0, 15.0, 3))
            .unwrap();
        let payload = query_payload(&db, Range::All, "0.2.132", 5, 200_000).unwrap();
        assert!(
            payload.series.contains_key("cpu_pct"),
            "cpu_pct must appear in series"
        );
        let pts = &payload.series["cpu_pct"];
        assert!(!pts.is_empty(), "cpu_pct series must be non-empty");
        assert_eq!(pts[0].0, 86400i64, "timestamp must match seeded row");
        assert!((pts[0].1 - 15.0).abs() < 1e-9, "value must be v_avg");

        // ── Range::H24 windowed (samples_minute) ─────────────────────────────
        let now = 200_000i64;
        db.write_sample(now - 1800, "rss_bytes", "", 1_000_000.0)
            .unwrap(); // in window
        db.write_sample(now - 90_000, "rss_bytes", "", 999_000.0)
            .unwrap(); // outside >24h
        let payload_h24 = query_payload(&db, Range::H24, "0.2.132", 5, now).unwrap();
        let h24_pts = payload_h24
            .series
            .get("rss_bytes")
            .expect("rss_bytes must appear in H24 series");
        assert_eq!(
            h24_pts.len(),
            1,
            "H24 window must exclude rows older than 24h"
        );
        assert_eq!(h24_pts[0].0, now - 1800, "in-window timestamp must match");

        // ── Range::D7 weighted by_endpoint ──────────────────────────────────
        // Seed rollup_hour: /api 2xx, v_avg=75.0, n=2 → weighted total = 150.
        let hour_ts = 86400i64 * 5;
        let d7_now = hour_ts + 4 * 3600; // well within D7 window
        db.insert_hour_row("requests", "/api 2xx", hour_ts, (50.0, 100.0, 75.0, 2))
            .unwrap();
        let payload_d7 = query_payload(&db, Range::D7, "0.2.132", 5, d7_now).unwrap();
        let by_api = payload_d7
            .by_endpoint
            .get("/api")
            .expect("by_endpoint[/api] must appear in D7 payload");
        assert_eq!(
            by_api.requests.get("2xx").copied().unwrap_or(0),
            150,
            "by_endpoint requests must use v_avg*n weighted total for rollup ranges"
        );
    }
}
