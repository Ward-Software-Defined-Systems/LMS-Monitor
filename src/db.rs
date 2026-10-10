#![allow(dead_code)]

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior, params};
use tokio::sync::mpsc;

use crate::parser::InferenceRecord;
use crate::server_log::ContextRejection;

/// Schema v1, frozen. Older binaries apply exactly this on every open, and
/// `migrates_v1_and_old_binary_keeps_working` relies on that; later changes go in `migrate`.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE TABLE IF NOT EXISTS inference_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    started_at TEXT NOT NULL,
    model_id TEXT,
    prompt_tokens INTEGER NOT NULL,
    gen_tokens INTEGER NOT NULL,
    ttft_ms REAL NOT NULL,
    gen_ms REAL NOT NULL,
    total_ms REAL NOT NULL,
    tokens_per_second REAL NOT NULL,
    raw_json TEXT
);

CREATE INDEX IF NOT EXISTS idx_records_session ON inference_records(session_id);
CREATE INDEX IF NOT EXISTS idx_records_started ON inference_records(started_at);

CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);

INSERT OR IGNORE INTO schema_version (version) VALUES (1);
"#;

/// Schema v2 additions, applied by `migrate` along with `inference_records.stop_reason`.
const V2_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS context_rejections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    occurred_at TEXT NOT NULL,
    model_id TEXT NOT NULL,
    input_tokens INTEGER,
    context_length INTEGER
);

CREATE INDEX IF NOT EXISTS idx_rejections_occurred ON context_rejections(occurred_at);

INSERT OR IGNORE INTO schema_version (version) VALUES (2);
"#;

pub fn open_or_create(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create db parent {}", parent.display()))?;
    }
    let mut conn =
        Connection::open(path).with_context(|| format!("open sqlite at {}", path.display()))?;
    conn.execute_batch(SCHEMA_SQL).context("apply schema")?;
    migrate(&mut conn).context("migrate schema")?;
    Ok(conn)
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )
}

/// Brings a v1 database to v2: adds `inference_records.stop_reason` and the
/// `context_rejections` table.
///
/// An older monitor may be writing to the same file. Taking the write lock up front
/// (`IMMEDIATE`) makes SQLite wait out its busy timeout; a deferred transaction that read
/// first would fail the lock upgrade at once. The older monitor keeps working afterwards:
/// its `SCHEMA_SQL` is all no-ops on a v2 file, and its insert names its columns, leaving
/// `stop_reason` NULL.
fn migrate(conn: &mut Connection) -> Result<()> {
    if schema_version(conn)? >= 2 {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have migrated while this one waited for the lock.
    if schema_version(&tx)? < 2 {
        let has_stop_reason: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('inference_records')
             WHERE name = 'stop_reason'",
            [],
            |r| r.get(0),
        )?;
        if has_stop_reason == 0 {
            tx.execute_batch("ALTER TABLE inference_records ADD COLUMN stop_reason TEXT")?;
        }
        tx.execute_batch(V2_SQL)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn start_session(conn: &Connection) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (started_at) VALUES (?1)",
        params![now],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn end_session(conn: &Connection, session_id: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
        params![now, session_id],
    )?;
    Ok(())
}

pub fn insert_record(conn: &Connection, session_id: i64, rec: &InferenceRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO inference_records (
            session_id, started_at, model_id, prompt_tokens, gen_tokens,
            ttft_ms, gen_ms, total_ms, tokens_per_second, raw_json, stop_reason
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
        params![
            session_id,
            rec.started_at.to_rfc3339(),
            rec.model_id,
            rec.prompt_tokens as i64,
            rec.gen_tokens as i64,
            rec.ttft_ms,
            rec.gen_ms,
            rec.total_ms,
            rec.tokens_per_second,
            rec.stop_reason,
        ],
    )?;
    Ok(())
}

pub fn insert_rejection(conn: &Connection, session_id: i64, rej: &ContextRejection) -> Result<()> {
    conn.execute(
        "INSERT INTO context_rejections (
            session_id, occurred_at, model_id, input_tokens, context_length
        ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            session_id,
            rej.at.to_rfc3339(),
            rej.model_id,
            rej.input_tokens.map(|n| n as i64),
            rej.context_length.map(|n| n as i64),
        ],
    )?;
    Ok(())
}

#[derive(Debug, Default, Clone)]
pub struct LifetimeTotals {
    pub session_count: u64,
    pub record_count: u64,
    pub total_prompt_tokens: u64,
    pub total_gen_tokens: u64,
}

pub fn lifetime_totals(conn: &Connection) -> Result<LifetimeTotals> {
    let session_count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let (record_count, total_prompt_tokens, total_gen_tokens): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(gen_tokens), 0)
         FROM inference_records",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(LifetimeTotals {
        session_count: session_count.max(0) as u64,
        record_count: record_count.max(0) as u64,
        total_prompt_tokens: total_prompt_tokens.max(0) as u64,
        total_gen_tokens: total_gen_tokens.max(0) as u64,
    })
}

#[derive(Debug)]
pub enum DbCommand {
    Insert(InferenceRecord),
    InsertRejection(ContextRejection),
    Shutdown,
}

#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::Sender<DbCommand>,
}

impl DbHandle {
    pub async fn insert(&self, rec: InferenceRecord) {
        let _ = self.tx.send(DbCommand::Insert(rec)).await;
    }

    pub async fn insert_rejection(&self, rej: ContextRejection) {
        let _ = self.tx.send(DbCommand::InsertRejection(rej)).await;
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(DbCommand::Shutdown).await;
    }
}

pub fn spawn_writer(conn: Connection, session_id: i64) -> DbHandle {
    let (tx, mut rx) = mpsc::channel::<DbCommand>(256);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                DbCommand::Insert(rec) => {
                    if let Err(e) = insert_record(&conn, session_id, &rec) {
                        tracing::warn!("db insert failed: {e:#}");
                    }
                }
                DbCommand::InsertRejection(rej) => {
                    if let Err(e) = insert_rejection(&conn, session_id, &rej) {
                        tracing::warn!("db insert of a context rejection failed: {e:#}");
                    }
                }
                DbCommand::Shutdown => {
                    if let Err(e) = end_session(&conn, session_id) {
                        tracing::warn!("db end_session on shutdown: {e:#}");
                    }
                    return;
                }
            }
        }
        // sender side dropped — treat as shutdown
        let _ = end_session(&conn, session_id);
    });
    DbHandle { tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("lmstudio-monitor-test-{pid}-{id}.db"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn rec(model: &str, prompt: u64, out_tokens: u64) -> InferenceRecord {
        InferenceRecord {
            model_id: model.into(),
            started_at: Utc::now(),
            prompt_tokens: prompt,
            gen_tokens: out_tokens,
            ttft_ms: 100.0,
            gen_ms: 1000.0,
            total_ms: 1100.0,
            tokens_per_second: 80.0,
            stop_reason: None,
            num_gpu_layers: None,
        }
    }

    #[test]
    fn ingest_50_records_then_restart_count_is_50() {
        let path = temp_db_path();
        {
            let conn = open_or_create(&path).unwrap();
            let sid = start_session(&conn).unwrap();
            for _ in 0..50 {
                insert_record(&conn, sid, &rec("m", 10, 50)).unwrap();
            }
            end_session(&conn, sid).unwrap();
        }
        {
            let conn = open_or_create(&path).unwrap();
            let totals = lifetime_totals(&conn).unwrap();
            assert_eq!(totals.record_count, 50);
            assert_eq!(totals.session_count, 1);
            assert_eq!(totals.total_prompt_tokens, 500);
            assert_eq!(totals.total_gen_tokens, 2_500);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lifetime_totals_aggregates_across_sessions() {
        let path = temp_db_path();
        for _session in 0..3 {
            let conn = open_or_create(&path).unwrap();
            let sid = start_session(&conn).unwrap();
            for _ in 0..10 {
                insert_record(&conn, sid, &rec("m", 5, 25)).unwrap();
            }
            end_session(&conn, sid).unwrap();
        }
        let conn = open_or_create(&path).unwrap();
        let totals = lifetime_totals(&conn).unwrap();
        assert_eq!(totals.session_count, 3);
        assert_eq!(totals.record_count, 30);
        assert_eq!(totals.total_prompt_tokens, 150);
        assert_eq!(totals.total_gen_tokens, 750);
        let _ = std::fs::remove_file(&path);
    }

    fn versions(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_version ORDER BY version")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn stop_reason_columns(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('inference_records')
             WHERE name = 'stop_reason'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn fresh_db_is_v2() {
        let path = temp_db_path();
        let conn = open_or_create(&path).unwrap();
        assert_eq!(versions(&conn), [1, 2]);
        assert_eq!(stop_reason_columns(&conn), 1);
        let rejections: i64 = conn
            .query_row("SELECT COUNT(*) FROM context_rejections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rejections, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_apply_is_idempotent() {
        let path = temp_db_path();
        let _conn1 = open_or_create(&path).unwrap();
        let _conn2 = open_or_create(&path).unwrap();
        let conn3 = open_or_create(&path).unwrap();
        assert_eq!(schema_version(&conn3).unwrap(), 2);
        assert_eq!(versions(&conn3), [1, 2]);
        assert_eq!(stop_reason_columns(&conn3), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// The insert a v1 binary runs, as it is in that binary.
    const V1_INSERT: &str = "INSERT INTO inference_records (
            session_id, started_at, model_id, prompt_tokens, gen_tokens,
            ttft_ms, gen_ms, total_ms, tokens_per_second, raw_json
        ) VALUES (?1, ?2, 'm', 10, 5, 1.0, 1.0, 2.0, 5.0, NULL)";

    /// A monitor built before v2 can be running against the same file while a newer one
    /// migrates it, and it has to keep working.
    #[test]
    fn migrates_v1_and_old_binary_keeps_working() {
        let path = temp_db_path();
        let old = Connection::open(&path).unwrap();
        old.execute_batch(SCHEMA_SQL).unwrap();
        let sid = start_session(&old).unwrap();
        let now = Utc::now().to_rfc3339();
        old.execute(V1_INSERT, params![sid, now]).unwrap();
        assert_eq!(versions(&old), [1]);

        let new = open_or_create(&path).unwrap();
        assert_eq!(versions(&new), [1, 2]);

        // The old connection stays open across the migration and keeps inserting, and its
        // schema setup is a no-op when it starts again.
        old.execute(V1_INSERT, params![sid, now]).unwrap();
        old.execute_batch(SCHEMA_SQL).unwrap();
        let (rows, with_stop): (i64, i64) = new
            .query_row(
                "SELECT COUNT(*), COUNT(stop_reason) FROM inference_records",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, with_stop), (2, 0));
        assert_eq!(versions(&new), [1, 2]);
        assert_eq!(stop_reason_columns(&new), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_opens_migrate_once() {
        let path = temp_db_path();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    open_or_create(&path).map(drop)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let conn = open_or_create(&path).unwrap();
        assert_eq!(versions(&conn), [1, 2]);
        assert_eq!(stop_reason_columns(&conn), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_record_persists_stop_reason() {
        let path = temp_db_path();
        let conn = open_or_create(&path).unwrap();
        let sid = start_session(&conn).unwrap();
        let mut full = rec("m", 10, 50);
        full.stop_reason = Some("contextLengthReached".into());
        insert_record(&conn, sid, &full).unwrap();
        insert_record(&conn, sid, &rec("m", 10, 50)).unwrap();
        let mut stmt = conn
            .prepare("SELECT stop_reason FROM inference_records ORDER BY id")
            .unwrap();
        let stops: Vec<Option<String>> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(stops, [Some("contextLengthReached".to_string()), None]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_db_lifetime_totals_zero() {
        let path = temp_db_path();
        let conn = open_or_create(&path).unwrap();
        let totals = lifetime_totals(&conn).unwrap();
        assert_eq!(totals.record_count, 0);
        assert_eq!(totals.session_count, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn writer_task_persists_inserts() {
        let path = temp_db_path();
        let conn = open_or_create(&path).unwrap();
        let sid = start_session(&conn).unwrap();
        let handle = spawn_writer(conn, sid);
        for _ in 0..25 {
            handle.insert(rec("m", 8, 40)).await;
        }
        handle.shutdown().await;
        // Give the writer task a moment to drain the shutdown.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let conn = open_or_create(&path).unwrap();
        let totals = lifetime_totals(&conn).unwrap();
        assert_eq!(totals.record_count, 25);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn writer_task_persists_rejections() {
        let path = temp_db_path();
        let conn = open_or_create(&path).unwrap();
        let sid = start_session(&conn).unwrap();
        let handle = spawn_writer(conn, sid);
        let rejection = |input, ctx| ContextRejection {
            model_id: "m".into(),
            at: Utc::now(),
            input_tokens: input,
            context_length: ctx,
        };
        handle
            .insert_rejection(rejection(Some(359_277), Some(262_144)))
            .await;
        handle.insert_rejection(rejection(None, None)).await;
        handle.shutdown().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let conn = open_or_create(&path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, input_tokens, context_length
                 FROM context_rejections ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(i64, Option<i64>, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            [(sid, Some(359_277), Some(262_144)), (sid, None, None)]
        );
        // A refused request never ran, so it isn't counted as one.
        assert_eq!(lifetime_totals(&conn).unwrap().record_count, 0);
        let _ = std::fs::remove_file(&path);
    }
}
