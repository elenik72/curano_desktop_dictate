/// Best-effort scan of running processes for known Philips / Dragon programs.
/// Returns process names found; may return an empty list if tasklist fails.
#[cfg(target_os = "windows")]
pub fn scan() -> Vec<String> {
    let output = match crate::devices::windows_process::tasklist_csv_output() {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    find_known_blockers(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "windows", test))]
fn find_known_blockers(stdout: &str) -> Vec<String> {
    const KNOWN_BLOCKERS: &[&str] = &[
        "SpeechExec.exe",
        "SpeechControl.exe",
        "DeviceControlCenter.exe",
        "SEDict.exe",
    ];

    let lower_stdout = stdout.to_lowercase();

    let mut found: Vec<String> = KNOWN_BLOCKERS
        .iter()
        .filter(|blocker| lower_stdout.contains(&blocker.to_lowercase()))
        .map(|s| s.to_string())
        .collect();

    // Catch Dragon variants (Dragon NaturallySpeaking, Dragon Medical, etc.)
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("dragon") {
            if let Some(name) = line.split('"').nth(1) {
                let name = name.to_string();
                if !found.contains(&name) {
                    found.push(name);
                }
            }
        }
    }

    found
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn scan() -> Vec<String> {
    vec![]
}

#[cfg(test)]
mod tests {
    use super::find_known_blockers;

    #[test]
    fn finds_known_blockers_case_insensitively() {
        let stdout = concat!(
            "\"speechexec.EXE\",\"100\",\"Console\",\"1\",\"1,000 K\"\r\n",
            "\"SPEECHCONTROL.exe\",\"101\",\"Console\",\"1\",\"1,000 K\"\r\n",
        );

        assert_eq!(
            find_known_blockers(stdout),
            vec!["SpeechExec.exe", "SpeechControl.exe"]
        );
    }

    #[test]
    fn finds_and_deduplicates_dragon_variants() {
        let stdout = concat!(
            "\"DragonMedical.exe\",\"200\",\"Console\",\"1\",\"1,000 K\"\r\n",
            "\"DragonMedical.exe\",\"201\",\"Console\",\"1\",\"1,000 K\"\r\n",
            "\"DragonBar.exe\",\"202\",\"Console\",\"1\",\"1,000 K\"\r\n",
        );

        assert_eq!(
            find_known_blockers(stdout),
            vec!["DragonMedical.exe", "DragonBar.exe"]
        );
    }

    #[test]
    fn ignores_malformed_dragon_rows() {
        let stdout = concat!(
            "dragon process without CSV fields\r\n",
            "\"SpeechControl.exe\",\"300\",\"Console\",\"1\",\"1,000 K\"\r\n",
        );

        assert_eq!(find_known_blockers(stdout), vec!["SpeechControl.exe"]);
    }
}
