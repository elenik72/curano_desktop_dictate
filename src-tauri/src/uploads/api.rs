//! HTTP client for the Curano consultations API.
//!
//! All requests run in Rust: the GCS signed-url PUT would hit CORS from the
//! webview, and the LiveSTT JWT lives on this side anyway.

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

/// Consultations created by the app default to this patient. If it does not
/// exist on the server, a technical patient is created transparently.
pub const DEFAULT_PATIENT_ID: i64 = 1287;

const JSON_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ApiCtx {
    pub base_url: String,
    pub token: String,
    client: reqwest::Client,
}

pub enum CreateConsultationError {
    /// The patient id was rejected — caller should create the technical
    /// patient and retry.
    PatientRejected(String),
    Other(String),
}

pub struct RemoteAudio {
    pub id: i64,
    pub status: String,
    pub transcription_text: Option<String>,
}

pub async fn api_ctx(app: &AppHandle) -> Result<ApiCtx, String> {
    let app_settings = settings::get_settings(app);
    let base_url =
        settings::validate_livestt_server_url_required(&app_settings.livestt_server_url)?;
    let token = ensure_fresh_livestt_access_token(app).await?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    Ok(ApiCtx {
        base_url,
        token,
        client,
    })
}

fn json_field<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    value.get(snake).or_else(|| value.get(camel))
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

    pub async fn create_consultation(
        &self,
        patient_id: i64,
    ) -> Result<i64, CreateConsultationError> {
        let url = format!("{}/api/consultations/", self.base_url);
        let payload = serde_json::json!({
            "patient_id": patient_id,
            "consultation_type": "initial",
            "consultation_date": chrono::Utc::now().to_rfc3339(),
            "language": "DE",
        });

        let response = self
            .auth(self.client.post(&url))
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                CreateConsultationError::Other(format!(
                    "Consultation create request failed: {}",
                    e.without_url()
                ))
            })?;

        let (status, value) = read_json_response(response, "consultation create")
            .await
            .map_err(CreateConsultationError::Other)?;

        if !status.is_success() {
            let detail = extract_error_detail(&value);
            let message = match &detail {
                Some(detail) => format!(
                    "Consultation create failed with status {}: {}",
                    status, detail
                ),
                None => format!("Consultation create failed with status {}", status),
            };

            // Only a complaint about the patient itself justifies the
            // technical-patient fallback; other 4xx are payload bugs and
            // retrying with a new patient would just create junk patients.
            let patient_rejected = matches!(
                status,
                reqwest::StatusCode::NOT_FOUND
                    | reqwest::StatusCode::BAD_REQUEST
                    | reqwest::StatusCode::UNPROCESSABLE_ENTITY
            ) && detail
                .as_deref()
                .is_some_and(|d| d.to_ascii_lowercase().contains("patient"));

            return Err(if patient_rejected {
                CreateConsultationError::PatientRejected(message)
            } else {
                CreateConsultationError::Other(message)
            });
        }

        value.get("id").and_then(Value::as_i64).ok_or_else(|| {
            CreateConsultationError::Other("Consultation create response missing id".to_string())
        })
    }

    /// Create the technical patient used as a container for app uploads.
    pub async fn create_technical_patient(&self) -> Result<i64, String> {
        let url = format!("{}/api/patients/", self.base_url);
        let payload = serde_json::json!({
            "first_name": "Dictate",
            "last_name": "Uploads",
            "date_of_birth": "1970-01-01",
        });

        let response = self
            .auth(self.client.post(&url))
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("Patient create request failed: {}", e.without_url()))?;

        let (status, value) = read_json_response(response, "patient create").await?;

        if !status.is_success() {
            return Err(format!("Patient create failed with status {}", status));
        }

        value
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "Patient create response missing id".to_string())
    }

    pub async fn create_upload_url(
        &self,
        consultation_id: i64,
        filename: &str,
    ) -> Result<String, String> {
        let url = format!(
            "{}/api/consultations/{}/audios/upload-url",
            self.base_url, consultation_id
        );

        let response = self
            .auth(self.client.post(&url))
            .query(&[("filename", filename)])
            .send()
            .await
            .map_err(|e| format!("Upload URL request failed: {}", e.without_url()))?;

        let (status, value) = read_json_response(response, "upload URL").await?;

        if !status.is_success() {
            return Err(format!("Upload URL request failed with status {}", status));
        }

        json_field(&value, "signed_url", "signedUrl")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "Upload URL response missing signed_url".to_string())
    }

    pub async fn list_audios(&self, consultation_id: i64) -> Result<Vec<RemoteAudio>, String> {
        let url = format!(
            "{}/api/consultations/{}/audios",
            self.base_url, consultation_id
        );

        let response = self
            .auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| format!("Audio list request failed: {}", e.without_url()))?;

        let (status, value) = read_json_response(response, "audio list").await?;

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("consultation_not_found".to_string());
        }

        if !status.is_success() {
            return Err(format!("Audio list failed with status {}", status));
        }

        let items = value
            .as_array()
            .ok_or_else(|| "Audio list response is not an array".to_string())?;

        Ok(items
            .iter()
            .filter_map(|item| {
                Some(RemoteAudio {
                    id: item.get("id")?.as_i64()?,
                    status: item.get("status")?.as_str()?.to_string(),
                    transcription_text: json_field(item, "transcription_text", "transcriptionText")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect())
    }

    pub async fn delete_consultation(&self, consultation_id: i64) -> Result<(), String> {
        let url = format!("{}/api/consultations/{}", self.base_url, consultation_id);

        let response = self
            .auth(self.client.delete(&url))
            .send()
            .await
            .map_err(|e| format!("Consultation delete request failed: {}", e.without_url()))?;

        let status = response.status();
        // Already gone server-side is fine — the goal is removal.
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(format!("Consultation delete failed with status {}", status))
        }
    }

    /// Stream a local file to a GCS signed URL, reporting progress 0-100.
    pub async fn put_signed_url(
        &self,
        signed_url: &str,
        path: &str,
        content_type: &str,
        on_progress: impl Fn(u8) + Send + Sync + 'static,
        cancel: CancellationToken,
    ) -> Result<(), String> {
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

        let request = self
            .client
            .put(signed_url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, total)
            .body(reqwest::Body::wrap_stream(stream))
            .send();

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
            result = request => result.map_err(|e| {
                format!("File upload failed: {}", e.without_url())
            })?,
        };

        let status = response.status();
        if !status.is_success() {
            return Err(format!("File upload failed with status {}", status));
        }

        Ok(())
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
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "amr" => "audio/amr",
        "wma" => "audio/x-ms-wma",
        "aif" | "aiff" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "mp4", "wav", "ogg", "oga", "opus", "flac", "aac", "webm", "amr", "wma", "aif",
    "aiff",
];

pub fn is_supported_audio(file_name: &str) -> bool {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SUPPORTED_EXTENSIONS.contains(&ext.as_str())
}
