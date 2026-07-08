use crate::audio_toolkit::{list_input_devices, vad::SmoothedVad, AudioRecorder, SileroVad};
use crate::helpers::clamshell;
use crate::livestt::session::LiveSttAudioSender;
use crate::settings::{get_settings, AppSettings};
use crate::utils;
use cpal::traits::DeviceTrait;
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Manager;

const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the open stream may go without delivering any audio callbacks
/// before the watchdog considers it dead and reopens it. WASAPI/CoreAudio
/// deliver buffers continuously (silence included), so a stall this long means
/// the stream is broken (device re-enumerated, format changed, driver reset).
const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_STALL_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Lowercase alphabetic characters only. Strips the noise Windows adds to
/// re-enumerated endpoints ("Microphone (2- Philips SpeechMike)") and survives
/// legacy 31-char name truncation.
fn normalize_device_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphabetic())
        .collect()
}

/// Find the best match for `wanted` among device `names`.
/// Order: exact match → normalized equality → normalized containment (guarded
/// against short generic names like "microphone" matching everything).
fn match_device_index(names: &[String], wanted: &str) -> Option<usize> {
    if let Some(idx) = names.iter().position(|n| n == wanted) {
        return Some(idx);
    }

    let wanted_norm = normalize_device_name(wanted);
    if wanted_norm.is_empty() {
        return None;
    }

    if let Some(idx) = names
        .iter()
        .position(|n| normalize_device_name(n) == wanted_norm)
    {
        return Some(idx);
    }

    names.iter().position(|n| {
        let n_norm = normalize_device_name(n);
        let shorter = n_norm.len().min(wanted_norm.len());
        shorter > 10 && (n_norm.contains(&wanted_norm) || wanted_norm.contains(&n_norm))
    })
}

fn set_mute(mute: bool) {
    // Expected behavior:
    // - Windows: works on most systems using standard audio drivers.
    // - Linux: works on many systems (PipeWire, PulseAudio, ALSA),
    //   but some distros may lack the tools used.
    // - macOS: works on most standard setups via AppleScript.
    // If unsupported, fails silently.

    #[cfg(target_os = "windows")]
    {
        unsafe {
            use windows::Win32::{
                Media::Audio::{
                    eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
                    MMDeviceEnumerator,
                },
                System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            };

            macro_rules! unwrap_or_return {
                ($expr:expr) => {
                    match $expr {
                        Ok(val) => val,
                        Err(_) => return,
                    }
                };
            }

            // Initialize the COM library for this thread.
            // If already initialized (e.g., by another library like Tauri), this does nothing.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let all_devices: IMMDeviceEnumerator =
                unwrap_or_return!(CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL));
            let default_device =
                unwrap_or_return!(all_devices.GetDefaultAudioEndpoint(eRender, eMultimedia));
            let volume_interface = unwrap_or_return!(
                default_device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            );

            let _ = volume_interface.SetMute(mute, std::ptr::null());
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        let mute_val = if mute { "1" } else { "0" };
        let amixer_state = if mute { "mute" } else { "unmute" };

        // Try multiple backends to increase compatibility
        // 1. PipeWire (wpctl)
        if Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 2. PulseAudio (pactl)
        if Command::new("pactl")
            .args(["set-sink-mute", "@DEFAULT_SINK@", mute_val])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }

        // 3. ALSA (amixer)
        let _ = Command::new("amixer")
            .args(["set", "Master", amixer_state])
            .output();
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let script = format!(
            "set volume output muted {}",
            if mute { "true" } else { "false" }
        );
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }
}

const WHISPER_SAMPLE_RATE: usize = 16000;

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone, Debug)]
pub enum RecordingState {
    Idle,
    Recording { binding_id: String },
}

#[derive(Clone, Debug)]
pub enum MicrophoneMode {
    AlwaysOn,
    OnDemand,
}

/* ──────────────────────────────────────────────────────────────── */

fn create_audio_recorder(
    vad_path: &str,
    app_handle: &tauri::AppHandle,
    recording_chunk_sender: Arc<Mutex<Option<LiveSttAudioSender>>>,
    last_audio_activity: Arc<Mutex<Instant>>,
) -> Result<AudioRecorder, anyhow::Error> {
    let silero = SileroVad::new(vad_path, 0.3)
        .map_err(|e| anyhow::anyhow!("Failed to create SileroVad: {}", e))?;
    let smoothed_vad = SmoothedVad::new(Box::new(silero), 15, 15, 2);

    // Recorder with local VAD plus a spectrum-level callback that forwards
    // updates to the frontend. The LiveSTT callback receives raw resampled
    // 16 kHz mono frames before local VAD filtering.
    let recorder = AudioRecorder::new()
        .map_err(|e| anyhow::anyhow!("Failed to create AudioRecorder: {}", e))?
        .with_vad(Box::new(smoothed_vad))
        .with_level_callback({
            let app_handle = app_handle.clone();
            move |levels| {
                *last_audio_activity.lock().unwrap() = Instant::now();
                utils::emit_levels(&app_handle, &levels);
            }
        })
        .with_recording_chunk_callback(move |chunk| {
            let sender = match recording_chunk_sender.try_lock() {
                Ok(sender) => sender.clone(),
                Err(_) => {
                    debug!("Dropped LiveSTT recording chunk because sender lock is busy");
                    return;
                }
            };
            if let Some(sender) = sender {
                sender.try_send_chunk(chunk);
            }
        });

    Ok(recorder)
}

/* ──────────────────────────────────────────────────────────────── */

#[derive(Clone)]
pub struct AudioRecordingManager {
    state: Arc<Mutex<RecordingState>>,
    mode: Arc<Mutex<MicrophoneMode>>,
    app_handle: tauri::AppHandle,

    recorder: Arc<Mutex<Option<AudioRecorder>>>,
    is_open: Arc<Mutex<bool>>,
    is_recording: Arc<Mutex<bool>>,
    did_mute: Arc<Mutex<bool>>,
    close_generation: Arc<AtomicU64>,
    recording_chunk_sender: Arc<Mutex<Option<LiveSttAudioSender>>>,
    last_audio_activity: Arc<Mutex<Instant>>,
}

impl AudioRecordingManager {
    /* ---------- construction ------------------------------------------------ */

    pub fn new(app: &tauri::AppHandle) -> Result<Self, anyhow::Error> {
        let settings = get_settings(app);
        let mode = if settings.always_on_microphone {
            MicrophoneMode::AlwaysOn
        } else {
            MicrophoneMode::OnDemand
        };

        let manager = Self {
            state: Arc::new(Mutex::new(RecordingState::Idle)),
            mode: Arc::new(Mutex::new(mode.clone())),
            app_handle: app.clone(),

            recorder: Arc::new(Mutex::new(None)),
            is_open: Arc::new(Mutex::new(false)),
            is_recording: Arc::new(Mutex::new(false)),
            did_mute: Arc::new(Mutex::new(false)),
            close_generation: Arc::new(AtomicU64::new(0)),
            recording_chunk_sender: Arc::new(Mutex::new(None)),
            last_audio_activity: Arc::new(Mutex::new(Instant::now())),
        };

        // Always-on?  Open immediately.
        if matches!(mode, MicrophoneMode::AlwaysOn) {
            manager.start_microphone_stream()?;
        }

        manager.spawn_stream_watchdog();

        Ok(manager)
    }

    /// Background watchdog: an open stream that stops delivering audio
    /// callbacks (device re-enumerated, format changed, driver reset — common
    /// on Windows with USB mics like the SpeechMike) is torn down silently by
    /// the OS without a stream error. Detect the stall and reopen. Never
    /// touches the stream mid-recording.
    fn spawn_stream_watchdog(&self) {
        let manager = self.clone();
        std::thread::Builder::new()
            .name("mic-stream-watchdog".to_string())
            .spawn(move || {
                // Escalation counter: consecutive stall recoveries that did not
                // bring audio back. After WATCHDOG_RESET_THRESHOLD plain stream
                // reopens, the OS audio session itself is suspect ("RPC failed",
                // "Class not registered") — do a full stack reset instead.
                const WATCHDOG_RESET_THRESHOLD: u32 = 2;
                let mut consecutive_stalls: u32 = 0;

                loop {
                    std::thread::sleep(STREAM_STALL_POLL_INTERVAL);

                    // Hold the state lock across the check AND the reopen to
                    // serialize against try_start_recording (same pattern as
                    // schedule_lazy_close): a recording must not begin between
                    // the Idle check and the stream teardown.
                    let state = manager.state.lock().unwrap();
                    if !matches!(*state, RecordingState::Idle) {
                        continue;
                    }
                    if !*manager.is_open.lock().unwrap() {
                        continue;
                    }

                    let stalled_for = manager.last_audio_activity.lock().unwrap().elapsed();
                    if stalled_for < STREAM_STALL_TIMEOUT {
                        // Audio is flowing again — de-escalate.
                        consecutive_stalls = 0;
                        continue;
                    }

                    consecutive_stalls += 1;
                    if consecutive_stalls >= WATCHDOG_RESET_THRESHOLD {
                        warn!(
                            "Microphone stream still silent after {} reopen attempts; performing full audio-stack reset",
                            consecutive_stalls - 1
                        );
                        if let Err(e) = manager.reset_audio_stack_locked() {
                            error!("Stream watchdog full reset failed: {e}");
                        }
                    } else {
                        warn!(
                            "Microphone stream delivered no audio for {:?}; reopening stream",
                            stalled_for
                        );
                        if let Err(e) = manager.update_selected_device() {
                            error!("Stream watchdog failed to reopen microphone: {e}");
                        }
                    }
                    // Reset either way so a persistently broken device retries
                    // at STREAM_STALL_TIMEOUT cadence instead of every poll tick.
                    *manager.last_audio_activity.lock().unwrap() = Instant::now();
                    drop(state);
                }
            })
            .expect("failed to spawn mic-stream-watchdog thread");
    }

    /* ---------- helper methods --------------------------------------------- */

    fn get_effective_microphone_device(&self, settings: &AppSettings) -> Option<cpal::Device> {
        // Check if we're in clamshell mode and have a clamshell microphone configured
        let use_clamshell_mic = if let Ok(is_clamshell) = clamshell::is_clamshell() {
            is_clamshell && settings.clamshell_microphone.is_some()
        } else {
            false
        };

        let device_name = if use_clamshell_mic {
            settings.clamshell_microphone.as_ref().unwrap()
        } else {
            settings.selected_microphone.as_ref()?
        };

        // Find the device by name
        match list_input_devices() {
            Ok(mut devices) => {
                let available_names = devices
                    .iter()
                    .map(|d| {
                        if d.is_default {
                            format!("{} (default)", d.name)
                        } else {
                            d.name.clone()
                        }
                    })
                    .collect::<Vec<_>>();
                debug!(
                    "Resolving selected microphone '{}'; available input devices: {:?}",
                    device_name, available_names
                );

                let names: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
                let selected =
                    match_device_index(&names, device_name).map(|idx| devices.swap_remove(idx));
                match &selected {
                    Some(d) if d.name != *device_name => {
                        warn!(
                            "Selected microphone '{}' matched device '{}' by fuzzy name (Windows re-enumeration or truncated name)",
                            device_name, d.name
                        );
                    }
                    None => {
                        warn!(
                            "Selected microphone '{}' is not present in CPAL input devices; falling back to system default input",
                            device_name
                        );
                    }
                    _ => {}
                }
                selected.map(|d| d.device)
            }
            Err(e) => {
                debug!("Failed to list devices, using default: {}", e);
                None
            }
        }
    }

    fn schedule_lazy_close(&self) {
        let gen = self.close_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let app = self.app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(STREAM_IDLE_TIMEOUT);
            let rm = app.state::<Arc<AudioRecordingManager>>();
            // Hold state lock across the check AND close to serialize against
            // try_start_recording, preventing a race where the stream is closed
            // under an active recording.
            let state = rm.state.lock().unwrap();
            if rm.close_generation.load(Ordering::SeqCst) == gen
                && matches!(*state, RecordingState::Idle)
            {
                // stop_microphone_stream does not acquire the state lock,
                // so holding it here is safe (no deadlock).
                info!(
                    "Closing idle microphone stream after {:?}",
                    STREAM_IDLE_TIMEOUT
                );
                rm.stop_microphone_stream();
            }
        });
    }

    /* ---------- microphone life-cycle -------------------------------------- */

    /// Applies mute if mute_while_recording is enabled and stream is open
    pub fn apply_mute(&self) {
        let settings = get_settings(&self.app_handle);
        let mut did_mute_guard = self.did_mute.lock().unwrap();

        if settings.mute_while_recording && *self.is_open.lock().unwrap() {
            set_mute(true);
            *did_mute_guard = true;
            debug!("Mute applied");
        }
    }

    /// Removes mute if it was applied
    pub fn remove_mute(&self) {
        let mut did_mute_guard = self.did_mute.lock().unwrap();
        if *did_mute_guard {
            set_mute(false);
            *did_mute_guard = false;
            debug!("Mute removed");
        }
    }

    pub fn preload_vad(&self) -> Result<(), anyhow::Error> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        if recorder_opt.is_none() {
            let vad_path = self
                .app_handle
                .path()
                .resolve(
                    "resources/models/silero_vad_v4.onnx",
                    tauri::path::BaseDirectory::Resource,
                )
                .map_err(|e| anyhow::anyhow!("Failed to resolve VAD path: {}", e))?;
            *recorder_opt = Some(create_audio_recorder(
                vad_path.to_str().unwrap(),
                &self.app_handle,
                self.recording_chunk_sender.clone(),
                self.last_audio_activity.clone(),
            )?);
        }
        Ok(())
    }

    pub fn start_microphone_stream(&self) -> Result<(), anyhow::Error> {
        let mut open_flag = self.is_open.lock().unwrap();
        if *open_flag {
            let worker_running = self
                .recorder
                .lock()
                .unwrap()
                .as_mut()
                .map(|rec| rec.is_worker_running())
                .unwrap_or(false);

            if worker_running {
                debug!("Microphone stream already active");
                return Ok(());
            }

            warn!("Microphone stream was marked active but recorder worker is gone; reopening");
            *open_flag = false;
        }

        let start_time = Instant::now();

        // Don't mute immediately - caller will handle muting after audio feedback
        let mut did_mute_guard = self.did_mute.lock().unwrap();
        *did_mute_guard = false;

        // Get the selected device from settings, considering clamshell mode
        let settings = get_settings(&self.app_handle);
        let selected_device = self.get_effective_microphone_device(&settings);
        let selected_device_name = selected_device
            .as_ref()
            .and_then(|device| device.name().ok())
            .unwrap_or_else(|| "system default input".to_string());
        info!("Opening microphone stream for device: {selected_device_name}");

        // Pre-flight check: if no device was selected/configured AND no devices
        // exist at all, fail early with a clear error instead of letting cpal
        // produce a cryptic backend-specific message.
        if selected_device.is_none() {
            let has_any_device = list_input_devices()
                .map(|devices| !devices.is_empty())
                .unwrap_or(false);
            if !has_any_device {
                return Err(anyhow::anyhow!("No input device found"));
            }
        }

        // Ensure VAD is loaded if it wasn't for whatever reason
        self.preload_vad()?;

        let mut recorder_opt = self.recorder.lock().unwrap();
        if let Some(rec) = recorder_opt.as_mut() {
            rec.open(selected_device)
                .map_err(|e| anyhow::anyhow!("Failed to open recorder: {}", e))?;
        }

        *open_flag = true;
        // Fresh baseline for the stall watchdog: the first audio callback can
        // lag stream.play() by seconds on USB/Bluetooth devices.
        *self.last_audio_activity.lock().unwrap() = Instant::now();
        // This timing covers through cpal's stream.play() returning — i.e. the
        // point cpal surfaces as "stream running." It does NOT guarantee the
        // host audio device is producing samples yet; the first input callback
        // fires asynchronously one buffer period later (hardware dependent,
        // typically ~10–200ms on macOS, longer on Bluetooth/USB).
        info!(
            "Microphone stream initialized in {:?}",
            start_time.elapsed()
        );
        Ok(())
    }

    pub fn stop_microphone_stream(&self) {
        let mut open_flag = self.is_open.lock().unwrap();
        if !*open_flag {
            return;
        }

        let mut did_mute_guard = self.did_mute.lock().unwrap();
        if *did_mute_guard {
            set_mute(false);
        }
        *did_mute_guard = false;

        if let Some(rec) = self.recorder.lock().unwrap().as_mut() {
            // If still recording, stop first.
            if *self.is_recording.lock().unwrap() {
                let _ = rec.stop();
                *self.recording_chunk_sender.lock().unwrap() = None;
                *self.is_recording.lock().unwrap() = false;
            }
            let _ = rec.close();
        }

        *open_flag = false;
        debug!("Microphone stream stopped");
    }

    /// Full audio-stack reset: tear down the stream AND the recorder (fresh
    /// WASAPI/CoreAudio objects), then reopen in always-on mode. Recovers from
    /// OS-level audio-session corruption ("RPC failed", "Class not registered")
    /// that a plain stream reopen cannot fix. Refuses to run mid-recording.
    pub fn reset_audio_stack(&self) -> Result<(), anyhow::Error> {
        // Hold the state lock across the teardown to serialize against
        // try_start_recording (same pattern as the stall watchdog).
        let state = self.state.lock().unwrap();
        if !matches!(*state, RecordingState::Idle) {
            return Err(anyhow::anyhow!(
                "Cannot reset the audio stack while recording"
            ));
        }
        self.reset_audio_stack_locked()
    }

    /// Core of the reset; caller must hold the `state` lock with `Idle` state.
    fn reset_audio_stack_locked(&self) -> Result<(), anyhow::Error> {
        warn!("Resetting audio stack: dropping recorder and reopening stream");
        self.close_generation.fetch_add(1, Ordering::SeqCst);
        self.stop_microphone_stream();
        *self.recorder.lock().unwrap() = None;

        if matches!(*self.mode.lock().unwrap(), MicrophoneMode::AlwaysOn) {
            // start_microphone_stream recreates the recorder via preload_vad.
            self.start_microphone_stream()?;
        }
        Ok(())
    }

    /// Close the microphone stream only when it is safe: on-demand mode and no
    /// active recording. Used by the mic-test UI so stopping a test never tears
    /// down an always-on stream or an in-progress recording.
    pub fn stop_microphone_stream_if_idle(&self) {
        if !matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
            return;
        }
        if !matches!(*self.state.lock().unwrap(), RecordingState::Idle) {
            return;
        }
        self.close_generation.fetch_add(1, Ordering::SeqCst);
        self.stop_microphone_stream();
    }

    /* ---------- mode switching --------------------------------------------- */

    pub fn update_mode(&self, new_mode: MicrophoneMode) -> Result<(), anyhow::Error> {
        let cur_mode = self.mode.lock().unwrap().clone();

        match (cur_mode, &new_mode) {
            (MicrophoneMode::AlwaysOn, MicrophoneMode::OnDemand) => {
                if matches!(*self.state.lock().unwrap(), RecordingState::Idle) {
                    self.close_generation.fetch_add(1, Ordering::SeqCst);
                    self.stop_microphone_stream();
                }
            }
            (MicrophoneMode::OnDemand, MicrophoneMode::AlwaysOn) => {
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                self.start_microphone_stream()?;
            }
            _ => {}
        }

        *self.mode.lock().unwrap() = new_mode;
        Ok(())
    }

    /* ---------- recording --------------------------------------------------- */

    pub fn try_start_recording(&self, binding_id: &str) -> Result<(), String> {
        self.try_start_recording_inner(binding_id, None)
    }

    pub fn try_start_recording_with_chunk_sender(
        &self,
        binding_id: &str,
        sender: LiveSttAudioSender,
    ) -> Result<(), String> {
        self.try_start_recording_inner(binding_id, Some(sender))
    }

    fn try_start_recording_inner(
        &self,
        binding_id: &str,
        chunk_sender: Option<LiveSttAudioSender>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Idle = *state {
            let in_on_demand_mode = matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand);
            // Ensure microphone is open in on-demand mode
            if in_on_demand_mode {
                // Cancel any pending lazy close
                self.close_generation.fetch_add(1, Ordering::SeqCst);
                if let Err(e) = self.start_microphone_stream() {
                    let msg = format!("{e}");
                    error!("Failed to open microphone stream: {msg}");
                    return Err(msg);
                }
            }

            // Preroll only makes sense for LiveSTT (chunk_sender present) and
            // only when the mic was already open before this call (always-on
            // mode). On-demand mode just opened the stream, so the ring is
            // cold — pass 0 to skip the replay path entirely.
            let preroll_samples = if chunk_sender.is_some() && !in_on_demand_mode {
                let preroll_ms = get_settings(&self.app_handle).livestt_preroll_ms;
                (preroll_ms as usize) * WHISPER_SAMPLE_RATE / 1000
            } else {
                0
            };

            *self.recording_chunk_sender.lock().unwrap() = chunk_sender.clone();
            let start_result = self.start_recorder(preroll_samples);

            if let Err(err) = start_result {
                error!("Failed to start recorder: {err}; reopening microphone stream");
                *self.recording_chunk_sender.lock().unwrap() = None;
                self.stop_microphone_stream();
                if let Err(open_err) = self.start_microphone_stream() {
                    let msg = format!("{open_err}");
                    error!("Failed to reopen microphone stream: {msg}");
                    return Err(msg);
                }

                *self.recording_chunk_sender.lock().unwrap() = chunk_sender;
                if let Err(retry_err) = self.start_recorder(preroll_samples) {
                    error!("Failed to start recorder after reopening stream: {retry_err}");
                    *self.recording_chunk_sender.lock().unwrap() = None;
                    return Err("Recorder not available".to_string());
                }
            }

            *self.is_recording.lock().unwrap() = true;
            *state = RecordingState::Recording {
                binding_id: binding_id.to_string(),
            };
            debug!(
                "Recording started for binding {binding_id} (preroll_samples={preroll_samples})"
            );
            Ok(())
        } else {
            Err("Already recording".to_string())
        }
    }

    fn start_recorder(&self, preroll_samples: usize) -> Result<(), String> {
        let mut recorder_opt = self.recorder.lock().unwrap();
        let Some(rec) = recorder_opt.as_mut() else {
            return Err("Recorder not available".to_string());
        };
        if !rec.is_worker_running() {
            return Err("Recorder worker is not running".to_string());
        }
        rec.start(preroll_samples)
            .map_err(|e| format!("Recorder start failed: {e}"))
    }

    pub fn update_selected_device(&self) -> Result<(), anyhow::Error> {
        // If currently open, restart the microphone stream to use the new device
        if *self.is_open.lock().unwrap() {
            self.close_generation.fetch_add(1, Ordering::SeqCst);
            self.stop_microphone_stream();
            self.start_microphone_stream()?;
        }
        Ok(())
    }

    pub fn stop_recording(&self, binding_id: &str) -> Option<Vec<f32>> {
        let mut state = self.state.lock().unwrap();

        match *state {
            RecordingState::Recording {
                binding_id: ref active,
            } if active == binding_id => {
                *state = RecordingState::Idle;
                drop(state);

                // Optionally keep recording for a bit longer to capture trailing audio
                let settings = get_settings(&self.app_handle);
                if settings.extra_recording_buffer_ms > 0 {
                    debug!(
                        "Extra recording buffer: sleeping {}ms before stopping",
                        settings.extra_recording_buffer_ms
                    );
                    std::thread::sleep(Duration::from_millis(settings.extra_recording_buffer_ms));
                }

                let samples = if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                    match rec.stop() {
                        Ok(buf) => buf,
                        Err(e) => {
                            error!("stop() failed: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    error!("Recorder not available");
                    Vec::new()
                };
                *self.recording_chunk_sender.lock().unwrap() = None;

                *self.is_recording.lock().unwrap() = false;

                // In on-demand mode, close the mic (lazily if the setting is enabled)
                if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                    if get_settings(&self.app_handle).lazy_stream_close {
                        self.schedule_lazy_close();
                    } else {
                        self.stop_microphone_stream();
                    }
                }

                // Pad if very short
                let s_len = samples.len();
                // debug!("Got {} samples", s_len);
                if s_len < WHISPER_SAMPLE_RATE && s_len > 0 {
                    let mut padded = samples;
                    padded.resize(WHISPER_SAMPLE_RATE * 5 / 4, 0.0);
                    Some(padded)
                } else {
                    Some(samples)
                }
            }
            _ => None,
        }
    }
    pub fn is_recording(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            RecordingState::Recording { .. }
        )
    }

    /// Cancel any ongoing recording without returning audio samples
    pub fn cancel_recording(&self) {
        let mut state = self.state.lock().unwrap();

        if let RecordingState::Recording { .. } = *state {
            *state = RecordingState::Idle;
            drop(state);

            *self.recording_chunk_sender.lock().unwrap() = None;
            if let Some(rec) = self.recorder.lock().unwrap().as_ref() {
                let _ = rec.cancel(); // Discard the result
            }

            *self.is_recording.lock().unwrap() = false;

            // In on-demand mode, close the mic (lazily if the setting is enabled)
            if matches!(*self.mode.lock().unwrap(), MicrophoneMode::OnDemand) {
                if get_settings(&self.app_handle).lazy_stream_close {
                    self.schedule_lazy_close();
                } else {
                    self.stop_microphone_stream();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::match_device_index;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_match_wins() {
        let devices = names(&["Built-in Microphone", "Microphone (Philips SpeechMike III)"]);
        assert_eq!(
            match_device_index(&devices, "Microphone (Philips SpeechMike III)"),
            Some(1)
        );
    }

    #[test]
    fn matches_windows_reenumerated_name() {
        let devices = names(&[
            "Built-in Microphone",
            "Microphone (2- Philips SpeechMike III)",
        ]);
        assert_eq!(
            match_device_index(&devices, "Microphone (Philips SpeechMike III)"),
            Some(1)
        );
    }

    #[test]
    fn matches_truncated_legacy_name() {
        let devices = names(&["Built-in Microphone", "Microphone (Philips SpeechMike III)"]);
        // Legacy WaveIn APIs truncate device names to 31 characters.
        assert_eq!(
            match_device_index(&devices, "Microphone (Philips SpeechMike "),
            Some(1)
        );
    }

    #[test]
    fn generic_short_name_does_not_fuzzy_match() {
        let devices = names(&["Microphone (Philips SpeechMike III)"]);
        assert_eq!(match_device_index(&devices, "Microphone"), None);
    }

    #[test]
    fn no_match_falls_through() {
        let devices = names(&["Built-in Microphone"]);
        assert_eq!(
            match_device_index(&devices, "Microphone (Philips SpeechMike III)"),
            None
        );
    }

    #[test]
    fn case_insensitive_normalized_match() {
        let devices = names(&["MICROPHONE (PHILIPS SPEECHMIKE III)"]);
        assert_eq!(
            match_device_index(&devices, "Microphone (Philips SpeechMike III)"),
            Some(0)
        );
    }
}
