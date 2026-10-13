//! Mend's records: upkeep runs, managed tool versions, and Bench's evals.

use rusqlite::{OptionalExtension, Row, params};

use crate::{Canon, Result, now_ms, record};

/// The last run of one upkeep task.
#[derive(Debug, Clone, PartialEq)]
pub struct Upkeep {
    pub task: String,
    pub last_at: i64,
    pub ok: bool,
    pub detail: Option<String>,
}

/// A binary Henry installs and upgrades itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub current: String,
    /// What `current` replaced, kept installed for a rollback.
    pub previous: Option<String>,
    /// A version switched to and not yet proven; a crash mid-switch is
    /// resolved by rolling back to `current`.
    pub pending: Option<String>,
    pub updated_at: i64,
}

/// One Bench run of one model.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchRun {
    pub id: i64,
    pub model: String,
    pub passed: i64,
    pub total: i64,
    pub tokens: i64,
    /// Per case: `{name: verdict}`.
    pub detail: serde_json::Value,
    pub at: i64,
}

impl Canon {
    /// Record an upkeep run. Failures go to the event log.
    pub fn record_upkeep(&self, task: &str, ok: bool, detail: &str) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO upkeep (task, last_at, ok, detail) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (task) DO UPDATE SET last_at = excluded.last_at, ok = excluded.ok, detail = excluded.detail",
            params![task, now_ms(), ok, detail],
        )?;
        if !ok {
            record(&tx, None, "mend.failed", &format!("upkeep `{task}` failed: {detail}"), &serde_json::Value::Null)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upkeep(&self, task: &str) -> Result<Option<Upkeep>> {
        Ok(self
            .conn()?
            .query_row("SELECT task, last_at, ok, detail FROM upkeep WHERE task = ?1", [task], upkeep_row)
            .optional()?)
    }

    pub fn upkeeps(&self) -> Result<Vec<Upkeep>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT task, last_at, ok, detail FROM upkeep ORDER BY task")?;
        let rows = stmt.query_map([], upkeep_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn tool(&self, name: &str) -> Result<Option<Tool>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT name, current, previous, pending, updated_at FROM tools WHERE name = ?1",
                [name],
                tool_row,
            )
            .optional()?)
    }

    pub fn tools(&self) -> Result<Vec<Tool>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT name, current, previous, pending, updated_at FROM tools ORDER BY name")?;
        let rows = stmt.query_map([], tool_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Write a tool's versions. Every change of `current` is an event.
    pub fn set_tool(&self, name: &str, current: &str, previous: Option<&str>, pending: Option<&str>) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let before: Option<String> =
            tx.query_row("SELECT current FROM tools WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        tx.execute(
            "INSERT INTO tools (name, current, previous, pending, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (name) DO UPDATE SET current = excluded.current, previous = excluded.previous,
                 pending = excluded.pending, updated_at = excluded.updated_at",
            params![name, current, previous, pending, now_ms()],
        )?;
        if before.as_deref() != Some(current) {
            let from = before.as_deref().unwrap_or("nothing");
            record(&tx, None, "mend.tool", &format!("{name}: {from} → {current}"), &serde_json::Value::Null)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_bench(
        &self,
        model: &str,
        passed: i64,
        total: i64,
        tokens: i64,
        detail: &serde_json::Value,
    ) -> Result<i64> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO bench (model, passed, total, tokens, detail, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![model, passed, total, tokens, detail.to_string(), now_ms()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The newest run of each model, best first.
    pub fn bench_latest(&self) -> Result<Vec<BenchRun>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, model, passed, total, tokens, detail, at FROM bench
             WHERE id IN (SELECT MAX(id) FROM bench GROUP BY model)
             ORDER BY CAST(passed AS REAL) / MAX(total, 1) DESC, tokens ASC",
        )?;
        let rows = stmt.query_map([], bench_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn upkeep_row(r: &Row<'_>) -> rusqlite::Result<Upkeep> {
    Ok(Upkeep { task: r.get(0)?, last_at: r.get(1)?, ok: r.get(2)?, detail: r.get(3)? })
}

fn tool_row(r: &Row<'_>) -> rusqlite::Result<Tool> {
    Ok(Tool { name: r.get(0)?, current: r.get(1)?, previous: r.get(2)?, pending: r.get(3)?, updated_at: r.get(4)? })
}

fn bench_row(r: &Row<'_>) -> rusqlite::Result<BenchRun> {
    Ok(BenchRun {
        id: r.get(0)?,
        model: r.get(1)?,
        passed: r.get(2)?,
        total: r.get(3)?,
        tokens: r.get(4)?,
        detail: crate::json(r.get(5)?),
        at: r.get(6)?,
    })
}
