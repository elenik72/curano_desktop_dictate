use serde::{Deserialize, Serialize};
use specta::Type;

/// Lifecycle of a single uploaded audio file.
///
/// `Queued`/`Uploading` are local stages; `Processing` means the file is on
/// the server and transcription is running there; `Completed`/`Failed` are
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum UploadStatus {
    Queued,
    Uploading,
    Processing,
    Completed,
    Failed,
}

/// One audio file uploaded for server-side transcription via the standalone
/// `/api/transcriptions` jobs API (no consultation or patient involved).
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct UploadEntry {
    pub id: String,
    pub file_name: String,
    /// Original local path; kept for retry and playback. May point to a
    /// moved/deleted file.
    pub source_path: Option<String>,
    pub size_bytes: u32,
    /// Server-side transcription job id, set once the upload finished.
    #[serde(default)]
    pub job_id: Option<i64>,
    pub created_at_ms: i64,
    pub status: UploadStatus,
    /// Upload progress 0-100, only meaningful while `Uploading`.
    pub progress: u8,
    pub transcript: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct UploadsChangedPayload {
    pub entries: Vec<UploadEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct UploadProgressPayload {
    pub id: String,
    pub progress: u8,
}
