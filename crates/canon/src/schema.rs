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
    // 3: Proof's progress through a release's trial, so a soak cut off by a
    // reboot resumes rather than restarts. `state` is Proof's own JSON.
    "CREATE TABLE proofs (
        release    INTEGER PRIMARY KEY REFERENCES releases(id) ON DELETE CASCADE,
        app        TEXT NOT NULL,
        state      TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;",
    // 4: onboarding. A draft exists before its app does: it is the record of
    // getting a repo from URL to Live. Proposals are every plan Draft
    // weighed, Sound's or Ghost's, kept so the UI can show each as a diff.
    // Overlays are files Henry adds to an app's checkout, such as a
    // Dockerfile Ghost wrote, since Henry never commits to your repo.
    "CREATE TABLE drafts (
        app        TEXT PRIMARY KEY,
        repo       TEXT NOT NULL,
        branch     TEXT,
        hostname   TEXT,
        stage      TEXT NOT NULL,
        round      INTEGER NOT NULL DEFAULT 0,
        spent      INTEGER NOT NULL DEFAULT 0,
        detail     TEXT,
        release    INTEGER,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE proposals (
        id         INTEGER PRIMARY KEY,
        app        TEXT NOT NULL,
        round      INTEGER NOT NULL,
        source     TEXT NOT NULL,
        model      TEXT,
        lease      TEXT NOT NULL,
        files      TEXT NOT NULL,
        generate   TEXT NOT NULL,
        notes      TEXT NOT NULL,
        verdict    TEXT,
        created_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX proposals_by_app ON proposals (app, id DESC);

    CREATE TABLE overlays (
        app     TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
        path    TEXT NOT NULL,
        content TEXT NOT NULL,
        PRIMARY KEY (app, path)
    ) STRICT;",
    // 5: Ark's record of every backup and restore drill. `target` is
    // `service/volume`, or `canon` for Henry's own state (app `henry`).
    // No foreign key: a removed app's backups still exist in the repository.
    "CREATE TABLE backups (
        id       INTEGER PRIMARY KEY,
        app      TEXT NOT NULL,
        target   TEXT NOT NULL,
        snapshot TEXT,
        bytes    INTEGER,
        ok       INTEGER NOT NULL,
        detail   TEXT,
        at       INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX backups_by_target ON backups (app, target, id DESC);

    CREATE TABLE drills (
        id       INTEGER PRIMARY KEY,
        app      TEXT NOT NULL,
        target   TEXT NOT NULL,
        snapshot TEXT NOT NULL,
        ok       INTEGER NOT NULL,
        detail   TEXT,
        at       INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX drills_by_target ON drills (app, target, id DESC);",
    // 6: Mend's upkeep. One row per task: when it last ran and how it went.
    // `tools` is each managed binary's version, with the one it replaced kept
    // for rollback and `pending` set while a switch is being proven. `bench`
    // is every model eval run.
    "CREATE TABLE upkeep (
        task    TEXT PRIMARY KEY,
        last_at INTEGER NOT NULL,
        ok      INTEGER NOT NULL,
        detail  TEXT
    ) STRICT;

    CREATE TABLE tools (
        name       TEXT PRIMARY KEY,
        current    TEXT NOT NULL,
        previous   TEXT,
        pending    TEXT,
        updated_at INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE bench (
        id     INTEGER PRIMARY KEY,
        model  TEXT NOT NULL,
        passed INTEGER NOT NULL,
        total  INTEGER NOT NULL,
        tokens INTEGER NOT NULL,
        detail TEXT NOT NULL,
        at     INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX bench_by_model ON bench (model, id DESC);",
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
