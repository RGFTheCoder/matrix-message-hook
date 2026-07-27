//! Generate note embeddings via a local Ollama server, for semantic search.
//!
//! Embedding is deliberately a plain outbound HTTP call from `hookd` itself
//! (using the `reqwest` client already vendored for the appservice client),
//! not a SurrealDB-side `fn::embed()` function calling out over `http::post`.
//! Both work, but doing it here keeps error handling / timeouts / graceful
//! degradation in normal Rust rather than needing SurrealDB's network
//! capability allowlist (`--allow-net`) wired up for the embedding host too —
//! and `hookd` already makes outbound calls like this elsewhere (the
//! appservice client). See `hook_core::store` for how the resulting vector is
//! stored (an `option<array<float>>` field, HNSW-indexed).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Generate an embedding for `text` using `model` on the Ollama server at
/// `ollama_url`. Uses the batched `/api/embed` endpoint (not the older,
/// singular `/api/embeddings`) with a one-element input, since it's the
/// actively maintained endpoint going forward.
pub async fn embed(ollama_url: &str, model: &str, text: &str) -> Result<Vec<f32>> {
    #[derive(Deserialize)]
    struct EmbedResponse {
        embeddings: Vec<Vec<f32>>,
    }

    let url = format!("{}/api/embed", ollama_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "model": model, "input": [text] }))
        .send()
        .await
        .with_context(|| format!("requesting embedding from {url}"))?;

    if !resp.status().is_success() {
        bail!(
            "ollama embed request failed: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }

    let mut body: EmbedResponse = resp.json().await.context("parsing ollama embed response")?;
    if body.embeddings.is_empty() {
        bail!("ollama returned no embeddings");
    }
    Ok(body.embeddings.swap_remove(0))
}
