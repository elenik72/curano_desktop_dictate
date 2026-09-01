//! Enumerate processes with active audio-capture sessions (Windows).
//!
//! Used by the SpeechMike settings UI to warn when another application is
//! also capturing from the microphone (Philips SpeechControl, Teams, etc.) —
//! a common cause of "the microphone delivers silence" reports.

/// Names of processes that currently hold an active capture session on any
/// active input endpoint, excluding this process. Best effort: returns an
/// empty list on any COM failure.
#[cfg(target_os = "windows")]
pub fn list_capture_session_processes() -> Vec<String> {
    use std::collections::HashSet;

    let pids = unsafe { collect_capture_session_pids() };
    if pids.is_empty() {
        return vec![];
    }

    let own_pid = std::process::id();
    let name_by_pid = tasklist_pid_names();

    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for pid in pids {
        if pid == own_pid || pid == 0 {
            continue;
        }
        let name = name_by_pid
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| format!("PID {pid}"));
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }
    names
}

#[cfg(target_os = "windows")]
unsafe fn collect_capture_session_pids() -> Vec<u32> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // Idempotent per thread; ignore "already initialized" results.
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let mut pids = Vec::new();

    let enumerator: IMMDeviceEnumerator =
        match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(_) => return pids,
        };

    let devices = match enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
        Ok(d) => d,
        Err(_) => return pids,
    };

    let device_count = devices.GetCount().unwrap_or(0);
    for device_idx in 0..device_count {
        let Ok(device) = devices.Item(device_idx) else {
            continue;
        };
        let Ok(session_manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            continue;
        };
        let Ok(session_enum) = session_manager.GetSessionEnumerator() else {
            continue;
        };
        let session_count = session_enum.GetCount().unwrap_or(0);
        for session_idx in 0..session_count {
            let Ok(session) = session_enum.GetSession(session_idx) else {
                continue;
            };
            let Ok(state) = session.GetState() else {
                continue;
            };
            if state != AudioSessionStateActive {
                continue;
            }
            let Ok(session2) = session.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if let Ok(pid) = session2.GetProcessId() {
                pids.push(pid);
            }
        }
    }

    pids
}

/// PID → executable name map via `tasklist` CSV output. Avoids extra COM /
/// process-handle permissions; best effort.
#[cfg(target_os = "windows")]
fn tasklist_pid_names() -> std::collections::HashMap<u32, String> {
    let Ok(output) = crate::devices::windows_process::tasklist_csv_output() else {
        return std::collections::HashMap::new();
    };

    parse_tasklist_pid_names(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "windows", test))]
fn parse_tasklist_pid_names(stdout: &str) -> std::collections::HashMap<u32, String> {
    let mut map = std::collections::HashMap::new();
    for line in stdout.lines() {
        let mut fields = line.split('"').filter(|s| !s.is_empty() && *s != ",");
        let (Some(name), Some(pid_str)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            map.insert(pid, name.to_string());
        }
    }
    map
}

#[cfg(not(target_os = "windows"))]
pub fn list_capture_session_processes() -> Vec<String> {
    vec![]
}

#[cfg(test)]
mod tests {
    use super::parse_tasklist_pid_names;

    #[test]
    fn parses_process_names_and_pids_from_tasklist_csv() {
        let stdout = concat!(
            "\"SpeechControl.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"\r\n",
            "\"Teams.exe\",\"9876\",\"Console\",\"1\",\"20,000 K\"\r\n",
        );

        let processes = parse_tasklist_pid_names(stdout);

        assert_eq!(
            processes.get(&1234).map(String::as_str),
            Some("SpeechControl.exe")
        );
        assert_eq!(processes.get(&9876).map(String::as_str), Some("Teams.exe"));
    }

    #[test]
    fn ignores_malformed_tasklist_rows() {
        let stdout = concat!(
            "not csv\r\n",
            "\"MissingPid.exe\",\"not-a-pid\",\"Console\"\r\n",
            "\"Valid.exe\",\"42\",\"Console\",\"1\",\"1,000 K\"\r\n",
        );

        let processes = parse_tasklist_pid_names(stdout);

        assert_eq!(processes.len(), 1);
        assert_eq!(processes.get(&42).map(String::as_str), Some("Valid.exe"));
    }
}
