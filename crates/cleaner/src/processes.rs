//! Asking the operating system whether a program is running.
//!
//! A cache is only clean while the program that owns it is closed: cleaning a
//! browser profile under a running browser mostly just makes the browser write
//! everything back on the next tick. `skip_if_running` asks this question before
//! an entry runs, and `cleaner::running` lets a handler ask it for itself.
//!
//! The answer comes from the platform's own process list rather than from a
//! snapshot API: one short-lived command per run is cheaper than a dependency
//! that links a whole process-enumeration layer into a size-optimised binary, and
//! the question is asked once per entry, not per file.

use std::io;

/// Whether any of `names` is running right now.
///
/// A name is compared case-insensitively with `.exe` ignored on both sides, so
/// `chrome`, `Chrome` and `chrome.exe` all mean the same program.
pub fn running<S: AsRef<str>>(names: &[S]) -> bool {
    if names.is_empty() {
        return false;
    }

    match list() {
        Ok(processes) => names.iter().any(|name| {
            let wanted = normalize(name.as_ref());
            processes.iter().any(|process| *process == wanted)
        }),
        // An unreadable process list answers "nothing is running": skipping a
        // cleanup because the question could not be asked would silently disable
        // the rule on a machine where the command is missing.
        Err(e) => {
            database::diag::warn(format!("cleaner: cannot list processes: {e}"));
            false
        }
    }
}

/// Running process names, normalized for comparison.
fn list() -> io::Result<Vec<String>> {
    let output = command()?;
    let text = String::from_utf8_lossy(&output.stdout);

    Ok(text.lines().map(|line| normalize(line.trim())).collect())
}

fn normalize(name: &str) -> String {
    let lower = name.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

#[cfg(windows)]
fn command() -> io::Result<std::process::Output> {
    // `/NH` drops the header, so every line is a process. CSV keeps names with
    // spaces (and commas) in one quoted field, which is why the first field is
    // taken rather than the first whitespace-separated word.
    let output = std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()?;

    Ok(std::process::Output {
        stdout: first_field(&output.stdout),
        ..output
    })
}

/// Keeps the quoted image name of each CSV line, one per line.
#[cfg(windows)]
fn first_field(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();

    for line in text.lines() {
        let name = line
            .split(',')
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .trim();

        if !name.is_empty() {
            out.push_str(name);
            out.push('\n');
        }
    }

    out.into_bytes()
}

#[cfg(not(windows))]
fn command() -> io::Result<std::process::Output> {
    // `comm` is the executable name without arguments, which is exactly what a
    // rule names; `-A` covers every process the user can see.
    std::process::Command::new("ps")
        .args(["-A", "-o", "comm="])
        .output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_requested_is_nothing_running() {
        assert!(!running::<&str>(&[]));
    }

    #[test]
    fn this_test_process_is_running() {
        // The test binary itself is a process the list must contain. Its exact
        // name varies by runner, so only the negative answer is asserted on:
        // a name that matches nothing is what a rule relies on to be skipped.
        assert!(!running(&["definitely-not-a-real-process-8c41"]));
    }

    #[test]
    fn names_ignore_case_and_exe() {
        assert_eq!(normalize("Chrome.EXE"), "chrome");
        assert_eq!(normalize("chrome"), "chrome");
    }
}
