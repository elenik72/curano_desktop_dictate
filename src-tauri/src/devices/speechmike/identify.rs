/// Philips USB Vendor ID shared by all SpeechMike, SpeechOne, and SpeechControl devices.
pub const PHILIPS_SPEECHMIKE_VID: u16 = 0x0911;

/// Substrings looked for in audio device names when auto-selecting a microphone.
const AUDIO_NAME_KEYWORDS: &[&str] = &["SpeechMike", "Speech Mike", "SpeechOne", "Philips"];

/// A resolved HID interface candidate ready for opening, with device metadata.
#[derive(Clone, Debug)]
pub struct DeviceCandidate {
    /// Platform-specific device path (owned so it outlives the `HidApi` borrow).
    pub path: std::ffi::CString,
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface_number: i32,
    pub product_name: String,
    pub serial: Option<String>,
}

/// Return all Philips SpeechMike HID interfaces sorted by preference for button reads.
///
/// Priority order:
/// 1. Consumer Control (`usage_page == 0x000C`) — most likely to deliver button reports.
/// 2. Interface 0 — common fallback on Windows when usage_page is 0.
/// 3. Everything else.
///
/// Returning multiple candidates lets the caller fall back to the next interface if
/// the preferred one is locked by another process.
pub fn pick_speechmike_interfaces(api: &hidapi::HidApi) -> Vec<DeviceCandidate> {
    let mut infos: Vec<_> = api
        .device_list()
        .filter(|d| d.vendor_id() == PHILIPS_SPEECHMIKE_VID)
        .collect();

    infos.sort_by_key(|d| interface_sort_key(d.usage_page(), d.usage(), d.interface_number()));

    for d in &infos {
        log::debug!(
            "SpeechMike HID candidate: product={} vid={:#06x} pid={:#06x} usage_page={:#06x} usage={:#06x} interface={}",
            d.product_string().unwrap_or("Philips SpeechMike"),
            d.vendor_id(),
            d.product_id(),
            d.usage_page(),
            d.usage(),
            d.interface_number(),
        );
    }

    infos
        .into_iter()
        .map(|d| DeviceCandidate {
            path: d.path().to_owned(),
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            usage_page: d.usage_page(),
            usage: d.usage(),
            interface_number: d.interface_number(),
            product_name: d
                .product_string()
                .unwrap_or("Philips SpeechMike")
                .to_string(),
            serial: d.serial_number().map(|s| s.to_string()),
        })
        .collect()
}

fn interface_sort_key(usage_page: u16, usage: u16, interface_number: i32) -> u8 {
    if usage_page >= 0xFF00 {
        0u8
    } else if usage_page == 0x000C {
        // Consumer Control is useful for media-like keys, but the
        // proprietary SpeechMike buttons usually arrive on vendor pages.
        1
    } else if interface_number == 0 {
        2
    } else if is_pointer_or_keyboard_usage(usage_page, usage) {
        // Mouse/keyboard interfaces are intentionally last. macOS and
        // browsers often open them for normal pointer/key delivery.
        4
    } else {
        3
    }
}

fn is_pointer_or_keyboard_usage(usage_page: u16, usage: u16) -> bool {
    usage_page == 0x0001 && matches!(usage, 0x0001 | 0x0002 | 0x0006 | 0x0007)
}

/// Whether this interface is safe to open for SpeechMike button polling.
///
/// On macOS, SpeechMike III can expose pointer/keyboard-like HID interfaces.
/// Opening those can interfere with the system pointer path. Vendor-defined
/// and Consumer Control interfaces are acceptable button sources; pointer and
/// keyboard usages remain blocked.
#[cfg(target_os = "macos")]
pub fn is_button_hid_interface(candidate: &DeviceCandidate) -> bool {
    candidate.usage_page >= 0xFF00 || candidate.usage_page == 0x000C
}

#[cfg(not(target_os = "macos"))]
pub fn is_button_hid_interface(candidate: &DeviceCandidate) -> bool {
    !is_pointer_or_keyboard_usage(candidate.usage_page, candidate.usage)
}

#[cfg(test)]
mod tests {
    use super::interface_sort_key;

    #[test]
    fn prefers_vendor_defined_controls_over_os_pointer_and_keyboard_interfaces() {
        assert!(interface_sort_key(0xFFA0, 0x0002, 3) < interface_sort_key(0x000C, 0x0001, 1));
        assert!(interface_sort_key(0x000C, 0x0001, 1) < interface_sort_key(0x0001, 0x0002, 4));
        assert!(interface_sort_key(0x000C, 0x0001, 1) < interface_sort_key(0x0001, 0x0006, 0));
    }
}

/// Find an audio input device whose name matches the connected SpeechMike.
///
/// Strategy:
/// 1. Case-insensitive substring match against `product_string` from HID.
/// 2. Fallback: any device whose name contains a known Philips keyword.
pub fn find_matching_audio_device(product_string: &str) -> Option<String> {
    let devices = crate::audio_toolkit::list_input_devices().ok()?;
    let product_lower = product_string.to_lowercase();

    // Prefer a device whose name overlaps with the HID product string.
    if let Some(d) = devices.iter().find(|d| {
        let n = d.name.to_lowercase();
        n.contains(&product_lower) || product_lower.contains(n.as_str())
    }) {
        return Some(d.name.clone());
    }

    // Fallback: any device with a known Philips keyword.
    devices
        .into_iter()
        .find(|d| AUDIO_NAME_KEYWORDS.iter().any(|kw| d.name.contains(kw)))
        .map(|d| d.name)
}
