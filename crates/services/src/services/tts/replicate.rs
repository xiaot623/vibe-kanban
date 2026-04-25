//! Replicate TTS client.
//!
//! Protocol:
//!  1. Look up model via `GET /models/{owner}/{name}` → grab `latest_version.id`.
//!  2. Build prediction input from the server-side adapter registry.
//!  3. Create a prediction via `POST /predictions`.
//!  4. Poll `GET /predictions/{id}` until status is `succeeded`, `failed`, or `canceled`.
//!  5. On cancellation signal, call `POST /predictions/{id}/cancel`.
//!  6. Download the output URL to the audio temp dir.

use std::{path::PathBuf, time::Duration};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use utils::cache_dir as vibe_cache_dir;

use super::{TtsError, TtsOutput};

const REPLICATE_API_BASE: &str = "https://api.replicate.com/v1";
const POLL_INTERVAL: Duration = Duration::from_millis(2000);
const MAX_POLL_ATTEMPTS: usize = 300; // 10 minutes max

/// Replicate prediction input built from the adapter registry.
#[derive(Debug, Clone)]
struct ModelAdapter {
    /// Input field carrying the text to synthesise.
    text_field: &'static str,
    /// Additional static fields for this model.
    extra: Vec<(&'static str, Value)>,
}

/// Return the server-side adapter for a curated model slug.
fn adapter_for_model(model_slug: &str) -> Option<ModelAdapter> {
    match model_slug {
        "minimax/speech-2.8-turbo" => Some(ModelAdapter {
            text_field: "text",
            extra: vec![("voice_id", json!("Wise_Woman"))],
        }),
        "qwen/qwen3-tts" => Some(ModelAdapter {
            text_field: "text",
            extra: vec![],
        }),
        _ => None,
    }
}

/// Replicate API response shapes.
#[derive(Debug, Deserialize)]
struct ModelResponse {
    latest_version: Option<ModelVersion>,
}

#[derive(Debug, Deserialize)]
struct ModelVersion {
    id: String,
}

#[derive(Debug, Deserialize)]
struct PredictionResponse {
    id: String,
    status: String,
    output: Option<Value>,
    error: Option<Value>,
}

#[derive(Debug, Serialize)]
struct CreatePredictionBody {
    version: String,
    input: Value,
}

pub struct ReplicateTtsClient {
    token: String,
    http: Client,
}

impl ReplicateTtsClient {
    pub fn new(token: String) -> Self {
        Self {
            token,
            http: Client::new(),
        }
    }

    pub async fn synthesize(
        &self,
        model_slug: &str,
        text: &str,
        cancel: CancellationToken,
    ) -> Result<TtsOutput, TtsError> {
        // 1. Validate model
        let adapter = adapter_for_model(model_slug)
            .ok_or_else(|| TtsError::UnsupportedModel(model_slug.to_string()))?;

        // 2. Resolve latest version
        let version_id = self.resolve_version(model_slug).await?;

        // 3. Build input
        let mut input = json!({
            adapter.text_field: text,
        });
        for (k, v) in &adapter.extra {
            input[k] = v.clone();
        }

        // 4. Create prediction
        let prediction_id = self.create_prediction(&version_id, input, &cancel).await?;

        // 5. Poll until done
        let output_url = self.poll_prediction(&prediction_id, &cancel).await?;

        // 6. Download
        let file_path = self.download_audio(&output_url, &prediction_id).await?;

        Ok(TtsOutput { file_path })
    }

    async fn resolve_version(&self, model_slug: &str) -> Result<String, TtsError> {
        let url = format!("{}/models/{}", REPLICATE_API_BASE, model_slug);
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::ReplicateApi(format!(
                "GET /models/{model_slug} returned {status}: {body}"
            )));
        }

        let model: ModelResponse = resp.json().await?;
        model.latest_version.map(|v| v.id).ok_or_else(|| {
            TtsError::ReplicateApi(format!("Model {model_slug} has no latest_version"))
        })
    }

    async fn create_prediction(
        &self,
        version_id: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> Result<String, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::Cancelled);
        }

        let body = CreatePredictionBody {
            version: version_id.to_string(),
            input,
        };

        let resp = self
            .http
            .post(format!("{}/predictions", REPLICATE_API_BASE))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(TtsError::ReplicateApi(format!(
                "POST /predictions returned {status}: {body_text}"
            )));
        }

        let prediction: PredictionResponse = resp.json().await?;
        Ok(prediction.id)
    }

    async fn poll_prediction(
        &self,
        prediction_id: &str,
        cancel: &CancellationToken,
    ) -> Result<String, TtsError> {
        for _ in 0..MAX_POLL_ATTEMPTS {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = self.cancel_prediction(prediction_id).await;
                    return Err(TtsError::Cancelled);
                }
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            let prediction = self.get_prediction(prediction_id).await?;
            match prediction.status.as_str() {
                "succeeded" => {
                    let output_url = extract_output_url(prediction.output).ok_or_else(|| {
                        TtsError::ReplicateApi(format!(
                            "Prediction {prediction_id} succeeded but output URL is missing"
                        ))
                    })?;
                    return Ok(output_url);
                }
                "failed" => {
                    let err_msg = prediction
                        .error
                        .as_ref()
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown error".to_string());
                    return Err(TtsError::PredictionFailed(err_msg));
                }
                "canceled" => {
                    return Err(TtsError::Cancelled);
                }
                _ => {
                    // starting | processing — keep polling
                }
            }
        }

        // Timed out — cancel prediction
        let _ = self.cancel_prediction(prediction_id).await;
        Err(TtsError::PredictionFailed(format!(
            "Prediction {prediction_id} timed out after polling"
        )))
    }

    async fn get_prediction(&self, prediction_id: &str) -> Result<PredictionResponse, TtsError> {
        let url = format!("{}/predictions/{}", REPLICATE_API_BASE, prediction_id);
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(TtsError::ReplicateApi(format!(
                "GET /predictions/{prediction_id} returned {status}: {body}"
            )));
        }

        Ok(resp.json().await?)
    }

    pub async fn cancel_prediction(&self, prediction_id: &str) -> Result<(), TtsError> {
        let url = format!(
            "{}/predictions/{}/cancel",
            REPLICATE_API_BASE, prediction_id
        );
        let resp = self.http.post(&url).bearer_auth(&self.token).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            tracing::warn!("Failed to cancel Replicate prediction {prediction_id}: {status}");
        }
        Ok(())
    }

    async fn download_audio(&self, url: &str, prediction_id: &str) -> Result<PathBuf, TtsError> {
        let audio_dir = vibe_cache_dir().join("audio");
        tokio::fs::create_dir_all(&audio_dir).await?;

        // Derive extension from URL (default mp3)
        let ext = url
            .split('?')
            .next()
            .and_then(|path| path.rsplit('.').next())
            .filter(|e| e.len() <= 5)
            .unwrap_or("mp3");
        let file_path = audio_dir.join(format!("{}.{}", prediction_id, ext));

        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(TtsError::ReplicateApi(format!(
                "Failed to download audio from {url}: {status}"
            )));
        }

        let bytes = resp.bytes().await?;
        tokio::fs::write(&file_path, &bytes).await?;

        Ok(file_path)
    }
}

/// Extract the first URL from a Replicate prediction output.
///
/// Output can be:
/// - `"https://..."` (string)
/// - `["https://..."]` (array of strings)
fn extract_output_url(output: Option<Value>) -> Option<String> {
    let output = output?;
    match output {
        Value::String(s) => Some(s),
        Value::Array(arr) => arr.into_iter().find_map(|v| {
            if let Value::String(s) = v {
                Some(s)
            } else {
                None
            }
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_registry_covers_all_curated_models() {
        let models = ["minimax/speech-2.8-turbo", "qwen/qwen3-tts"];
        for model in models {
            assert!(
                adapter_for_model(model).is_some(),
                "Missing adapter for {model}"
            );
        }
    }

    #[test]
    fn adapter_returns_none_for_unknown_model() {
        assert!(adapter_for_model("unknown/model-xyz").is_none());
    }

    #[test]
    fn extract_output_url_handles_string() {
        let url = extract_output_url(Some(json!("https://example.com/out.mp3")));
        assert_eq!(url.as_deref(), Some("https://example.com/out.mp3"));
    }

    #[test]
    fn extract_output_url_handles_array() {
        let url = extract_output_url(Some(json!(["https://example.com/out.mp3"])));
        assert_eq!(url.as_deref(), Some("https://example.com/out.mp3"));
    }

    #[test]
    fn extract_output_url_returns_none_for_null() {
        assert_eq!(extract_output_url(None), None);
        assert_eq!(extract_output_url(Some(json!(null))), None);
    }
}
