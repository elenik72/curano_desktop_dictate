use super::buttons::{ButtonEvent, ButtonEventKind, SpeechMikeButton};
use crate::actions::{fire_action, ActionIntent, ActionTriggerSource};
use crate::TranscriptionCoordinator;
use tauri::{AppHandle, Manager};

/// Route a decoded SpeechMike button event to the appropriate recording action.
pub fn dispatch_button_event(app: &AppHandle, event: ButtonEvent) {
    match (&event.button, &event.kind) {
        (SpeechMikeButton::Record, ButtonEventKind::Press | ButtonEventKind::StateOn) => {
            send_record_toggle(app);
        }
        // The SpeechMike REC button is a hardware toggle in this app: press to
        // start, press again to stop. Release/state-off reports are ignored.
        (SpeechMikeButton::Record, ButtonEventKind::Release | ButtonEventKind::StateOff) => {}
        (SpeechMikeButton::Stop, ButtonEventKind::Press | ButtonEventKind::StateOn) => {
            fire_action(
                app,
                ActionIntent::Cancel,
                true,
                ActionTriggerSource::SpeechMike,
            );
        }
        (SpeechMikeButton::Eol, ButtonEventKind::Press | ButtonEventKind::StateOn) => {
            fire_action(
                app,
                ActionIntent::TranscribeWithPostProcess,
                true,
                ActionTriggerSource::SpeechMike,
            );
        }
        // Trigger always behaves as push-to-talk regardless of the global setting.
        (SpeechMikeButton::Trigger, ButtonEventKind::Press) => {
            fire_action(
                app,
                ActionIntent::Transcribe,
                true,
                ActionTriggerSource::SpeechMike,
            );
        }
        (SpeechMikeButton::Trigger, ButtonEventKind::Release) => {
            fire_action(
                app,
                ActionIntent::Transcribe,
                false,
                ActionTriggerSource::SpeechMike,
            );
        }
        (SpeechMikeButton::Unknown(id), _) => {
            log::debug!(
                "SpeechMike unmapped button report_id={:#04x}: {}",
                id,
                event
                    .raw_report
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        // InsertOverwrite, Forward, Rewind: reserved, no-op.
        _ => {}
    }
}

fn send_record_toggle(app: &AppHandle) {
    if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
        coordinator.send_input("transcribe", "speechmike", true, false);
    } else {
        log::warn!("SpeechMike record button: TranscriptionCoordinator not initialized");
    }
}
