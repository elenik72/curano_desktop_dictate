//! HTTP client for the Curano API (shared `ApiCtx`) and the standalone
//! transcription-jobs endpoints used by the uploads feature.
//!
//! All requests run in Rust: the JWT lives on this side and the webview
//! would hit CORS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tauri::AppHandle;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

use crate::livestt::auth::ensure_fresh_livestt_access_token;
use crate::settings;

const JSON_TIMEOUT: Duration = Duration::from_secs(30);

/// Server-side limit for `POST /api/transcriptions` uploads (~200 MB).
pub const MAX_UPLOAD_BYTES: u64 = 200 * 1024 * 1024;

pub struct ApiCtx {
    pub base_url: String,
    pub token: String,
    client: reqwest::Client,
}

/// State of a standalone transcription job (`GET /api/transcriptions/{id}`).
pub struct TranscriptionJob {
    pub status: String,
    pub transcription_text: Option<String>,
    pub error_message: Option<String>,
}

pub async fn api_ctx(app: &AppHandle) -> Result<ApiCtx, String> {
    let app_settings = settings::get_settings(app);
    let base_url =
        settings::validate_livestt_server_url_required(&app_settings.livestt_server_url)?;
    let token = ensure_fresh_livestt_access_token(app).await?;

    // HTTP/1.1 only: streamed multipart bodies over HTTP/2 through the CDN
    // intermittently die with mid-stream resets ("error sending request").
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .http1_only()
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    Ok(ApiCtx {
        base_url,
        token,
        client,
    })
}

/// Full error chain ("error sending request: connection reset by peer"
/// instead of just the top-level message).
fn error_chain(error: reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut source = std::error::Error::source(&error);
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }

    let mut text = error.without_url().to_string();
    for part in parts {
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
    }
    text
}

/// Flatten a FastAPI error body (`detail` as string or validation array)
/// into a short human-readable string.
fn extract_error_detail(value: &Value) -> Option<String> {
    let detail = value.get("detail")?;

    let text = match detail {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("msg").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(200).collect())
    }
}

async fn read_json_response(
    response: reqwest::Response,
    what: &str,
) -> Result<(reqwest::StatusCode, Value), String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|_| format!("Failed to read {} response", what))?;

    let value = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        log::warn!(
            "{} failed: status={}, body={}",
            what,
            status,
            body.chars().take(300).collect::<String>()
        );
    }
    Ok((status, value))
}

impl ApiCtx {
    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(JSON_TIMEOUT)
    }

    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.auth(self.client.get(url))
    }

    pub fn post(&self, url: &str) -> reqwest::RequestBuilder {
        self.auth(self.client.post(url))
    }

    pub fn patch(&self, url: &str) -> reqwest::RequestBuilder {
        self.auth(self.client.patch(url))
    }

    pub fn put(&self, url: &str) -> reqwest::RequestBuilder {
        self.auth(self.client.put(url))
    }

    pub fn delete(&self, url: &str) -> reqwest::RequestBuilder {
        self.auth(self.client.delete(url))
    }

    /// Upload a local audio file to `POST /api/transcriptions` as multipart,
    /// reporting progress 0-100. Returns the created job id.
    pub async fn create_transcription_job(
        &self,
        path: &str,
        file_name: &str,
        dictation: bool,
        on_progress: impl Fn(u8) + Send + Sync + 'static,
        cancel: CancellationToken,
    ) -> Result<i64, String> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("Failed to open file: {}", e))?;
        let total = file
            .metadata()
            .await
            .map_err(|e| format!("Failed to read file metadata: {}", e))?
            .len();

        if total == 0 {
            return Err("File is empty".to_string());
        }
        if total > MAX_UPLOAD_BYTES {
            return Err("file_too_large".to_string());
        }

        let sent = Arc::new(AtomicU64::new(0));
        let last_pct = Arc::new(AtomicU64::new(0));
        let stream = ReaderStream::new(file).inspect({
            let sent = sent.clone();
            let last_pct = last_pct.clone();
            move |chunk| {
                if let Ok(bytes) = chunk {
                    let done =
                        sent.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
                    let pct = ((done.min(total) * 100) / total) as u64;
                    if pct > last_pct.swap(pct, Ordering::Relaxed) {
                        on_progress(pct as u8);
                    }
                }
            }
        });

        let part =
            reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), total)
                .file_name(file_name.to_string())
                .mime_str(content_type_for(file_name))
                .map_err(|e| format!("Failed to build upload part: {}", e))?;

        let form = reqwest::multipart::Form::new().part("file", part);

        // No overall timeout: large files legitimately take minutes. Only
        // the connect timeout on the client applies.
        let request = self
            .client
            .post(format!("{}/api/transcriptions", self.base_url))
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("dictation", if dictation { "true" } else { "false" })])
            .multipart(form)
            .send();

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
            result = request => result.map_err(|e| {
                format!("File upload failed: {}", error_chain(e))
            })?,
        };

        let (status, value) = read_json_response(response, "transcription upload").await?;

        if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
            return Err("file_too_large".to_string());
        }

        if !status.is_success() {
            return Err(match extract_error_detail(&value) {
                Some(detail) => format!("Upload failed ({}): {}", status.as_u16(), detail),
                None => format!("Upload failed with status {}", status),
            });
        }

        value
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "Transcription upload response missing id".to_string())
    }

    pub async fn get_transcription_job(&self, job_id: i64) -> Result<TranscriptionJob, String> {
        let url = format!("{}/api/transcriptions/{}", self.base_url, job_id);

        let response = self
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Transcription status request failed: {}", e.without_url()))?;

        let (status, value) = read_json_response(response, "transcription status").await?;

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("job_not_found".to_string());
        }

        if !status.is_success() {
            return Err(format!(
                "Transcription status failed with status {}",
                status
            ));
        }

        Ok(TranscriptionJob {
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            transcription_text: value
                .get("transcription_text")
                .and_then(Value::as_str)
                .map(str::to_string),
            error_message: value
                .get("error_message")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
        })
    }
}

pub fn content_type_for(file_name: &str) -> &'static str {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "mp4" => "audio/mp4",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Formats accepted by `POST /api/transcriptions`.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "m4a", "wav", "mp4"];

pub fn is_supported_audio(file_name: &str) -> bool {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SUPPORTED_EXTENSIONS.contains(&ext.as_str())
}
