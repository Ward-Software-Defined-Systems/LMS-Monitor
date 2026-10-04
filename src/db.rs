#![allow(dead_code)]

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use tokio::sync::mpsc;

use crate::parser::InferenceRecord;

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

pub fn open_or_create(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create db parent {}", parent.display()))?;
    }
    let conn =
        Connection::open(path).with_context(|| format!("open sqlite at {}", path.display()))?;
    conn.execute_batch(SCHEMA_SQL).context("apply schema")?;
    Ok(conn)
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
            ttft_ms, gen_ms, total_ms, tokens_per_second, raw_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
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

    #[test]
    fn schema_apply_is_idempotent() {
        let path = temp_db_path();
        let _conn1 = open_or_create(&path).unwrap();
        let _conn2 = open_or_create(&path).unwrap();
        let conn3 = open_or_create(&path).unwrap();
        let version: i64 = conn3
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
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
}
