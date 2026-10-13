//! Migrations. Each entry runs once, in order, inside a transaction, and
//! `PRAGMA user_version` records how far the file has come. Never edit a
//! shipped entry; append a new one.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: the first record.
    "CREATE TABLE apps (
        name          TEXT PRIMARY KEY,
        repo          TEXT NOT NULL,
        lease       TEXT NOT NULL,
        source        TEXT NOT NULL,
        desired       TEXT NOT NULL,
        status        TEXT NOT NULL,
        status_detail TEXT,
        live_release  INTEGER,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE routes (
        hostname TEXT NOT NULL,
        path     TEXT NOT NULL,
        app      TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
        service  TEXT NOT NULL,
        PRIMARY KEY (hostname, path)
    ) STRICT;

    CREATE TABLE releases (
        id          INTEGER PRIMARY KEY,
        app         TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
        commit_sha  TEXT NOT NULL,
        lease     TEXT NOT NULL,
        state       TEXT NOT NULL,
        images      TEXT NOT NULL DEFAULT '{}',
        reason      TEXT,
        created_at  INTEGER NOT NULL,
        finished_at INTEGER
    ) STRICT;
    CREATE INDEX releases_by_app ON releases (app, id DESC);

    CREATE TABLE jobs (
        id         INTEGER PRIMARY KEY,
        kind       TEXT NOT NULL,
        app        TEXT,
        state      TEXT NOT NULL,
        attempts   INTEGER NOT NULL DEFAULT 0,
        payload    TEXT NOT NULL DEFAULT '{}',
        error      TEXT,
        not_before INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX jobs_due ON jobs (state, not_before, id);

    CREATE TABLE events (
        id      INTEGER PRIMARY KEY,
        at      INTEGER NOT NULL,
        app     TEXT,
        kind    TEXT NOT NULL,
        message TEXT NOT NULL,
        data    TEXT NOT NULL DEFAULT '{}'
    ) STRICT;
    CREATE INDEX events_by_app ON events (app, id DESC);

    CREATE TABLE secrets (
        scope      TEXT NOT NULL,
        name       TEXT NOT NULL,
        sealed     BLOB NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (scope, name)
    ) STRICT;",
    // 2: host ports Plumb publishes containers on, for Hinge to route to.
    // `slot` is the release id for a stateless service, 0 for a stateful one.
    "CREATE TABLE ports (
        app       TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
        service   TEXT NOT NULL,
        slot      INTEGER NOT NULL,
        host_port INTEGER NOT NULL UNIQUE,
        PRIMARY KEY (app, service, slot)
    ) STRICT;",
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

pub const LATEST: i64 = MIGRATIONS.len() as i64;
