use std::io;
use std::os::windows::process::CommandExt;
use std::process::{Command, Output};

// Prevent console applications such as tasklist.exe from creating a visible
// console window when Curano is built as a Windows GUI application.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run `tasklist` in CSV mode without creating a console window.
pub(crate) fn tasklist_csv_output() -> io::Result<Output> {
    let mut command = Command::new("tasklist");
    command
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW);
    command.output()
}
