/// Physical buttons on a Philips SpeechMike device.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SpeechMikeButton {
    Record,
    Stop,
    Eol,
    InsertOverwrite,
    Trigger,
    Forward,
    Rewind,
    /// Report ID we have not yet mapped to a named button.
    Unknown(u8),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ButtonEventKind {
    Press,
    Release,
    StateOn,
    StateOff,
}

#[derive(Debug, Clone)]
pub struct ButtonEvent {
    pub button: SpeechMikeButton,
    pub kind: ButtonEventKind,
    pub raw_report: Vec<u8>,
}

/// Parse a raw HID report buffer into a ButtonEvent.
///
/// SpeechMike devices report button state as small HID reports. The exact
/// byte-to-button layout differs by model/firmware, so the conservative
/// fallback is: any non-zero report from a safe button HID interface is treated
/// as the primary Record button. Zero-state reports are releases and ignored by
/// toggle mode.
pub fn parse_button_event(raw: &[u8]) -> Option<ButtonEvent> {
    if raw.is_empty() {
        return None;
    }

    let report_id = raw[0];
    log::debug!(
        "SpeechMike HID report_id={:#04x} bytes={}",
        report_id,
        raw.iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let Some(code) = active_button_code(raw) else {
        return Some(ButtonEvent {
            button: SpeechMikeButton::Record,
            kind: ButtonEventKind::Release,
            raw_report: raw.to_vec(),
        });
    };

    let button = match (report_id, code) {
        // Keep known mappings here as they are verified from device logs.
        _ => SpeechMikeButton::Record,
    };

    Some(ButtonEvent {
        button,
        kind: ButtonEventKind::Press,
        raw_report: raw.to_vec(),
    })
}

/// Returns a compact code for the active button state. A report id followed by
/// all zero bytes is a release/state-clear report, not a press.
fn active_button_code(raw: &[u8]) -> Option<u16> {
    for (idx, byte) in raw.iter().enumerate().skip(1) {
        if *byte != 0 {
            return Some(((idx as u16) << 8) | (*byte as u16));
        }
    }

    if raw.len() == 1 && raw[0] != 0 {
        return Some(raw[0] as u16);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{parse_button_event, ButtonEventKind, SpeechMikeButton};

    #[test]
    fn maps_non_zero_button_report_to_record_press() {
        let event = parse_button_event(&[0x01, 0x04]).expect("button event");

        assert!(matches!(event.button, SpeechMikeButton::Record));
        assert!(matches!(event.kind, ButtonEventKind::Press));
    }

    #[test]
    fn maps_zero_state_report_to_release() {
        let event = parse_button_event(&[0x01, 0x00]).expect("button event");

        assert!(matches!(event.button, SpeechMikeButton::Record));
        assert!(matches!(event.kind, ButtonEventKind::Release));
    }
}
