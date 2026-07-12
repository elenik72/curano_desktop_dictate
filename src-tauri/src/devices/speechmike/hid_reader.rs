use hidapi::HidApi;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

use super::buttons::parse_button_event;
use super::dispatch::dispatch_button_event;
use super::identify::{
    find_matching_audio_device, is_button_hid_interface, pick_speechmike_interfaces,
};
use super::status::SpeechMikeStatus;
use crate::managers::audio::AudioRecordingManager;
use crate::settings::{get_settings, write_settings};

/// Payload for the `speechmike://raw-report` debug event.
#[derive(serde::Serialize, Clone)]
struct RawHidReport {
    hex: String,
    length: usize,
    vendor_id: u16,
    product_id: u16,
}

/// Tracks what was last emitted to the frontend, to avoid duplicate events.
#[derive(PartialEq)]
enum LastEmit {
    Disconnected,
    Connected {
        vid: u16,
        pid: u16,
        serial: Option<String>,
        buttons_enabled: bool,
    },
    Blocked,
}

/// How long a device must be continuously missing / unreadable before we
/// emit a disconnect event to the frontend.
const DISCONNECT_GRACE: Duration = Duration::from_millis(1500);
const AUDIO_READY_DELAY: Duration = Duration::from_secs(3);
const AUDIO_SWITCH_RETRY_DELAY: Duration = Duration::from_secs(2);
// 5 attempts × 2s retry + 3s initial delay ≈ 13s of coverage: Windows can
// register the USB audio endpoint well after HID enumeration sees the device.
const AUDIO_SWITCH_ATTEMPTS: usize = 5;

/// Entry point for the background HID polling thread. Runs forever.
///
/// Design goals:
/// - Reuse a single `HidApi` instance to avoid IOHIDManager races on macOS.
/// - Debounce missing/error state: only emit disconnect after `DISCONNECT_GRACE`.
/// - Deduplicate events: never re-emit connected/blocked for the same state.
/// - Classify Windows `ERROR_ACCESS_DENIED` (0x00000005) on *read* as "blocked",
///   not "device removed" — fixes flapping caused by Philips SpeechControl SDK
///   holding exclusive HID access on Windows regardless of system locale.
pub fn polling_loop(app: AppHandle, status: Arc<Mutex<SpeechMikeStatus>>) {
    let mut hid_api: Option<HidApi> = None;
    let mut last_emit = LastEmit::Disconnected;
    let mut hid_button_polling_suppressed = false;
    let mut defaults_applied_for_connection = false;
    // Tracks when the device first went missing (for debounce).
    let mut missing_since: Option<Instant> = None;

    loop {
        // ── 1. Ensure we have a live HidApi instance ──────────────────────
        let api = match ensure_api(&mut hid_api) {
            Some(a) => a,
            None => {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        // Refresh device enumeration (reuses existing HidApi, avoids re-init).
        if let Err(e) = api.refresh_devices() {
            log::warn!("SpeechMike: refresh_devices failed: {e}");
            hid_api = None; // recreate on next iteration
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        // ── 2. Find Philips HID interfaces sorted by read preference ──────
        let candidates = pick_speechmike_interfaces(api);

        if candidates.is_empty() {
            // Device not visible in enumeration — debounce before telling UI.
            if missing_since.is_none() {
                missing_since = Some(Instant::now());
            }
            if missing_since.unwrap().elapsed() >= DISCONNECT_GRACE
                && last_emit != LastEmit::Disconnected
            {
                emit_disconnected(&app, &status, &mut last_emit);
                defaults_applied_for_connection = false;
            }
            if missing_since
                .map(|t| t.elapsed() >= DISCONNECT_GRACE)
                .unwrap_or(false)
            {
                missing_since = None;
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        // Device is physically present — reset the missing-device timer.
        missing_since = None;
        if !defaults_applied_for_connection {
            enable_speechmike_defaults(&app);
            defaults_applied_for_connection = true;
        }

        let button_candidates = candidates
            .iter()
            .filter(|candidate| is_button_hid_interface(candidate))
            .cloned()
            .collect::<Vec<_>>();
        let hid_buttons_enabled = get_settings(&app).speechmike_button_mapping_enabled
            && !hid_button_polling_suppressed
            && !button_candidates.is_empty();
        if !hid_buttons_enabled {
            let candidate = &candidates[0];
            let new_key = LastEmit::Connected {
                vid: candidate.vendor_id,
                pid: candidate.product_id,
                serial: candidate.serial.clone(),
                buttons_enabled: false,
            };

            if last_emit != new_key {
                let audio_name = find_matching_audio_device(&candidate.product_name);
                {
                    let mut s = lock_status(&status);
                    s.connected = true;
                    s.blocked_by_other_app = false;
                    s.device_name = Some(candidate.product_name.clone());
                    s.vendor_id = Some(candidate.vendor_id);
                    s.product_id = Some(candidate.product_id);
                    s.serial_number = candidate.serial.clone();
                    s.audio_device_name = audio_name.clone();
                    s.buttons_enabled = false;
                    s.auto_select_enabled = get_settings(&app).speechmike_auto_select;
                    s.detected_blocking_processes = vec![];
                    s.last_error = None;
                }
                if button_candidates.is_empty() {
                    log::warn!(
                        "SpeechMike connected: {} (VID={:#06x} PID={:#06x}, no safe HID button interface)",
                        candidate.product_name,
                        candidate.vendor_id,
                        candidate.product_id,
                    );
                } else if hid_button_polling_suppressed {
                    log::warn!(
                        "SpeechMike connected: {} (VID={:#06x} PID={:#06x}, HID button polling suppressed after read errors)",
                        candidate.product_name,
                        candidate.vendor_id,
                        candidate.product_id,
                    );
                } else {
                    log::info!(
                        "SpeechMike connected: {} (VID={:#06x} PID={:#06x}, buttons disabled)",
                        candidate.product_name,
                        candidate.vendor_id,
                        candidate.product_id,
                    );
                }
                let snapshot = lock_status(&status).clone();
                let _ = app.emit("speechmike://connected", snapshot);
                maybe_auto_select_microphone(&app, &audio_name);
                last_emit = new_key;
            } else {
                refresh_audio_device(&app, &status, &candidate.product_name);
            }

            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        // ── 3. Try to open each candidate interface in priority order ─────
        let mut opened = None;
        let mut any_blocked = false;
        // Save metadata from the first (highest-priority) candidate so we can
        // populate device info even when all HID open attempts fail (blocked case).
        let mut first_meta: Option<(String, u16, u16, Option<String>)> = None;

        for candidate in button_candidates {
            if first_meta.is_none() {
                first_meta = Some((
                    candidate.product_name.clone(),
                    candidate.vendor_id,
                    candidate.product_id,
                    candidate.serial.clone(),
                ));
            }
            match api.open_path(candidate.path.as_c_str()) {
                Ok(dev) => {
                    opened = Some((dev, candidate));
                    break;
                }
                Err(e) => {
                    let s = e.to_string();
                    if is_access_error(&s) {
                        any_blocked = true;
                        log::debug!("SpeechMike: open blocked on interface: {e}");
                    } else {
                        log::debug!("SpeechMike: open failed: {e}");
                    }
                }
            }
        }

        let Some((device, candidate)) = opened else {
            // All open attempts failed.
            if any_blocked && last_emit != LastEmit::Blocked {
                let processes = scan_blocking_processes();
                log::warn!("SpeechMike: all HID interfaces blocked by another app");

                // Device is physically present — populate its info and auto-select
                // the microphone so audio capture still works even without HID buttons.
                let (product_name, vid, pid, serial) =
                    first_meta.unwrap_or_else(|| ("Philips SpeechMike".into(), 0, 0, None));
                let audio_name = find_matching_audio_device(&product_name);

                {
                    let mut s = lock_status(&status);
                    s.connected = true; // physically present; only buttons are unavailable
                    s.blocked_by_other_app = true;
                    s.device_name = Some(product_name.clone());
                    s.vendor_id = Some(vid);
                    s.product_id = Some(pid);
                    s.serial_number = serial;
                    s.audio_device_name = audio_name.clone();
                    s.buttons_enabled = false;
                    s.auto_select_enabled = get_settings(&app).speechmike_auto_select;
                    s.detected_blocking_processes = processes;
                }
                let snapshot = lock_status(&status).clone();
                let _ = app.emit("speechmike://blocked-by-other-app", snapshot);
                maybe_auto_select_microphone(&app, &audio_name);
                last_emit = LastEmit::Blocked;
            }
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };

        // ── 4. Emit "connected" only on an actual state transition (dedup) ─
        let new_key = LastEmit::Connected {
            vid: candidate.vendor_id,
            pid: candidate.product_id,
            serial: candidate.serial.clone(),
            buttons_enabled: true,
        };

        if last_emit != new_key {
            let audio_name = find_matching_audio_device(&candidate.product_name);
            {
                let mut s = lock_status(&status);
                s.connected = true;
                s.blocked_by_other_app = false;
                s.device_name = Some(candidate.product_name.clone());
                s.vendor_id = Some(candidate.vendor_id);
                s.product_id = Some(candidate.product_id);
                s.serial_number = candidate.serial.clone();
                s.audio_device_name = audio_name.clone();
                s.buttons_enabled = true;
                s.auto_select_enabled = get_settings(&app).speechmike_auto_select;
                s.detected_blocking_processes = vec![];
                s.last_error = None;
            }
            log::info!(
                "SpeechMike connected: {} (VID={:#06x} PID={:#06x}, usage_page={:#06x}, usage={:#06x}, interface={})",
                candidate.product_name,
                candidate.vendor_id,
                candidate.product_id,
                candidate.usage_page,
                candidate.usage,
                candidate.interface_number,
            );
            let snapshot = lock_status(&status).clone();
            let _ = app.emit("speechmike://connected", snapshot);
            maybe_auto_select_microphone(&app, &audio_name);
            last_emit = new_key;
        } else {
            refresh_audio_device(&app, &status, &candidate.product_name);
        }

        // ── 5. Inner read loop — stays here while device is healthy ───────
        let mut buf = [0u8; 64];
        let mut read_err_since: Option<Instant> = None;
        let mut last_report: Option<Vec<u8>> = None;

        'read: loop {
            match device.read_timeout(&mut buf, 50) {
                Ok(0) => {
                    // Timeout — device is alive, heartbeat.
                    read_err_since = None;
                }
                Ok(n) => {
                    read_err_since = None;
                    let raw = buf[..n].to_vec();
                    let settings = get_settings(&app);

                    if settings.livesttt_raw_hid_debug {
                        let hex = raw
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let _ = app.emit(
                            "speechmike://raw-report",
                            RawHidReport {
                                hex,
                                length: n,
                                vendor_id: candidate.vendor_id,
                                product_id: candidate.product_id,
                            },
                        );
                    }

                    if settings.speechmike_button_mapping_enabled {
                        if last_report.as_ref() == Some(&raw) {
                            continue;
                        }
                        last_report = Some(raw.clone());

                        if let Some(event) = parse_button_event(&raw) {
                            dispatch_button_event(&app, event);
                        }
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();

                    if is_access_error(&err_str) {
                        // Another process grabbed exclusive HID access mid-session
                        // (common with Philips SpeechControl SDK on Windows).
                        // Emit "blocked" once; audio capture remains available.
                        log::warn!("SpeechMike: read blocked by other app: {e}");
                        if last_emit != LastEmit::Blocked {
                            let processes = scan_blocking_processes();
                            let audio_name = find_matching_audio_device(&candidate.product_name);
                            {
                                let mut s = lock_status(&status);
                                s.connected = true; // physically present; audio still works
                                s.blocked_by_other_app = true;
                                s.buttons_enabled = false;
                                s.auto_select_enabled = get_settings(&app).speechmike_auto_select;
                                s.audio_device_name = audio_name.clone();
                                s.detected_blocking_processes = processes;
                                s.last_error = Some(err_str);
                            }
                            let snapshot = lock_status(&status).clone();
                            let _ = app.emit("speechmike://blocked-by-other-app", snapshot);
                            maybe_auto_select_microphone(&app, &audio_name);
                            last_emit = LastEmit::Blocked;
                        }
                        // Brief pause, then retry outer loop (may open a different interface).
                        std::thread::sleep(Duration::from_millis(500));
                        break 'read;
                    } else {
                        // Transient error (cable jiggle, driver reset, etc.).
                        // Wait up to DISCONNECT_GRACE before treating as real disconnect.
                        log::debug!("SpeechMike: read error: {e}");
                        if read_err_since.is_none() {
                            read_err_since = Some(Instant::now());
                        }
                        if read_err_since.unwrap().elapsed() >= DISCONNECT_GRACE {
                            log::warn!(
                                "SpeechMike: persistent read errors after {:.1}s; suppressing HID button polling for this session",
                                DISCONNECT_GRACE.as_secs_f32()
                            );
                            hid_button_polling_suppressed = true;
                            let audio_name = find_matching_audio_device(&candidate.product_name);
                            {
                                let mut s = lock_status(&status);
                                s.connected = true;
                                s.blocked_by_other_app = false;
                                s.buttons_enabled = false;
                                s.auto_select_enabled = get_settings(&app).speechmike_auto_select;
                                s.audio_device_name = audio_name.clone();
                                s.detected_blocking_processes = vec![];
                                s.last_error = Some(err_str);
                            }
                            let snapshot = lock_status(&status).clone();
                            let _ = app.emit("speechmike://connected", snapshot);
                            maybe_auto_select_microphone(&app, &audio_name);
                            last_emit = LastEmit::Connected {
                                vid: candidate.vendor_id,
                                pid: candidate.product_id,
                                serial: candidate.serial.clone(),
                                buttons_enabled: false,
                            };
                            break 'read;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }

        // ── 6. After read loop exits ──────────────────────────────────────
        // A read error does not necessarily mean the device was physically
        // unplugged. On Windows, SpeechMike can briefly invalidate the opened
        // HID handle while the device still appears in enumeration a moment
        // later. Keep the frontend in its current state here and let the
        // candidates.is_empty() path above debounce real disconnects.
        log::debug!("SpeechMike: reopening HID interface after read loop ended");
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Get or create the persistent HidApi instance.
fn ensure_api(hid_api: &mut Option<HidApi>) -> Option<&mut HidApi> {
    if hid_api.is_none() {
        match HidApi::new() {
            Ok(api) => *hid_api = Some(api),
            Err(e) => {
                log::error!("SpeechMike: HidApi::new() failed: {e}");
                return None;
            }
        }
    }
    hid_api.as_mut()
}

/// Emit `speechmike://disconnected`, clear status, update `last_emit`.
fn emit_disconnected(
    app: &AppHandle,
    status: &Arc<Mutex<SpeechMikeStatus>>,
    last_emit: &mut LastEmit,
) {
    {
        let mut s = lock_status(status);
        s.connected = false;
        s.blocked_by_other_app = false;
        s.device_name = None;
        s.vendor_id = None;
        s.product_id = None;
        s.serial_number = None;
        s.audio_device_name = None;
        s.buttons_enabled = false;
        s.detected_blocking_processes = vec![];
    }
    log::info!("SpeechMike disconnected");
    let _ = app.emit("speechmike://disconnected", ());
    *last_emit = LastEmit::Disconnected;
}

/// Heuristic: does this error string indicate an OS-level access-denied condition?
///
/// Checks for English keywords, numeric `ERROR_ACCESS_DENIED` (0x00000005),
/// and the German locale string "verweigert" (seen in "Zugriff verweigert").
/// Additional locale strings can be added here as needed.
fn is_access_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("access")
        || lower.contains("denied")
        || lower.contains("permission")
        || lower.contains("sharing")
        || lower.contains("busy")
        || lower.contains("verweigert") // de: "Zugriff verweigert"
        || lower.contains("0x00000005") // ERROR_ACCESS_DENIED (all locales)
}

fn lock_status(
    status: &Arc<Mutex<SpeechMikeStatus>>,
) -> std::sync::MutexGuard<'_, SpeechMikeStatus> {
    status.lock().unwrap_or_else(|e| e.into_inner())
}

fn enable_speechmike_defaults(app: &AppHandle) {
    let mut settings = get_settings(app);
    if settings.speechmike_auto_select && settings.speechmike_button_mapping_enabled {
        return;
    }

    settings.speechmike_auto_select = true;
    settings.speechmike_button_mapping_enabled = true;
    settings.selected_microphone_user_overridden = false;
    write_settings(app, settings);
    log::info!("SpeechMike defaults enabled: auto-select and button mapping");
}

/// CoreAudio can register the USB audio interface later than HID enumeration.
/// Keep retrying while the HID device is physically present so auto-select does
/// not miss a SpeechMike that appears a few seconds after connection.
fn refresh_audio_device(
    app: &AppHandle,
    status: &Arc<Mutex<SpeechMikeStatus>>,
    product_name: &str,
) {
    let Some(audio_name) = find_matching_audio_device(product_name) else {
        return;
    };

    let changed = {
        let mut s = lock_status(status);
        if s.audio_device_name.as_ref() == Some(&audio_name) {
            false
        } else {
            s.audio_device_name = Some(audio_name.clone());
            true
        }
    };

    if changed {
        log::info!("SpeechMike audio device became available: {audio_name}");
        let snapshot = lock_status(status).clone();
        let _ = app.emit("speechmike://connected", snapshot);
    }

    if changed {
        maybe_auto_select_microphone(app, &Some(audio_name));
    }
}

/// Auto-select the SpeechMike audio device if the user hasn't manually chosen one.
fn maybe_auto_select_microphone(app: &AppHandle, audio_name: &Option<String>) {
    let Some(name) = audio_name else {
        return;
    };

    let mut settings = get_settings(app);
    if !settings.speechmike_auto_select || settings.selected_microphone_user_overridden {
        return;
    }

    settings.selected_microphone = Some(name.clone());
    settings.speechmike_last_seen_name = Some(name.clone());
    write_settings(app, settings);

    schedule_microphone_update(app.clone(), name.clone());
    log::info!(
        "SpeechMike auto-selected audio device: {name}; waiting {:?} before opening stream",
        AUDIO_READY_DELAY
    );
}

fn schedule_microphone_update(app: AppHandle, name: String) {
    std::thread::spawn(move || {
        for attempt in 1..=AUDIO_SWITCH_ATTEMPTS {
            let delay = if attempt == 1 {
                AUDIO_READY_DELAY
            } else {
                AUDIO_SWITCH_RETRY_DELAY
            };
            std::thread::sleep(delay);

            let settings = get_settings(&app);
            if settings.selected_microphone.as_ref() != Some(&name) {
                log::debug!(
                    "SpeechMike auto-select: selected microphone changed before retry, skipping {name}"
                );
                return;
            }

            let Some(rm) = app.try_state::<Arc<AudioRecordingManager>>() else {
                log::debug!("SpeechMike auto-select: audio manager not ready");
                return;
            };

            match rm.update_selected_device() {
                Ok(()) => {
                    log::info!(
                        "SpeechMike auto-select: switched audio device to {name} on attempt {attempt}"
                    );
                    return;
                }
                Err(e) => {
                    log::warn!(
                        "SpeechMike auto-select: switch attempt {attempt}/{AUDIO_SWITCH_ATTEMPTS} failed for {name}: {e}"
                    );
                }
            }
        }

        log::error!(
            "SpeechMike auto-select: failed to switch audio device to {name} after {AUDIO_SWITCH_ATTEMPTS} attempts"
        );
    });
}

#[cfg(target_os = "windows")]
fn scan_blocking_processes() -> Vec<String> {
    super::windows_process::scan()
}

#[cfg(not(target_os = "windows"))]
fn scan_blocking_processes() -> Vec<String> {
    vec![]
}
