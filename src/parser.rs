#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct RawEnvelope {
    timestamp: i64,
    data: RawData,
}

#[derive(Debug, Deserialize)]
struct RawData {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(rename = "modelIdentifier")]
    model_identifier: Option<String>,
    stats: Option<PredictionStats>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PredictionStats {
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(rename = "tokensPerSecond")]
    pub tokens_per_second: f64,
    #[serde(rename = "numGpuLayers")]
    pub num_gpu_layers: Option<i32>,
    #[serde(rename = "timeToFirstTokenSec")]
    pub time_to_first_token_sec: f64,
    #[serde(rename = "totalTimeSec")]
    pub total_time_sec: f64,
    #[serde(rename = "promptTokensCount")]
    pub prompt_tokens_count: u64,
    #[serde(rename = "predictedTokensCount")]
    pub predicted_tokens_count: u64,
    #[serde(rename = "totalTokensCount")]
    pub total_tokens_count: u64,
}

#[derive(Debug, Clone)]
pub enum LogEvent {
    PredictionInput {
        model_id: String,
        timestamp_ms: i64,
    },
    PredictionOutput {
        model_id: String,
        timestamp_ms: i64,
        stats: PredictionStats,
    },
    Other(String),
}

#[derive(Debug, Clone)]
pub struct InferenceRecord {
    pub model_id: String,
    pub started_at: DateTime<Utc>,
    pub prompt_tokens: u64,
    pub gen_tokens: u64,
    pub ttft_ms: f64,
    pub gen_ms: f64,
    pub total_ms: f64,
    pub tokens_per_second: f64,
    pub stop_reason: Option<String>,
    pub num_gpu_layers: Option<i32>,
}

pub fn parse_line(line: &str) -> LogEvent {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LogEvent::Other(String::new());
    }
    let env: RawEnvelope = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return LogEvent::Other(trimmed.to_string()),
    };
    let model_id = match env.data.model_identifier.clone() {
        Some(m) => m,
        None => return LogEvent::Other(trimmed.to_string()),
    };
    match env.data.event_type.as_str() {
        "llm.prediction.input" => LogEvent::PredictionInput {
            model_id,
            timestamp_ms: env.timestamp,
        },
        "llm.prediction.output" => match env.data.stats {
            Some(stats) => LogEvent::PredictionOutput {
                model_id,
                timestamp_ms: env.timestamp,
                stats,
            },
            None => LogEvent::Other(trimmed.to_string()),
        },
        _ => LogEvent::Other(trimmed.to_string()),
    }
}

#[derive(Debug)]
struct PendingInput {
    timestamp_ms: i64,
}

pub struct RecordBuilder {
    pending: HashMap<String, VecDeque<PendingInput>>,
    pub stale_after: Duration,
}

impl Default for RecordBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordBuilder {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            stale_after: Duration::from_secs(300),
        }
    }

    pub fn ingest(&mut self, event: LogEvent) -> Option<InferenceRecord> {
        match event {
            LogEvent::PredictionInput {
                model_id,
                timestamp_ms,
            } => {
                self.pending
                    .entry(model_id)
                    .or_default()
                    .push_back(PendingInput { timestamp_ms });
                None
            }
            LogEvent::PredictionOutput {
                model_id,
                timestamp_ms,
                stats,
            } => {
                let ttft_ms = stats.time_to_first_token_sec * 1000.0;
                let total_ms = stats.total_time_sec * 1000.0;
                let gen_ms = if stats.tokens_per_second > 0.0 {
                    (stats.predicted_tokens_count as f64) / stats.tokens_per_second * 1000.0
                } else {
                    total_ms
                };
                let expected_wall_ms = (ttft_ms + gen_ms) as i64;
                let max_acceptable_wall_ms = (expected_wall_ms * 2).max(5_000) + 1_000;
                let started_ms = self
                    .match_input(&model_id, timestamp_ms, max_acceptable_wall_ms)
                    .unwrap_or(timestamp_ms);
                let started_at = Utc
                    .timestamp_millis_opt(started_ms)
                    .single()
                    .unwrap_or_else(Utc::now);
                Some(InferenceRecord {
                    model_id,
                    started_at,
                    prompt_tokens: stats.prompt_tokens_count,
                    gen_tokens: stats.predicted_tokens_count,
                    ttft_ms,
                    gen_ms,
                    total_ms,
                    tokens_per_second: stats.tokens_per_second,
                    stop_reason: stats.stop_reason,
                    num_gpu_layers: stats.num_gpu_layers,
                })
            }
            LogEvent::Other(_) => None,
        }
    }

    fn match_input(
        &mut self,
        model_id: &str,
        output_ts_ms: i64,
        max_wall_ms: i64,
    ) -> Option<i64> {
        let queue = self.pending.get_mut(model_id)?;
        while let Some(front) = queue.front() {
            let wall = output_ts_ms - front.timestamp_ms;
            if wall <= max_wall_ms {
                return queue.pop_front().map(|p| p.timestamp_ms);
            }
            queue.pop_front();
        }
        None
    }

    pub fn evict_stale(&mut self, now_ms: i64) {
        let cutoff_ms = now_ms - self.stale_after.as_millis() as i64;
        for q in self.pending.values_mut() {
            while q.front().is_some_and(|p| p.timestamp_ms < cutoff_ms) {
                q.pop_front();
            }
        }
        self.pending.retain(|_, q| !q.is_empty());
    }

    pub fn pending_count(&self, model_id: &str) -> usize {
        self.pending.get(model_id).map_or(0, |q| q.len())
    }
}

pub async fn parser_task(
    mut line_rx: mpsc::Receiver<String>,
    record_tx: mpsc::Sender<InferenceRecord>,
) {
    let mut builder = RecordBuilder::new();
    let mut evict = tokio::time::interval(Duration::from_secs(30));
    evict.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            line = line_rx.recv() => {
                match line {
                    Some(line) => {
                        let event = parse_line(&line);
                        if let Some(record) = builder.ingest(event) {
                            if record_tx.send(record).await.is_err() {
                                return;
                            }
                        }
                    }
                    None => return,
                }
            }
            _ = evict.tick() => {
                builder.evict_stale(Utc::now().timestamp_millis());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/lms-log-source-model-stats-mlx.jsonl");

    #[test]
    fn classifies_each_fixture_line() {
        let mut input = 0;
        let mut output = 0;
        let mut other = 0;
        for line in FIXTURE.lines() {
            match parse_line(line) {
                LogEvent::PredictionInput { .. } => input += 1,
                LogEvent::PredictionOutput { .. } => output += 1,
                LogEvent::Other(_) => other += 1,
            }
        }
        assert_eq!(input, 3);
        assert_eq!(output, 2);
        assert_eq!(other, 2); // header line + blank line
    }

    #[test]
    fn record_builder_pairs_correctly_skipping_orphan() {
        let mut builder = RecordBuilder::new();
        let mut records = Vec::new();
        for line in FIXTURE.lines() {
            if let Some(rec) = builder.ingest(parse_line(line)) {
                records.push(rec);
            }
        }

        assert_eq!(records.len(), 2);

        let r1 = &records[0];
        assert_eq!(r1.model_id, "qwen3.6-35b-a3b-ud-mlx");
        assert_eq!(r1.prompt_tokens, 20);
        assert_eq!(r1.gen_tokens, 99);
        assert!((r1.tokens_per_second - 96.482_412_060_301_51).abs() < 1e-6);
        assert!((r1.ttft_ms - 330.0).abs() < 0.5);
        assert!((r1.total_ms - 995.0).abs() < 0.5);
        assert_eq!(r1.stop_reason.as_deref(), Some("maxPredictedTokensReached"));
        // gen_ms = 99 / 96.482 * 1000 ≈ 1026.1 ms
        assert!((r1.gen_ms - 1026.1).abs() < 1.0);
        // started_at must come from req-2 input (t=1777939641468), not the orphan req-1 input (t=1777939632760)
        assert_eq!(r1.started_at.timestamp_millis(), 1_777_939_641_468);

        let r2 = &records[1];
        assert_eq!(r2.gen_tokens, 129);
        assert!((r2.tokens_per_second - 93.632_958_801_498_13).abs() < 1e-6);
        assert_eq!(r2.started_at.timestamp_millis(), 1_777_939_642_812);

        // After processing, queue is empty: orphan was discarded during r1's matching.
        assert_eq!(builder.pending_count("qwen3.6-35b-a3b-ud-mlx"), 0);
    }

    #[test]
    fn unknown_line_is_other_no_panic() {
        let line = "this is not json gibberish !@#$";
        match parse_line(line) {
            LogEvent::Other(s) => assert_eq!(s, line),
            other => panic!("expected Other, got {:?}", other),
        }
    }

    #[test]
    fn header_line_is_other() {
        assert!(matches!(
            parse_line("Streaming logs from LM Studio"),
            LogEvent::Other(_)
        ));
    }

    #[test]
    fn empty_line_is_other() {
        assert!(matches!(parse_line(""), LogEvent::Other(_)));
    }

    #[test]
    fn evict_stale_drops_old_pending() {
        let mut builder = RecordBuilder::new();
        builder.stale_after = Duration::from_millis(100);
        builder.ingest(LogEvent::PredictionInput {
            model_id: "m".into(),
            timestamp_ms: 1_000,
        });
        builder.ingest(LogEvent::PredictionInput {
            model_id: "m".into(),
            timestamp_ms: 1_500,
        });
        builder.evict_stale(1_200); // anything before t=1100 is stale
        assert_eq!(builder.pending_count("m"), 1);
    }

    #[test]
    fn output_with_no_pending_uses_output_ts_as_started_at() {
        let mut builder = RecordBuilder::new();
        let stats = PredictionStats {
            stop_reason: None,
            tokens_per_second: 50.0,
            num_gpu_layers: Some(-1),
            time_to_first_token_sec: 0.1,
            total_time_sec: 1.0,
            prompt_tokens_count: 10,
            predicted_tokens_count: 50,
            total_tokens_count: 60,
        };
        let rec = builder
            .ingest(LogEvent::PredictionOutput {
                model_id: "m".into(),
                timestamp_ms: 5_000,
                stats,
            })
            .unwrap();
        assert_eq!(rec.started_at.timestamp_millis(), 5_000);
    }
}
