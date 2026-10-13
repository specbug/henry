//! Apps moved onto Henry from hostd: where their data came from, and the
//! DNS record their cutover replaced, so it can be put back.

use rusqlite::{OptionalExtension, params};

use crate::{Canon, Result, now_ms, record};

#[derive(Debug, Clone, PartialEq)]
pub struct Adoption {
    pub app: String,
    /// hostd's volume, e.g. `odyssey-data`.
    pub source_volume: Option<String>,
    /// The lease volume it was copied into.
    pub volume: Option<String>,
    pub seeded_at: Option<i64>,
    pub cutover_at: Option<i64>,
    /// The DNS record the cutover replaced (`null` when it made a new one).
    pub dns: Option<serde_json::Value>,
}

impl Canon {
    /// Note an adoption, or what it copies from. Running it again keeps the
    /// cutover, if one happened.
    pub fn put_adoption(&self, app: &str, source_volume: Option<&str>, volume: Option<&str>) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO adoptions (app, source_volume, volume, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (app) DO UPDATE SET source_volume = excluded.source_volume, volume = excluded.volume",
            params![app, source_volume, volume, now_ms()],
        )?;
        Ok(())
    }

    pub fn adoption(&self, app: &str) -> Result<Option<Adoption>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT app, source_volume, volume, seeded_at, cutover_at, dns FROM adoptions WHERE app = ?1",
                [app],
                |r| {
                    Ok(Adoption {
                        app: r.get(0)?,
                        source_volume: r.get(1)?,
                        volume: r.get(2)?,
                        seeded_at: r.get(3)?,
                        cutover_at: r.get(4)?,
                        dns: r.get::<_, Option<String>>(5)?.map(crate::json),
                    })
                },
            )
            .optional()?)
    }

    /// The data was copied in, `files` of them.
    pub fn set_seeded(&self, app: &str, files: u64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("UPDATE adoptions SET seeded_at = ?2 WHERE app = ?1", params![app, now_ms()])?;
        record(
            &conn,
            Some(app),
            "adopt.seeded",
            &format!("copied {files} file(s) from hostd's volume"),
            &serde_json::Value::Null,
        )
    }

    /// The hostname now points at Henry; `old` is the record it replaced.
    pub fn set_cutover(&self, app: &str, old: Option<&serde_json::Value>) -> Result<()> {
        let conn = self.conn()?;
        let old = old.map(|v| v.to_string());
        conn.execute("UPDATE adoptions SET cutover_at = ?2, dns = ?3 WHERE app = ?1", params![app, now_ms(), old])?;
        record(&conn, Some(app), "adopt.cutover", "public traffic now comes to Henry", &serde_json::Value::Null)
    }

    /// The cutover was undone.
    pub fn clear_cutover(&self, app: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("UPDATE adoptions SET cutover_at = NULL, dns = NULL WHERE app = ?1", [app])?;
        record(&conn, Some(app), "adopt.back", "public traffic handed back to hostd", &serde_json::Value::Null)
    }
}
