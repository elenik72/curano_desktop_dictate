//! Upload queue, persistence and server-status polling.
//!
//! Files upload through a semaphore (max 3 in parallel, Drive-style queue)
//! straight to the standalone `POST /api/transcriptions` endpoint — no
//! consultation or patient involved. A poller then watches
//! `GET /api/transcriptions/{job_id}` until the status turns terminal.
//! There is no server-side delete for jobs, so removing an entry is local.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use tokio_util::sync::CancellationToken;

use super::api::{self, MAX_UPLOAD_BYTES};
use super::types::{UploadEntry, UploadProgressPayload, UploadStatus, UploadsChangedPayload};

const UPLOADS_STORE_PATH: &str = "uploads_store.json";
const UPLOADS_ENTRIES_KEY: &str = "entries";
/// Leftover from the consultation-based flow; cleaned up on startup.
const UPLOADS_LEGACY_PATIENT_KEY: &str = "patient_id";
const MAX_PARALLEL_UPLOADS: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Spoken-punctuation formatting: plain text without timecodes.
const DICTATION_MODE: bool = true;
/// Total attempts for one upload before it is marked failed.
const UPLOAD_ATTEMPTS: u32 = 3;

static ENTRY_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_entry_id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().timestamp_millis(),
        ENTRY_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub struct UploadsManager {
    app: AppHandle,
    entries: Mutex<Vec<UploadEntry>>,
    cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl UploadsManager {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let mut entries = load_entries(&app);

        for entry in entries.iter_mut() {
            // Uploads killed mid-flight by an app restart cannot resume.
            // Entries from the old consultation-based flow have no job_id
            // and cannot be polled either. Both become retryable failures.
            let interrupted =
                matches!(entry.status, UploadStatus::Queued | UploadStatus::Uploading)
                    || (entry.status == UploadStatus::Processing && entry.job_id.is_none());

            if interrupted {
                entry.status = UploadStatus::Failed;
                entry.error = Some("interrupted".to_string());
                entry.progress = 0;
            }
        }

        if let Ok(store) = app.store(crate::portable::store_path(UPLOADS_STORE_PATH)) {
            store.delete(UPLOADS_LEGACY_PATIENT_KEY);
        }

        let manager = Arc::new(Self {
            app,
            entries: Mutex::new(entries),
            cancel_tokens: Mutex::new(HashMap::new()),
            semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_UPLOADS)),
        });

        manager.persist();
        manager.clone().spawn_poller();
        manager
    }

    pub fn entries(&self) -> Vec<UploadEntry> {
        self.entries.lock().expect("uploads state poisoned").clone()
    }

    fn emit_changed(&self) {
        let payload = UploadsChangedPayload {
            entries: self.entries(),
        };
        if let Err(e) = payload.emit(&self.app) {
            log::warn!("Failed to emit uploads-changed event: {}", e);
        }
    }

    fn persist(&self) {
        let entries = self.entries();
        match self
            .app
            .store(crate::portable::store_path(UPLOADS_STORE_PATH))
        {
            Ok(store) => match serde_json::to_value(&entries) {
                Ok(value) => store.set(UPLOADS_ENTRIES_KEY, value),
                Err(e) => log::warn!("Failed to serialize uploads: {}", e),
            },
            Err(e) => log::warn!("Failed to open uploads store: {}", e),
        }
    }

    fn update_entry(&self, id: &str, apply: impl FnOnce(&mut UploadEntry)) -> bool {
        let mut entries = self.entries.lock().expect("uploads state poisoned");
        match entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => {
                apply(entry);
                true
            }
            None => false,
        }
    }

    fn entry_snapshot(&self, id: &str) -> Option<UploadEntry> {
        self.entries
            .lock()
            .expect("uploads state poisoned")
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }

    fn remove_entry(&self, id: &str) {
        self.entries
            .lock()
            .expect("uploads state poisoned")
            .retain(|e| e.id != id);
    }

    /// Validate paths and enqueue them. Returns an error only when nothing
    /// could be enqueued.
    pub fn add_files(self: &Arc<Self>, paths: Vec<String>) -> Result<(), String> {
        if paths.is_empty() {
            return Err("no_files".to_string());
        }
        if paths.len() > 10 {
            return Err("too_many_files".to_string());
        }

        let mut new_ids = Vec::new();
        let mut added_any = false;

        for path in paths {
            let file_name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());

            if !api::is_supported_audio(&file_name) {
                log::warn!("Skipping unsupported file: {}", file_name);
                continue;
            }

            let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size_bytes == 0 {
                log::warn!("Skipping empty/unreadable file: {}", file_name);
                continue;
            }

            // Oversized files become visible failed entries instead of
            // silently disappearing.
            let too_large = size_bytes > MAX_UPLOAD_BYTES;

            let entry = UploadEntry {
                id: next_entry_id(),
                file_name,
                source_path: Some(path),
                size_bytes: size_bytes.min(u32::MAX as u64) as u32,
                job_id: None,
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                status: if too_large {
                    UploadStatus::Failed
                } else {
                    UploadStatus::Queued
                },
                progress: 0,
                transcript: None,
                error: too_large.then(|| "file_too_large".to_string()),
            };

            if !too_large {
                new_ids.push(entry.id.clone());
            }
            added_any = true;
            self.entries
                .lock()
                .expect("uploads state poisoned")
                .insert(0, entry);
        }

        if !added_any {
            return Err("unsupported_files".to_string());
        }

        self.persist();
        self.emit_changed();

        for id in new_ids {
            self.clone().spawn_pipeline(id);
        }

        Ok(())
    }

    pub fn retry(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let entry = self
            .entry_snapshot(id)
            .ok_or_else(|| "not_found".to_string())?;

        if entry.status != UploadStatus::Failed {
            return Err("not_failed".to_string());
        }

        entry
            .source_path
            .as_deref()
            .filter(|p| std::path::Path::new(p).exists())
            .ok_or_else(|| "source_missing".to_string())?;

        self.update_entry(id, |e| {
            e.status = UploadStatus::Queued;
            e.progress = 0;
            e.job_id = None;
            e.transcript = None;
            e.error = None;
        });
        self.persist();
        self.emit_changed();
        self.clone().spawn_pipeline(id.to_string());
        Ok(())
    }

    /// Cancel an in-flight upload and remove the entry.
    pub fn cancel(&self, id: &str) -> Result<(), String> {
        let entry = self
            .entry_snapshot(id)
            .ok_or_else(|| "not_found".to_string())?;

        if !matches!(entry.status, UploadStatus::Queued | UploadStatus::Uploading) {
            return Err("not_active".to_string());
        }

        if let Some(token) = self
            .cancel_tokens
            .lock()
            .expect("uploads tokens poisoned")
            .get(id)
        {
            token.cancel();
        }

        self.remove_entry(id);
        self.persist();
        self.emit_changed();
        Ok(())
    }

    /// Remove an entry from the local list. The jobs API has no delete, so
    /// the server-side job (and its transcript) stays untouched.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let entry = self
            .entry_snapshot(id)
            .ok_or_else(|| "not_found".to_string())?;

        if matches!(entry.status, UploadStatus::Queued | UploadStatus::Uploading) {
            return self.cancel(id);
        }

        self.remove_entry(id);
        self.persist();
        self.emit_changed();
        Ok(())
    }

    fn spawn_pipeline(self: Arc<Self>, id: String) {
        let token = CancellationToken::new();
        self.cancel_tokens
            .lock()
            .expect("uploads tokens poisoned")
            .insert(id.clone(), token.clone());

        tauri::async_runtime::spawn(async move {
            let result = run_pipeline(&self, &id, token).await;

            self.cancel_tokens
                .lock()
                .expect("uploads tokens poisoned")
                .remove(&id);

            match result {
                PipelineOutcome::Done => {}
                PipelineOutcome::Cancelled => {
                    // cancel() already removed the entry and notified.
                }
                PipelineOutcome::Failed(error) => {
                    log::warn!("Upload {} failed: {}", id, error);
                    self.update_entry(&id, |e| {
                        e.status = UploadStatus::Failed;
                        e.error = Some(error);
                    });
                    self.persist();
                    self.emit_changed();
                }
            }
        });
    }

    fn spawn_poller(self: Arc<Self>) {
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;

                let targets: Vec<UploadEntry> = self
                    .entries()
                    .into_iter()
                    .filter(|e| e.status == UploadStatus::Processing && e.job_id.is_some())
                    .collect();

                if targets.is_empty() {
                    continue;
                }

                let ctx = match api::api_ctx(&self.app).await {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        log::debug!("Uploads poll skipped: {}", e);
                        continue;
                    }
                };

                let mut changed = false;

                for target in targets {
                    let Some(job_id) = target.job_id else {
                        continue;
                    };

                    match ctx.get_transcription_job(job_id).await {
                        Ok(job) => {
                            let new_status = match job.status.as_str() {
                                "completed" => UploadStatus::Completed,
                                "failed" | "deleted" => UploadStatus::Failed,
                                _ => UploadStatus::Processing,
                            };

                            let transcript = job
                                .transcription_text
                                .as_deref()
                                .map(strip_transcript_timestamps);

                            self.update_entry(&target.id, |e| {
                                if e.status != new_status {
                                    e.status = new_status;
                                    changed = true;
                                    if new_status == UploadStatus::Failed {
                                        e.error = Some(match job.status.as_str() {
                                            "deleted" => "job_deleted".to_string(),
                                            _ => job.error_message.clone().unwrap_or_else(|| {
                                                "transcription_failed".to_string()
                                            }),
                                        });
                                    }
                                }
                                if new_status == UploadStatus::Completed
                                    && e.transcript != transcript
                                {
                                    e.transcript = transcript.clone();
                                    changed = true;
                                }
                            });
                        }
                        Err(e) if e == "job_not_found" => {
                            self.update_entry(&target.id, |entry| {
                                entry.status = UploadStatus::Failed;
                                entry.error = Some("job_deleted".to_string());
                            });
                            changed = true;
                        }
                        Err(e) => {
                            log::debug!("Uploads poll for job {} failed: {}", job_id, e);
                        }
                    }
                }

                if changed {
                    self.persist();
                    self.emit_changed();
                }
            }
        });
    }
}

enum PipelineOutcome {
    Done,
    Cancelled,
    Failed(String),
}

async fn run_pipeline(
    manager: &Arc<UploadsManager>,
    id: &str,
    token: CancellationToken,
) -> PipelineOutcome {
    let permit = tokio::select! {
        _ = token.cancelled() => return PipelineOutcome::Cancelled,
        permit = manager.semaphore.clone().acquire_owned() => permit,
    };
    let _permit = match permit {
        Ok(permit) => permit,
        Err(_) => return PipelineOutcome::Failed("Upload queue closed".to_string()),
    };

    let Some(entry) = manager.entry_snapshot(id) else {
        return PipelineOutcome::Cancelled;
    };
    let Some(source_path) = entry.source_path.clone() else {
        return PipelineOutcome::Failed("Source file path is unknown".to_string());
    };

    manager.update_entry(id, |e| {
        e.status = UploadStatus::Uploading;
        e.progress = 0;
    });
    manager.emit_changed();

    // Transient network drops mid-body are common on multi-megabyte
    // uploads; retry the whole POST a couple of times before giving up.
    // Only transport errors retry — server rejections (4xx, size) fail fast.
    let mut job_id = None;
    for attempt in 1..=UPLOAD_ATTEMPTS {
        let ctx = match api::api_ctx(&manager.app).await {
            Ok(ctx) => ctx,
            Err(e) => return PipelineOutcome::Failed(e),
        };

        let progress_app = manager.app.clone();
        let progress_id = id.to_string();
        let upload_result = ctx
            .create_transcription_job(
                &source_path,
                &entry.file_name,
                DICTATION_MODE,
                move |pct| {
                    let _ = (UploadProgressPayload {
                        id: progress_id.clone(),
                        progress: pct,
                    })
                    .emit(&progress_app);
                },
                token.clone(),
            )
            .await;

        match upload_result {
            Ok(created) => {
                job_id = Some(created);
                break;
            }
            Err(e) if e == "cancelled" => return PipelineOutcome::Cancelled,
            Err(e) => {
                let transient = e.starts_with("File upload failed:");
                if !transient || attempt == UPLOAD_ATTEMPTS {
                    return PipelineOutcome::Failed(e);
                }

                let backoff = Duration::from_secs(2 * attempt as u64);
                log::warn!(
                    "Upload {} attempt {}/{} failed ({}); retrying in {:?}",
                    id,
                    attempt,
                    UPLOAD_ATTEMPTS,
                    e,
                    backoff
                );

                let _ = (UploadProgressPayload {
                    id: id.to_string(),
                    progress: 0,
                })
                .emit(&manager.app);

                tokio::select! {
                    _ = token.cancelled() => return PipelineOutcome::Cancelled,
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
        }
    }

    let Some(job_id) = job_id else {
        return PipelineOutcome::Failed("Upload failed".to_string());
    };

    manager.update_entry(id, |e| {
        e.job_id = Some(job_id);
        e.status = UploadStatus::Processing;
        e.progress = 100;
    });
    manager.persist();
    manager.emit_changed();

    PipelineOutcome::Done
}

fn load_entries(app: &AppHandle) -> Vec<UploadEntry> {
    let Ok(store) = app.store(crate::portable::store_path(UPLOADS_STORE_PATH)) else {
        return Vec::new();
    };

    let mut entries: Vec<UploadEntry> = store
        .get(UPLOADS_ENTRIES_KEY)
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();

    for entry in entries.iter_mut() {
        if let Some(transcript) = entry.transcript.as_deref() {
            entry.transcript = Some(strip_transcript_timestamps(transcript));
        }
    }

    entries
}

fn strip_transcript_timestamps(text: &str) -> String {
    text.lines()
        .filter(|line| !is_timestamp_range_line(line))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_timestamp_range_line(line: &str) -> bool {
    let normalized = line.trim().replace("-->", "~");
    let Some((start, end)) = normalized.split_once('~') else {
        return false;
    };

    is_timestamp_token(start.trim()) && is_timestamp_token(end.trim())
}

fn is_timestamp_token(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) {
        return false;
    }

    parts.iter().enumerate().all(|(index, part)| {
        let valid_len = if index + 1 == parts.len() {
            part.len() == 2
        } else {
            part.len() == 2 || part.len() == 1
        };

        valid_len && part.chars().all(|ch| ch.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::strip_transcript_timestamps;

    #[test]
    fn removes_timestamp_range_lines() {
        let input =
            "02:52 ~ 02:53\nWie geht es mit Gelenken?\n02:54 ~ 02:55\nÄhm, Gelenke sehr gut.";

        assert_eq!(
            strip_transcript_timestamps(input),
            "Wie geht es mit Gelenken?\nÄhm, Gelenke sehr gut."
        );
    }

    #[test]
    fn supports_arrow_and_hour_timestamps() {
        let input = "00:02:52 --> 00:02:53\nHello\n0:03:01 ~ 0:03:02\nWorld";

        assert_eq!(strip_transcript_timestamps(input), "Hello\nWorld");
    }
}
