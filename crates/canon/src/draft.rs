//! Onboarding records: drafts, the proposals Draft weighed, and the files
//! Henry overlays on an app's checkout.

use rusqlite::{OptionalExtension, Row, params};

use crate::{Canon, DraftStage, Error, Result, json, now_ms, record};

/// One repo on its way from URL to Live.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub app: String,
    pub repo: String,
    /// Unset: the repo's default branch.
    pub branch: Option<String>,
    pub hostname: Option<String>,
    pub stage: DraftStage,
    /// Plans tried so far. The repair budget caps it.
    pub round: i64,
    /// Model tokens spent, in and out, across every round.
    pub spent: i64,
    pub detail: Option<String>,
    /// The release this round is trying.
    pub release: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A plan as weighed: who wrote it, and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub id: i64,
    pub app: String,
    pub round: i64,
    /// `repo`, `sound` or `ghost`.
    pub source: String,
    pub model: Option<String>,
    pub lease: String,
    /// `{path: content}` for files Henry adds to the checkout.
    pub files: serde_json::Value,
    /// Secrets Henry generates: `[{name, template}]`. Never values.
    pub generate: serde_json::Value,
    pub notes: String,
    /// Unset while it is being tried; then `accepted`, or why it failed.
    pub verdict: Option<String>,
    pub created_at: i64,
}

pub struct NewProposal<'a> {
    pub app: &'a str,
    pub round: i64,
    pub source: &'a str,
    pub model: Option<&'a str>,
    pub lease: &'a str,
    pub files: &'a serde_json::Value,
    pub generate: &'a serde_json::Value,
    pub notes: &'a str,
}

const DRAFT_COLS: &str = "app, repo, branch, hostname, stage, round, spent, detail, release, created_at, updated_at";
const PROPOSAL_COLS: &str = "id, app, round, source, model, lease, files, generate, notes, verdict, created_at";

impl Canon {
    /// Begin onboarding `app`. A stuck draft starts over, even once a round
    /// has registered its app (the next round updates that app's lease), as
    /// long as it is the same repo; anything else with that name is taken.
    pub fn start_draft(&self, app: &str, repo: &str, branch: Option<&str>, hostname: Option<&str>) -> Result<Draft> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let registered: Option<String> =
            tx.query_row("SELECT repo FROM apps WHERE name = ?1", [app], |r| r.get(0)).optional()?;
        let stage: Option<String> =
            tx.query_row("SELECT stage FROM drafts WHERE app = ?1", [app], |r| r.get(0)).optional()?;
        let stuck = stage.as_deref() == Some(DraftStage::Stuck.as_str());
        let free = match (&registered, &stage) {
            (None, None) => true,
            (None, Some(_)) => stuck,
            (Some(r), Some(_)) => stuck && r == repo,
            (Some(_), None) => false,
        };
        if !free {
            return Err(Error::AppExists(app.to_owned()));
        }
        let now = now_ms();
        tx.execute(
            "INSERT INTO drafts (app, repo, branch, hostname, stage, round, spent, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'survey', 0, 0, ?5, ?5)
             ON CONFLICT (app) DO UPDATE SET repo = excluded.repo, branch = excluded.branch,
                 hostname = excluded.hostname, stage = 'survey', round = 0, spent = 0, detail = NULL,
                 release = NULL, updated_at = excluded.updated_at",
            params![app, repo, branch, hostname, now],
        )?;
        record(&tx, Some(app), "draft.started", &format!("onboarding {repo}"), &serde_json::Value::Null)?;
        let draft = get_draft(&tx, app)?.ok_or_else(|| Error::NoSuchApp(app.to_owned()))?;
        tx.commit()?;
        Ok(draft)
    }

    pub fn draft(&self, app: &str) -> Result<Option<Draft>> {
        get_draft(&*self.conn()?, app)
    }

    pub fn drafts(&self) -> Result<Vec<Draft>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!("SELECT {DRAFT_COLS} FROM drafts ORDER BY created_at"))?;
        let rows = stmt.query_map([], draft_row)?;
        rows.map(|r| r?).collect()
    }

    /// Move a draft to `stage`. The event log gets a line only when the
    /// stage or its detail changes.
    pub fn set_draft_stage(&self, app: &str, stage: DraftStage, detail: Option<&str>) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let before = get_draft(&tx, app)?.ok_or_else(|| Error::NoSuchApp(app.to_owned()))?;
        if before.stage == stage && before.detail.as_deref() == detail {
            return Ok(());
        }
        tx.execute(
            "UPDATE drafts SET stage = ?2, detail = ?3, updated_at = ?4 WHERE app = ?1",
            params![app, stage.as_str(), detail, now_ms()],
        )?;
        let message = match detail {
            Some(d) => format!("onboarding: {stage}: {d}"),
            None => format!("onboarding: {stage}"),
        };
        record(&tx, Some(app), "draft.stage", &message, &serde_json::json!({ "stage": stage.as_str() }))?;
        tx.commit()?;
        Ok(())
    }

    /// Start the next round of planning; returns its number.
    pub fn next_draft_round(&self, app: &str) -> Result<i64> {
        let conn = self.conn()?;
        let changed = conn
            .execute("UPDATE drafts SET round = round + 1, updated_at = ?2 WHERE app = ?1", params![app, now_ms()])?;
        if changed == 0 {
            return Err(Error::NoSuchApp(app.to_owned()));
        }
        Ok(conn.query_row("SELECT round FROM drafts WHERE app = ?1", [app], |r| r.get(0))?)
    }

    pub fn set_draft_release(&self, app: &str, release: Option<i64>) -> Result<()> {
        self.conn()?.execute(
            "UPDATE drafts SET release = ?2, updated_at = ?3 WHERE app = ?1",
            params![app, release, now_ms()],
        )?;
        Ok(())
    }

    /// Add model tokens to a draft's spend; returns the new total.
    pub fn add_draft_spend(&self, app: &str, tokens: i64) -> Result<i64> {
        let conn = self.conn()?;
        conn.execute("UPDATE drafts SET spent = spent + ?2 WHERE app = ?1", params![app, tokens])?;
        Ok(conn.query_row("SELECT spent FROM drafts WHERE app = ?1", [app], |r| r.get(0))?)
    }

    pub fn add_proposal(&self, p: &NewProposal<'_>) -> Result<i64> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO proposals (app, round, source, model, lease, files, generate, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                p.app,
                p.round,
                p.source,
                p.model,
                p.lease,
                p.files.to_string(),
                p.generate.to_string(),
                p.notes,
                now_ms()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_proposal_verdict(&self, id: i64, verdict: &str) -> Result<()> {
        let changed = self.conn()?.execute("UPDATE proposals SET verdict = ?2 WHERE id = ?1", params![id, verdict])?;
        if changed == 0 {
            return Err(Error::NotFound { kind: "proposal", id });
        }
        Ok(())
    }

    /// The newest proposals for an app, newest first.
    pub fn proposals(&self, app: &str, limit: usize) -> Result<Vec<Proposal>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare(&format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE app = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![app, limit as i64], proposal_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replace every overlay file of an app.
    pub fn set_overlays(&self, app: &str, files: &[(String, String)]) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM overlays WHERE app = ?1", [app])?;
        for (path, content) in files {
            tx.execute("INSERT INTO overlays (app, path, content) VALUES (?1, ?2, ?3)", params![app, path, content])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn overlays(&self, app: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT path, content FROM overlays WHERE app = ?1 ORDER BY path")?;
        let rows = stmt.query_map([app], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn get_draft(conn: &rusqlite::Connection, app: &str) -> Result<Option<Draft>> {
    conn.query_row(&format!("SELECT {DRAFT_COLS} FROM drafts WHERE app = ?1"), [app], draft_row).optional()?.transpose()
}

fn draft_row(r: &Row<'_>) -> rusqlite::Result<Result<Draft>> {
    let stage: String = r.get(4)?;
    Ok((|| {
        Ok(Draft {
            app: r.get(0)?,
            repo: r.get(1)?,
            branch: r.get(2)?,
            hostname: r.get(3)?,
            stage: DraftStage::parse(&stage).ok_or(Error::Corrupt("draft stage", stage.clone()))?,
            round: r.get(5)?,
            spent: r.get(6)?,
            detail: r.get(7)?,
            release: r.get(8)?,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
        })
    })())
}

fn proposal_row(r: &Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: r.get(0)?,
        app: r.get(1)?,
        round: r.get(2)?,
        source: r.get(3)?,
        model: r.get(4)?,
        lease: r.get(5)?,
        files: json(r.get(6)?),
        generate: json(r.get(7)?),
        notes: r.get(8)?,
        verdict: r.get(9)?,
        created_at: r.get(10)?,
    })
}
