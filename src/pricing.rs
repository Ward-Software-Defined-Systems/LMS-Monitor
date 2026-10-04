#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::parser::InferenceRecord;

pub const FRONTIER_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "gemini-3-1-pro",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelPrice {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Provider {
    #[serde(default)]
    pub models: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PricingTable {
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
}

impl PricingTable {
    pub fn defaults() -> Self {
        toml::from_str(include_str!("../pricing.toml")).expect("baked-in pricing.toml parses")
    }

    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }

    pub fn lookup(&self, model_key: &str) -> Option<&ModelPrice> {
        self.providers
            .values()
            .find_map(|p| p.models.get(model_key))
    }

    /// Lays `overrides` over this table one model at a time. Each key is first removed
    /// from every provider, so an override wins whichever provider it's filed under and
    /// `lookup` never sees two rates for one model.
    pub fn merge(&mut self, overrides: &PricingTable) {
        for (provider, p) in &overrides.providers {
            for (key, price) in &p.models {
                for existing in self.providers.values_mut() {
                    existing.models.remove(key);
                }
                self.providers
                    .entry(provider.clone())
                    .or_default()
                    .models
                    .insert(key.clone(), price.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HypotheticalCost {
    pub input_usd: f64,
    pub output_usd: f64,
    pub total_usd: f64,
}

pub fn hypothetical_cost(rec: &InferenceRecord, price: &ModelPrice) -> HypotheticalCost {
    let input_usd = rec.prompt_tokens as f64 / 1_000_000.0 * price.input_per_mtok_usd;
    let output_usd = rec.gen_tokens as f64 / 1_000_000.0 * price.output_per_mtok_usd;
    HypotheticalCost {
        input_usd,
        output_usd,
        total_usd: input_usd + output_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(prompt: u64, out_tokens: u64) -> InferenceRecord {
        InferenceRecord {
            model_id: "local".into(),
            started_at: Utc::now(),
            prompt_tokens: prompt,
            gen_tokens: out_tokens,
            ttft_ms: 0.0,
            gen_ms: 0.0,
            total_ms: 0.0,
            tokens_per_second: 0.0,
            stop_reason: None,
            num_gpu_layers: None,
        }
    }

    #[test]
    fn defaults_have_all_frontier_models() {
        let table = PricingTable::defaults();
        for key in FRONTIER_MODELS {
            assert!(
                table.lookup(key).is_some(),
                "frontier model {key} missing from defaults"
            );
        }
    }

    #[test]
    fn defaults_match_pricing_toml() {
        let table = PricingTable::defaults();
        let fable = table.lookup("claude-fable-5").unwrap();
        assert_eq!(fable.input_per_mtok_usd, 10.00);
        assert_eq!(fable.output_per_mtok_usd, 50.00);

        let opus = table.lookup("claude-opus-4-8").unwrap();
        assert_eq!(opus.input_per_mtok_usd, 5.00);
        assert_eq!(opus.output_per_mtok_usd, 25.00);

        for (key, input, output) in [
            ("claude-fable-5-1", 10.00, 50.00),
            ("claude-opus-5-5", 4.00, 20.00),
            ("claude-opus-5", 5.00, 25.00),
        ] {
            let p = table.lookup(key).unwrap();
            assert_eq!(
                (p.input_per_mtok_usd, p.output_per_mtok_usd),
                (input, output),
                "{key}"
            );
        }

        let gemini = table.lookup("gemini-3-1-pro").unwrap();
        assert_eq!(gemini.input_per_mtok_usd, 2.00);
        assert_eq!(gemini.output_per_mtok_usd, 12.00);
    }

    #[test]
    fn one_million_in_one_million_out_matches_rates() {
        let table = PricingTable::defaults();
        let opus = table.lookup("claude-opus-4-8").unwrap();
        let cost = hypothetical_cost(&rec(1_000_000, 1_000_000), opus);
        assert_eq!(cost.input_usd, 5.00);
        assert_eq!(cost.output_usd, 25.00);
        assert_eq!(cost.total_usd, 30.00);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let table = PricingTable::defaults();
        assert!(table.lookup("nonexistent-model").is_none());
    }

    #[test]
    fn merge_overrides_one_model_and_keeps_the_rest() {
        let mut table = PricingTable::defaults();
        let overrides = PricingTable::from_str(
            r#"
[providers.mine.models.claude-opus-4-8]
input_per_mtok_usd = 1.0
output_per_mtok_usd = 2.0
"#,
        )
        .unwrap();
        table.merge(&overrides);

        let opus = table.lookup("claude-opus-4-8").unwrap();
        assert_eq!(
            (opus.input_per_mtok_usd, opus.output_per_mtok_usd),
            (1.0, 2.0)
        );
        let holders: Vec<&str> = table
            .providers
            .iter()
            .filter(|(_, p)| p.models.contains_key("claude-opus-4-8"))
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(holders, ["mine"], "the override must be the only rate left");
        for key in FRONTIER_MODELS {
            assert!(table.lookup(key).is_some(), "{key} lost in merge");
        }
        assert_eq!(
            table.lookup("claude-fable-5").unwrap().input_per_mtok_usd,
            10.00
        );
    }

    #[test]
    fn baked_keys_are_unique_across_providers() {
        let table = PricingTable::defaults();
        let mut seen = std::collections::BTreeSet::new();
        for p in table.providers.values() {
            for key in p.models.keys() {
                assert!(seen.insert(key.as_str()), "{key} is under two providers");
            }
        }
    }

    #[test]
    fn cost_scales_linearly() {
        let table = PricingTable::defaults();
        let fable = table.lookup("claude-fable-5").unwrap();
        let cost = hypothetical_cost(&rec(500_000, 100_000), fable);
        // 0.5 * 10.00 + 0.1 * 50.00 = 5.0 + 5.0 = 10.00
        assert!((cost.total_usd - 10.00).abs() < 1e-9);
    }
}
