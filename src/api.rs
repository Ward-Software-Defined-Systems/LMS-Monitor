#![allow(dead_code)]

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub publisher: Option<String>,
    pub arch: Option<String>,
    pub compatibility_type: Option<String>,
    pub quantization: Option<String>,
    pub state: String,
    pub max_context_length: Option<u64>,
    pub loaded_context_length: Option<u64>,
    pub capabilities: Option<Vec<String>>,
}

impl ModelInfo {
    pub fn is_loaded(&self) -> bool {
        self.state == "loaded"
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Clone)]
pub enum ModelsSnapshot {
    Loaded(Vec<ModelInfo>),
    Unreachable { error: String },
}

pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
}

pub async fn list_models(
    client: &reqwest::Client,
    base_url: &str,
) -> anyhow::Result<Vec<ModelInfo>> {
    let url = format!("{}/api/v0/models", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await?.error_for_status()?;
    let body: ModelsResponse = resp.json().await?;
    Ok(body.data)
}

pub async fn poll_models(base_url: String, interval: Duration, tx: mpsc::Sender<ModelsSnapshot>) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(ModelsSnapshot::Unreachable {
                    error: format!("client init: {e}"),
                })
                .await;
            return;
        }
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let snap = match list_models(&client, &base_url).await {
            Ok(models) => ModelsSnapshot::Loaded(models),
            Err(e) => ModelsSnapshot::Unreachable {
                error: e.to_string(),
            },
        };
        if tx.send(snap).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADED_FIXTURE: &str = include_str!("../fixtures/api-models-loaded.json");
    const EMPTY_FIXTURE: &str = include_str!("../fixtures/api-models-empty.json");

    #[test]
    fn parses_loaded_fixture() {
        let resp: ModelsResponse =
            serde_json::from_str(LOADED_FIXTURE).expect("loaded fixture parses");
        assert_eq!(resp.data.len(), 2);

        let mlx = &resp.data[0];
        assert_eq!(mlx.id, "qwen3.6-35b-a3b-ud-mlx");
        assert_eq!(mlx.state, "loaded");
        assert!(mlx.is_loaded());
        assert_eq!(mlx.kind.as_deref(), Some("vlm"));
        assert_eq!(mlx.compatibility_type.as_deref(), Some("mlx"));
        assert_eq!(mlx.quantization.as_deref(), Some("4bit"));
        assert_eq!(mlx.max_context_length, Some(262144));
        assert_eq!(mlx.loaded_context_length, Some(262144));
        assert_eq!(
            mlx.capabilities.as_deref().unwrap(),
            &["tool_use".to_string()]
        );

        let embed = &resp.data[1];
        assert_eq!(embed.id, "text-embedding-nomic-embed-text-v1.5");
        assert_eq!(embed.state, "not-loaded");
        assert!(!embed.is_loaded());
        assert_eq!(embed.kind.as_deref(), Some("embeddings"));
        assert_eq!(embed.compatibility_type.as_deref(), Some("gguf"));
        assert_eq!(embed.loaded_context_length, None);
        assert_eq!(embed.capabilities, None);
    }

    #[test]
    fn parses_empty_fixture() {
        let resp: ModelsResponse =
            serde_json::from_str(EMPTY_FIXTURE).expect("empty fixture parses");
        assert!(resp.data.is_empty());
    }

    #[test]
    fn list_models_url_trims_trailing_slash() {
        let trimmed = "http://localhost:1234/".trim_end_matches('/');
        assert_eq!(
            format!("{}/api/v0/models", trimmed),
            "http://localhost:1234/api/v0/models"
        );
    }
}
