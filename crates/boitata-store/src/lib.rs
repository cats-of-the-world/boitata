//! Embedded persistence for boitata, backed by SQLite (via `rusqlite`, bundled).
//!
//! [`Store`] owns a single SQLite connection behind a mutex and exposes async
//! methods; each runs its query on a blocking worker (`spawn_blocking`) so the
//! synchronous `rusqlite` API never stalls the tokio runtime. Writes here are
//! small and infrequent (a blueprint checkpoint per super-step), so a single
//! serialized connection is plenty.
//!
//! The database is opened in WAL mode so a reader (e.g. a `runs` listing) never
//! blocks the writer. Schema changes are applied by [`migrate`] using SQLite's
//! `user_version`, so opening an older database upgrades it in place.
//!
//! This crate deliberately knows nothing about blueprint types: a checkpoint's
//! graph state is stored as an opaque JSON string the caller serializes. That
//! keeps the persistence layer a shared foundation other features can grow into
//! (their own tables + migrations) without depending on the orchestrator.

mod checkpoints;

pub use checkpoints::{CheckpointRecord, CheckpointUpsert, RunState};

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;

/// A handle to the SQLite-backed store. Cheap to clone — clones share one
/// connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Ordered schema migrations. Each entry is one schema version; the index+1 is
/// the `user_version` it advances the database to. Only ever append — never edit
/// or reorder an existing entry, or databases in the field won't match.
const MIGRATIONS: &[&str] = &[
    // v1: blueprint run checkpoints, one row per run keyed by run id. `state` and
    // `frontier` are opaque JSON the orchestrator (de)serializes; `status` tracks
    // whether the run is resumable. Timestamps are RFC 3339 text.
    "CREATE TABLE checkpoints (
        run_id     TEXT PRIMARY KEY,
        blueprint  TEXT NOT NULL,
        task       TEXT NOT NULL,
        step       INTEGER NOT NULL,
        frontier   TEXT NOT NULL,
        state      TEXT NOT NULL,
        status     TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX checkpoints_status ON checkpoints(status);",
];

impl Store {
    /// Open (creating if absent) the store at `path`, enabling WAL and applying
    /// any pending migrations. The parent directory must already exist.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open store database at {}", path.display()))?;
        Self::init(conn)
    }

    /// Open an in-memory store (each call is a fresh, private database). For tests
    /// and ephemeral use.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory().context("failed to open in-memory store")?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Self> {
        // WAL lets a reader run concurrently with the writer; NORMAL sync is the
        // usual WAL pairing (durable across app crashes, may lose only the last
        // txn on OS crash — fine for resumable checkpoints).
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("failed to set synchronous mode")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure with exclusive access to the connection on a blocking worker,
    /// so the synchronous `rusqlite` calls don't block the async runtime.
    pub(crate) async fn call<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            // A poisoned mutex (a prior closure panicked mid-query) surfaces as a
            // recoverable error rather than cascading panics on every later call.
            let guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("store connection mutex poisoned: {e}"))?;
            f(&guard)
        })
        .await
        .context("store worker task failed")?
    }
}

/// Apply every migration past the database's current `user_version`, advancing
/// the version after each so re-opening is idempotent.
///
/// Each migration runs in its own transaction so the DDL and the `user_version`
/// bump commit atomically: a crash between them can't leave the schema applied
/// but the version stale (which would re-run the migration and fail on next open).
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let current: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .context("failed to read schema version")?;
    let current = current as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn
            .transaction()
            .with_context(|| format!("failed to begin migration v{}", i + 1))?;
        tx.execute_batch(sql)
            .with_context(|| format!("failed to apply migration v{}", i + 1))?;
        // `user_version` doesn't accept bind params; the value is a trusted index.
        tx.pragma_update(None, "user_version", (i + 1) as i64)
            .with_context(|| format!("failed to record schema version v{}", i + 1))?;
        tx.commit()
            .with_context(|| format!("failed to commit migration v{}", i + 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_bring_a_fresh_db_to_current_version() {
        let store = Store::open_in_memory().unwrap();
        let version = store
            .call(|conn| {
                Ok(conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
    }

    #[tokio::test]
    async fn reopening_an_existing_db_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        // Open, close, reopen — migrate must not error on the already-current db.
        Store::open(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let version = store
            .call(|conn| {
                Ok(conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
    }
}
