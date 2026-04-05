//! Text-to-speech service.
//!
//! Provides a thin dispatch layer over provider-specific clients.
//! v1 only supports Replicate.

pub mod replicate;

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::services::config::TtsConfig;

/// A resolved audio output file on disk.
pub struct TtsOutput {
    /// Absolute path to the downloaded audio file.
    pub file_path: PathBuf,
}

/// Errors produced by the TTS layer.
#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("TTS not configured: {0}")]
    NotConfigured(String),

    #[error("Unsupported TTS model: {0}")]
    UnsupportedModel(String),

    #[error("Replicate API error: {0}")]
    ReplicateApi(String),

    #[error("Prediction failed: {0}")]
    PredictionFailed(String),

    #[error("Prediction cancelled")]
    Cancelled,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// Synthesise `text` to audio using the provider/model in `config`.
///
/// Returns the path to the downloaded audio file.
/// The caller is responsible for deleting the file when done.
///
/// `cancel` can be used to abort an in-flight synthesis request.
pub async fn synthesize(
    config: &TtsConfig,
    text: &str,
    cancel: CancellationToken,
) -> Result<TtsOutput, TtsError> {
    use crate::services::config::TtsProvider;
    match config.provider {
        TtsProvider::Replicate => {
            let api_token = resolve_replicate_token(config);
            let Some(token) = api_token else {
                return Err(TtsError::NotConfigured(
                    "Replicate API token is not configured".to_string(),
                ));
            };
            let client = replicate::ReplicateTtsClient::new(token);
            client.synthesize(&config.model, text, cancel).await
        }
    }
}

/// Apply a speed multiplier to an audio file using FFmpeg's `atempo` filter.
///
/// The `atempo` filter only accepts values in the range [0.5, 2.0].  For
/// speed values outside that range we chain multiple `atempo` passes.
///
/// Returns the path to the speed-adjusted file (same directory as the
/// original).  If `speed` is effectively 1.0 (within a small epsilon) the
/// original `file_path` is returned unchanged and no FFmpeg process is
/// spawned.
///
/// The caller is responsible for cleaning up **both** files when done.
pub async fn apply_speed(file_path: &PathBuf, speed: f32) -> Result<PathBuf, TtsError> {
    // Skip processing for 1× (no change needed).
    if (speed - 1.0_f32).abs() < 1e-3 {
        return Ok(file_path.clone());
    }

    // Build chained atempo filter string for arbitrary speed values.
    let filter = build_atempo_filter(speed);

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp3");
    let stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let out_path = file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(format!("{stem}_speed.{ext}"));

    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",                              // overwrite output
            "-i", file_path.to_str().unwrap_or(""),
            "-filter:a", &filter,
            "-vn",                             // no video stream
            out_path.to_str().unwrap_or(""),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| TtsError::Io(e))?;

    if !status.success() {
        return Err(TtsError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("ffmpeg exited with status {status}"),
        )));
    }

    Ok(out_path)
}

/// Build an `atempo` filter chain that achieves the requested speed.
///
/// `atempo` only accepts values in [0.5, 2.0], so we chain passes for
/// extreme values:
///   - speed 3.0 → `atempo=2.0,atempo=1.5`
///   - speed 0.25 → `atempo=0.5,atempo=0.5`
fn build_atempo_filter(speed: f32) -> String {
    let mut filters = Vec::new();
    let mut remaining = speed;

    if remaining > 1.0 {
        while remaining > 2.0 + 1e-3 {
            filters.push("atempo=2.0".to_string());
            remaining /= 2.0;
        }
        filters.push(format!("atempo={remaining:.4}"));
    } else {
        while remaining < 0.5 - 1e-3 {
            filters.push("atempo=0.5".to_string());
            remaining /= 0.5;
        }
        filters.push(format!("atempo={remaining:.4}"));
    }

    filters.join(",")
}

/// Resolve the Replicate API token with the precedence:
/// 1. Non-empty value from config UI
/// 2. `REPLICATE_API_TOKEN` environment variable
/// 3. `None`
pub fn resolve_replicate_token(config: &TtsConfig) -> Option<String> {
    if let Some(token) = &config.replicate_api_token {
        if !token.trim().is_empty() {
            return Some(token.clone());
        }
    }
    std::env::var("REPLICATE_API_TOKEN").ok().filter(|t| !t.trim().is_empty())
}
