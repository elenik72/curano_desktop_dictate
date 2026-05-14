/// Philips USB Vendor ID shared by all SpeechMike, SpeechOne, and SpeechControl devices.
pub const PHILIPS_SPEECHMIKE_VID: u16 = 0x0911;

/// Substrings looked for in audio device names when auto-selecting a microphone.
const AUDIO_NAME_KEYWORDS: &[&str] = &["SpeechMike", "Speech Mike", "SpeechOne", "Philips"];

/// A resolved HID interface candidate ready for opening, with device metadata.
pub struct DeviceCandidate {
    /// Platform-specific device path (owned so it outlives the `HidApi` borrow).
    pub path: std::ffi::CString,
    pub vendor_id: u16,
    pub product_id: u16,
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

    infos.sort_by_key(|d| {
        if d.usage_page() == 0x000C {
            0u8
        } else if d.interface_number() == 0 {
            1
        } else {
            2
        }
    });

    infos
        .into_iter()
        .map(|d| DeviceCandidate {
            path: d.path().to_owned(),
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            product_name: d
                .product_string()
                .unwrap_or("Philips SpeechMike")
                .to_string(),
            serial: d.serial_number().map(|s| s.to_string()),
        })
        .collect()
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
