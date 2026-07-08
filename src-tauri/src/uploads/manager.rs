//! Upload queue, persistence and server-status polling.
//!
//! Files upload through a semaphore (max 3 in parallel, Drive-style queue).
//! Every entry gets its own consultation on the Curano backend; after the
//! signed-url PUT succeeds the server transcribes on its own and a poller
//! watches `GET /audios` until the status turns terminal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use tokio_util::sync::CancellationToken;

use super::api::{self, ApiCtx, CreateConsultationError, DEFAULT_PATIENT_ID};
use super::types::{UploadEntry, UploadProgressPayload, UploadStatus, UploadsChangedPayload};

const UPLOADS_STORE_PATH: &str = "uploads_store.json";
const UPLOADS_ENTRIES_KEY: &str = "entries";
const UPLOADS_PATIENT_KEY: &str = "patient_id";
const MAX_PARALLEL_UPLOADS: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(4);

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

        // Uploads killed mid-flight by an app restart cannot resume: the
        // signed URL is gone. Mark them failed so the user can retry.
        for entry in entries.iter_mut() {
            if matches!(entry.status, UploadStatus::Queued | UploadStatus::Uploading) {
                entry.status = UploadStatus::Failed;
                entry.error = Some("interrupted".to_string());
                entry.progress = 0;
            }
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

    fn cached_patient_id(&self) -> i64 {
        self.app
            .store(crate::portable::store_path(UPLOADS_STORE_PATH))
            .ok()
            .and_then(|store| store.get(UPLOADS_PATIENT_KEY))
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_PATIENT_ID)
    }

    fn cache_patient_id(&self, patient_id: i64) {
        if let Ok(store) = self
            .app
            .store(crate::portable::store_path(UPLOADS_STORE_PATH))
        {
            store.set(UPLOADS_PATIENT_KEY, Value::from(patient_id));
        }
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

            let entry = UploadEntry {
                id: next_entry_id(),
                file_name,
                source_path: Some(path),
                size_bytes: size_bytes.min(u32::MAX as u64) as u32,
                consultation_id: None,
                audio_id: None,
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                status: UploadStatus::Queued,
                progress: 0,
                transcript: None,
                error: None,
            };

            new_ids.push(entry.id.clone());
            self.entries
                .lock()
                .expect("uploads state poisoned")
                .insert(0, entry);
        }

        if new_ids.is_empty() {
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

        // The retry uploads into a fresh consultation; drop the old one so
        // failed attempts don't pile up server-side.
        if let Some(old_consultation) = entry.consultation_id {
            let app = self.app.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(ctx) = api::api_ctx(&app).await {
                    let _ = ctx.delete_consultation(old_consultation).await;
                }
            });
        }

        self.update_entry(id, |e| {
            e.status = UploadStatus::Queued;
            e.progress = 0;
            e.consultation_id = None;
            e.audio_id = None;
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

        // The pipeline may already have created the container consultation.
        if let Some(consultation_id) = entry.consultation_id {
            let app = self.app.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(ctx) = api::api_ctx(&app).await {
                    let _ = ctx.delete_consultation(consultation_id).await;
                }
            });
        }

        Ok(())
    }

    /// Delete an entry and its server-side consultation (with the audio and
    /// transcript in it).
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let entry = self
            .entry_snapshot(id)
            .ok_or_else(|| "not_found".to_string())?;

        if matches!(entry.status, UploadStatus::Queued | UploadStatus::Uploading) {
            return self.cancel(id);
        }

        if let Some(consultation_id) = entry.consultation_id {
            let ctx = api::api_ctx(&self.app).await?;
            ctx.delete_consultation(consultation_id).await?;
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
                    .filter(|e| e.status == UploadStatus::Processing)
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
                    let Some(consultation_id) = target.consultation_id else {
                        continue;
                    };

                    match ctx.list_audios(consultation_id).await {
                        Ok(audios) => {
                            let audio = match target.audio_id {
                                Some(audio_id) => audios.into_iter().find(|a| a.id == audio_id),
                                None => audios.into_iter().next(),
                            };

                            let Some(audio) = audio else { continue };

                            let new_status = match audio.status.as_str() {
                                "completed" => UploadStatus::Completed,
                                "reject" | "failed" => UploadStatus::Failed,
                                _ => UploadStatus::Processing,
                            };

                            let transcript = audio
                                .transcription_text
                                .as_deref()
                                .map(strip_transcript_timestamps);
                            let audio_id = audio.id;

                            self.update_entry(&target.id, |e| {
                                if e.audio_id != Some(audio_id) {
                                    e.audio_id = Some(audio_id);
                                    changed = true;
                                }
                                if e.status != new_status {
                                    e.status = new_status;
                                    changed = true;
                                    if new_status == UploadStatus::Failed {
                                        e.error = Some("transcription_failed".to_string());
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
                        Err(e) if e == "consultation_not_found" => {
                            self.update_entry(&target.id, |entry| {
                                entry.status = UploadStatus::Failed;
                                entry.error = Some("consultation_deleted".to_string());
                            });
                            changed = true;
                        }
                        Err(e) => {
                            log::debug!(
                                "Uploads poll for consultation {} failed: {}",
                                consultation_id,
                                e
                            );
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

    let ctx = match api::api_ctx(&manager.app).await {
        Ok(ctx) => ctx,
        Err(e) => return PipelineOutcome::Failed(e),
    };

    let consultation_id = match create_consultation_with_fallback(manager, &ctx).await {
        Ok(consultation_id) => consultation_id,
        Err(e) => return PipelineOutcome::Failed(e),
    };

    manager.update_entry(id, |e| e.consultation_id = Some(consultation_id));
    manager.persist();

    if token.is_cancelled() {
        return PipelineOutcome::Cancelled;
    }

    let signed_url = match ctx
        .create_upload_url(consultation_id, &entry.file_name)
        .await
    {
        Ok(url) => url,
        Err(e) => return PipelineOutcome::Failed(e),
    };

    let progress_app = manager.app.clone();
    let progress_id = id.to_string();
    let upload_result = ctx
        .put_signed_url(
            &signed_url,
            &source_path,
            api::content_type_for(&entry.file_name),
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
        Ok(()) => {}
        Err(e) if e == "cancelled" => return PipelineOutcome::Cancelled,
        Err(e) => return PipelineOutcome::Failed(e),
    }

    manager.update_entry(id, |e| {
        e.status = UploadStatus::Processing;
        e.progress = 100;
    });
    manager.persist();
    manager.emit_changed();

    PipelineOutcome::Done
}

async fn create_consultation_with_fallback(
    manager: &Arc<UploadsManager>,
    ctx: &ApiCtx,
) -> Result<i64, String> {
    let patient_id = manager.cached_patient_id();

    match ctx.create_consultation(patient_id).await {
        Ok(consultation_id) => Ok(consultation_id),
        Err(CreateConsultationError::PatientRejected(reason)) => {
            log::info!(
                "Patient {} rejected ({}), creating technical patient",
                patient_id,
                reason
            );
            let new_patient = ctx.create_technical_patient().await?;
            manager.cache_patient_id(new_patient);
            ctx.create_consultation(new_patient)
                .await
                .map_err(|e| match e {
                    CreateConsultationError::PatientRejected(msg)
                    | CreateConsultationError::Other(msg) => msg,
                })
        }
        Err(CreateConsultationError::Other(e)) => Err(e),
    }
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
