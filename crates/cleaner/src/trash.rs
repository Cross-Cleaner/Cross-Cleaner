//! Moving things to the recycle bin instead of deleting them.
//!
//! Deleting is final in a way that makes a wrong pattern unrecoverable, so a
//! rule that says `to_trash` trades a little disk space for the ability to say
//! "that was not what I meant" and get the files back.
//!
//! Each platform has its own bin and its own rules, and none of them is part of
//! the filesystem API: Windows goes through the shell, Linux follows the XDG
//! trash specification, and macOS has a plain `~/.Trash` directory. All three
//! are implemented here so the cleaner can ask one question — [`send`].

use std::io;
use std::path::Path;

/// Whether this build can move files to the recycle bin at all.
///
/// False on Android, where there is no per-user trash and the platform expects
/// deletion to be explicit.
pub const fn is_supported() -> bool {
    cfg!(any(windows, target_os = "linux", target_os = "macos"))
}

/// Moves `path` (a file or a whole directory) to the recycle bin.
///
/// The item keeps its name where the bin allows it; when a name is taken, a
/// numeric suffix is added rather than overwriting what is already there.
pub fn send(path: &Path) -> io::Result<()> {
    imp::send(path)
}

/// Seconds since the Unix epoch, from a modification time.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    #[cfg(target_os = "macos")]
    pub(super) fn send(path: &Path) -> io::Result<()> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME is not set"))?;
        let trash = home.join(".Trash");
        fs::create_dir_all(&trash)?;

        let target = unique(&trash, path);
        // A rename is atomic and instant on the same volume, which is the
        // common case; across volumes it fails, and copying a tree that large
        // is not something a cleanup should silently start doing.
        fs::rename(path, &target)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn send(path: &Path) -> io::Result<()> {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok_or_else(|| io::Error::other("neither XDG_DATA_HOME nor HOME is set"))?;

        let trash = data_home.join("Trash");
        let files = trash.join("files");
        let info = trash.join("info");
        fs::create_dir_all(&files)?;
        fs::create_dir_all(&info)?;

        // The recorded path has to be absolute, and it has to be read before the
        // move: afterwards the path no longer exists to canonicalise.
        let absolute = std::path::absolute(path)?;
        let name = unique(&files, path);
        let target = files.join(&name);

        fs::rename(path, &target)?;

        // A failure here leaves the file in the bin with no record of where it
        // came from: recoverable by hand, but not by "restore". Reported rather
        // than ignored so the run can say so.
        let record = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            percent_encode(&absolute.to_string_lossy()),
            timestamp(),
        );
        fs::write(
            info.join(format!("{}.trashinfo", name.to_string_lossy())),
            record,
        )
    }

    /// A name that is free in `dir`, adding `.1`, `.2`, … as needed.
    fn unique(dir: &Path, path: &Path) -> PathBuf {
        let name = path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("unnamed"));

        if !dir.join(&name).exists() {
            return name;
        }

        let stem = Path::new(&name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".into());
        let extension = Path::new(&name)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();

        for n in 1..10_000 {
            let candidate = format!("{stem}.{n}{extension}");
            if !dir.join(&candidate).exists() {
                return PathBuf::from(candidate);
            }
        }

        name
    }

    /// Percent-encodes everything outside the unreserved set, which is what the
    /// trash specification asks for in the `Path=` line.
    fn percent_encode(text: &str) -> String {
        let mut out = String::with_capacity(text.len());

        for byte in text.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }

        out
    }

    /// `YYYY-MM-DDThh:mm:ss` in local time, which is the format the trash
    /// specification fixes. Hand-rolled because a date library for one line is
    /// not worth the dependency in a size-optimised binary.
    fn timestamp() -> String {
        let now = super::unix_now();

        // Local offset, read from the platform rather than guessed: a trashed
        // file with a UTC timestamp looks a few hours in the future or past.
        let offset = local_offset_seconds();
        let local = (now as i64 + offset).max(0) as u64;

        let days = (local / 86_400) as i64;
        let seconds = local % 86_400;
        let (year, month, day) = civil_from_days(days);

        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60,
        )
    }

    /// Days since the Unix epoch to a civil date. Howard Hinnant's algorithm,
    /// which is exact for every year the calendar is used for.
    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };

        (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
    }

    /// The local time zone's offset from UTC, in seconds, right now.
    fn local_offset_seconds() -> i64 {
        // `date +%z` is the one portable answer short of linking libc; the
        // result is used for a timestamp, so a failure falls back to UTC.
        let output = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        match output.as_deref() {
            Some(text) if text.len() == 5 => {
                let sign = if text.starts_with('-') { -1 } else { 1 };
                let hours: i64 = text[1..3].parse().unwrap_or(0);
                let minutes: i64 = text[3..5].parse().unwrap_or(0);
                sign * (hours * 3600 + minutes * 60)
            }
            _ => 0,
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    use windows::core::PCWSTR;

    pub(super) fn send(path: &Path) -> io::Result<()> {
        // SHFileOperationW wants a list of double-null-terminated paths, and it
        // refuses to grow its own buffer: the two trailing NULs are part of the
        // format, not padding.
        let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();

        if from.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains a NUL byte",
            ));
        }

        from.push(0);
        from.push(0);

        let operation = SHFILEOPSTRUCTW {
            wFunc: FO_DELETE as u32,
            pFrom: PCWSTR(from.as_ptr()),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
            ..Default::default()
        };

        // SAFETY: `from` outlives the call and is double-NUL-terminated, and
        // `pTo` is left null because nothing is being copied or moved to a named
        // destination.
        let code = unsafe { SHFileOperationW(&operation as *const _ as *mut _) };

        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the recycle bin refused the item (0x{code:04X})"
            )))
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use std::io;
    use std::path::Path;

    pub(super) fn send(_path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform has no recycle bin",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sending_a_missing_path_fails() {
        assert!(send(Path::new("definitely-not-here-9f2c.tmp")).is_err());
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn sending_moves_the_file_out_of_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appcore-trash-probe.tmp");
        std::fs::write(&path, b"data").unwrap();

        send(&path).unwrap();

        // The point of a trash is that the original path is free: whether the
        // item can be restored by hand is the platform's business.
        assert!(!path.exists());
    }
}
