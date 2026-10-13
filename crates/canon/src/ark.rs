//! Ark's records: every backup taken and every restore drill run.

use rusqlite::{OptionalExtension, Row, params};

use crate::{Canon, Result, now_ms, record};

/// One backup of one target, successful or not.
#[derive(Debug, Clone, PartialEq)]
pub struct Backup {
    pub id: i64,
    pub app: String,
    /// `service/volume`, or `canon` for Henry's own state.
    pub target: String,
    pub snapshot: Option<String>,
    pub bytes: Option<i64>,
    pub ok: bool,
    pub detail: Option<String>,
    pub at: i64,
}

/// One restore drill: a snapshot restored into a scratch copy and checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Drill {
    pub id: i64,
    pub app: String,
    pub target: String,
    pub snapshot: String,
    pub ok: bool,
    pub detail: Option<String>,
    pub at: i64,
}

const BACKUP_COLS: &str = "id, app, target, snapshot, bytes, ok, detail, at";
const DRILL_COLS: &str = "id, app, target, snapshot, ok, detail, at";

impl Canon {
    /// Record a backup. A failure goes to the event log; a success does not,
    /// since one a day per volume would drown everything else.
    pub fn record_backup(
        &self,
        app: &str,
        target: &str,
        snapshot: Option<&str>,
        bytes: Option<i64>,
        detail: Option<&str>,
    ) -> Result<i64> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let ok = snapshot.is_some();
        tx.execute(
            "INSERT INTO backups (app, target, snapshot, bytes, ok, detail, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![app, target, snapshot, bytes, ok, detail, now_ms()],
        )?;
        let id = tx.last_insert_rowid();
        if !ok {
            let why = detail.unwrap_or("unknown error");
            record(
                &tx,
                Some(app),
                "ark.failed",
                &format!("backup of {target} failed: {why}"),
                &serde_json::Value::Null,
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// The newest backups of an app, newest first.
    pub fn backups(&self, app: &str, limit: usize) -> Result<Vec<Backup>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare(&format!("SELECT {BACKUP_COLS} FROM backups WHERE app = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![app, limit as i64], backup_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The newest backup attempt of a target, whatever became of it.
    pub fn last_backup(&self, app: &str, target: &str) -> Result<Option<Backup>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {BACKUP_COLS} FROM backups WHERE app = ?1 AND target = ?2 ORDER BY id DESC LIMIT 1"),
                params![app, target],
                backup_row,
            )
            .optional()?)
    }

    pub fn last_good_backup(&self, app: &str, target: &str) -> Result<Option<Backup>> {
        Ok(self
            .conn()?
            .query_row(
                &format!(
                    "SELECT {BACKUP_COLS} FROM backups WHERE app = ?1 AND target = ?2 AND ok ORDER BY id DESC LIMIT 1"
                ),
                params![app, target],
                backup_row,
            )
            .optional()?)
    }

    pub fn record_drill(&self, app: &str, target: &str, snapshot: &str, ok: bool, detail: &str) -> Result<i64> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO drills (app, target, snapshot, ok, detail, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![app, target, snapshot, ok, detail, now_ms()],
        )?;
        let id = tx.last_insert_rowid();
        let (kind, verdict) = if ok { ("ark.drill", "restored and checked") } else { ("ark.drill_failed", "FAILED") };
        let short = &snapshot[..snapshot.len().min(8)];
        record(
            &tx,
            Some(app),
            kind,
            &format!("restore drill of {target} ({short}): {verdict}: {detail}"),
            &serde_json::Value::Null,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// The newest drill of a target, passed or failed.
    pub fn last_drill(&self, app: &str, target: &str) -> Result<Option<Drill>> {
        self.drill_where(app, target, "")
    }

    pub fn last_good_drill(&self, app: &str, target: &str) -> Result<Option<Drill>> {
        self.drill_where(app, target, "AND ok")
    }

    fn drill_where(&self, app: &str, target: &str, filter: &str) -> Result<Option<Drill>> {
        Ok(self
            .conn()?
            .query_row(
                &format!(
                    "SELECT {DRILL_COLS} FROM drills WHERE app = ?1 AND target = ?2 {filter} ORDER BY id DESC LIMIT 1"
                ),
                params![app, target],
                drill_row,
            )
            .optional()?)
    }
}

fn backup_row(r: &Row<'_>) -> rusqlite::Result<Backup> {
    Ok(Backup {
        id: r.get(0)?,
        app: r.get(1)?,
        target: r.get(2)?,
        snapshot: r.get(3)?,
        bytes: r.get(4)?,
        ok: r.get(5)?,
        detail: r.get(6)?,
        at: r.get(7)?,
    })
}

fn drill_row(r: &Row<'_>) -> rusqlite::Result<Drill> {
    Ok(Drill {
        id: r.get(0)?,
        app: r.get(1)?,
        target: r.get(2)?,
        snapshot: r.get(3)?,
        ok: r.get(4)?,
        detail: r.get(5)?,
        at: r.get(6)?,
    })
}
