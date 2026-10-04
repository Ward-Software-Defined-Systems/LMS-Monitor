#![allow(dead_code)]

use std::collections::HashMap;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::parser::InferenceRecord;

#[derive(Debug, Default, Clone)]
pub struct WindowMetrics {
    pub req_count: u64,
    pub total_prompt_tokens: u64,
    pub total_gen_tokens: u64,
    pub mean_tps: f64,
    pub p50_tps: f64,
    pub p95_tps: f64,
    pub mean_ttft_ms: f64,
}

#[derive(Debug, Default, Clone)]
pub struct ModelMetrics {
    pub window_1m: WindowMetrics,
    pub window_5m: WindowMetrics,
    pub window_15m: WindowMetrics,
    pub session_lifetime: WindowMetrics,
}

#[derive(Debug, Clone)]
pub struct AggregateSnapshot {
    pub window_1m: WindowMetrics,
    pub window_5m: WindowMetrics,
    pub window_15m: WindowMetrics,
    pub session_lifetime: WindowMetrics,
    pub per_model: HashMap<String, ModelMetrics>,
    pub session_started_at: DateTime<Utc>,
}

pub struct Aggregator {
    records: Vec<InferenceRecord>,
    session_started_at: DateTime<Utc>,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Aggregator {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            session_started_at: Utc::now(),
        }
    }

    pub fn ingest(&mut self, rec: InferenceRecord) {
        self.records.push(rec);
    }

    pub fn reset_session(&mut self) {
        self.records.clear();
        self.session_started_at = Utc::now();
    }

    pub fn snapshot(&self) -> AggregateSnapshot {
        self.snapshot_at(Utc::now())
    }

    fn snapshot_at(&self, now: DateTime<Utc>) -> AggregateSnapshot {
        let cutoff_1m = now - ChronoDuration::seconds(60);
        let cutoff_5m = now - ChronoDuration::seconds(300);
        let cutoff_15m = now - ChronoDuration::seconds(900);

        let recs_1m: Vec<&InferenceRecord> = self
            .records
            .iter()
            .filter(|r| r.started_at >= cutoff_1m)
            .collect();
        let recs_5m: Vec<&InferenceRecord> = self
            .records
            .iter()
            .filter(|r| r.started_at >= cutoff_5m)
            .collect();
        let recs_15m: Vec<&InferenceRecord> = self
            .records
            .iter()
            .filter(|r| r.started_at >= cutoff_15m)
            .collect();
        let recs_lifetime: Vec<&InferenceRecord> = self.records.iter().collect();

        let mut per_model_records: HashMap<String, Vec<&InferenceRecord>> = HashMap::new();
        for rec in &self.records {
            per_model_records
                .entry(rec.model_id.clone())
                .or_default()
                .push(rec);
        }

        let mut per_model = HashMap::new();
        for (model_id, recs) in per_model_records {
            let m1m: Vec<&InferenceRecord> = recs
                .iter()
                .copied()
                .filter(|r| r.started_at >= cutoff_1m)
                .collect();
            let m5m: Vec<&InferenceRecord> = recs
                .iter()
                .copied()
                .filter(|r| r.started_at >= cutoff_5m)
                .collect();
            let m15m: Vec<&InferenceRecord> = recs
                .iter()
                .copied()
                .filter(|r| r.started_at >= cutoff_15m)
                .collect();
            per_model.insert(
                model_id,
                ModelMetrics {
                    window_1m: compute(&m1m),
                    window_5m: compute(&m5m),
                    window_15m: compute(&m15m),
                    session_lifetime: compute(&recs),
                },
            );
        }

        AggregateSnapshot {
            window_1m: compute(&recs_1m),
            window_5m: compute(&recs_5m),
            window_15m: compute(&recs_15m),
            session_lifetime: compute(&recs_lifetime),
            per_model,
            session_started_at: self.session_started_at,
        }
    }
}

fn compute(records: &[&InferenceRecord]) -> WindowMetrics {
    if records.is_empty() {
        return WindowMetrics::default();
    }
    let mut total_prompt_tokens = 0u64;
    let mut total_gen_tokens = 0u64;
    let mut sum_tps = 0.0;
    let mut sum_ttft = 0.0;
    let mut tps_vals: Vec<f64> = Vec::with_capacity(records.len());
    for r in records {
        total_prompt_tokens += r.prompt_tokens;
        total_gen_tokens += r.gen_tokens;
        sum_tps += r.tokens_per_second;
        sum_ttft += r.ttft_ms;
        tps_vals.push(r.tokens_per_second);
    }
    let n = records.len() as f64;
    tps_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    WindowMetrics {
        req_count: records.len() as u64,
        total_prompt_tokens,
        total_gen_tokens,
        mean_tps: sum_tps / n,
        p50_tps: percentile(&tps_vals, 50),
        p95_tps: percentile(&tps_vals, 95),
        mean_ttft_ms: sum_ttft / n,
    }
}

fn percentile(sorted: &[f64], p: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p as f64 / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        started_at: DateTime<Utc>,
        model: &str,
        prompt: u64,
        out_tokens: u64,
        tps: f64,
    ) -> InferenceRecord {
        InferenceRecord {
            model_id: model.into(),
            started_at,
            prompt_tokens: prompt,
            gen_tokens: out_tokens,
            ttft_ms: 100.0,
            gen_ms: out_tokens as f64 / tps * 1000.0,
            total_ms: 100.0 + out_tokens as f64 / tps * 1000.0,
            tokens_per_second: tps,
            stop_reason: None,
            num_gpu_layers: None,
        }
    }

    #[test]
    fn empty_aggregator_zero_snapshot() {
        let agg = Aggregator::new();
        let snap = agg.snapshot();
        assert_eq!(snap.window_1m.req_count, 0);
        assert_eq!(snap.session_lifetime.req_count, 0);
        assert!(snap.per_model.is_empty());
    }

    #[test]
    fn synthetic_100_records_lifetime_totals() {
        let now = Utc::now();
        let mut agg = Aggregator::new();
        for i in 0..100u64 {
            agg.ingest(rec(now, "m1", 10, 50, 80.0 + i as f64));
        }
        let snap = agg.snapshot_at(now);
        assert_eq!(snap.session_lifetime.req_count, 100);
        assert_eq!(snap.session_lifetime.total_prompt_tokens, 1000);
        assert_eq!(snap.session_lifetime.total_gen_tokens, 5000);
        // mean of 80..=179 = 129.5
        assert!((snap.session_lifetime.mean_tps - 129.5).abs() < 0.01);
        // p50: idx round(0.5 * 99) = 50 -> value 130
        assert!((snap.session_lifetime.p50_tps - 130.0).abs() < 0.5);
        // p95: idx round(0.95 * 99) = 94 -> value 174
        assert!((snap.session_lifetime.p95_tps - 174.0).abs() < 0.5);
    }

    #[test]
    fn rolling_windows_filter_by_age() {
        let now = Utc::now();
        let mut agg = Aggregator::new();
        agg.ingest(rec(now, "m", 10, 50, 100.0)); // in 1m
        agg.ingest(rec(now - ChronoDuration::seconds(120), "m", 10, 50, 100.0)); // in 5m
        agg.ingest(rec(now - ChronoDuration::seconds(600), "m", 10, 50, 100.0)); // in 15m
        agg.ingest(rec(
            now - ChronoDuration::seconds(1_800),
            "m",
            10,
            50,
            100.0,
        )); // only lifetime

        let snap = agg.snapshot_at(now);
        assert_eq!(snap.window_1m.req_count, 1);
        assert_eq!(snap.window_5m.req_count, 2);
        assert_eq!(snap.window_15m.req_count, 3);
        assert_eq!(snap.session_lifetime.req_count, 4);
    }

    #[test]
    fn per_model_breakdown() {
        let now = Utc::now();
        let mut agg = Aggregator::new();
        agg.ingest(rec(now, "m1", 10, 50, 100.0));
        agg.ingest(rec(now, "m1", 10, 50, 200.0));
        agg.ingest(rec(now, "m2", 20, 100, 150.0));
        let snap = agg.snapshot_at(now);
        assert_eq!(snap.per_model.len(), 2);
        let m1 = &snap.per_model["m1"];
        assert_eq!(m1.session_lifetime.req_count, 2);
        assert_eq!(m1.session_lifetime.total_prompt_tokens, 20);
        assert_eq!(m1.session_lifetime.total_gen_tokens, 100);
        assert!((m1.session_lifetime.mean_tps - 150.0).abs() < 0.01);
        let m2 = &snap.per_model["m2"];
        assert_eq!(m2.session_lifetime.req_count, 1);
        assert_eq!(m2.session_lifetime.total_gen_tokens, 100);
    }

    #[test]
    fn reset_session_clears_records() {
        let now = Utc::now();
        let mut agg = Aggregator::new();
        agg.ingest(rec(now, "m", 10, 50, 100.0));
        assert_eq!(agg.snapshot_at(now).session_lifetime.req_count, 1);
        agg.reset_session();
        assert_eq!(agg.snapshot_at(now).session_lifetime.req_count, 0);
    }
}
