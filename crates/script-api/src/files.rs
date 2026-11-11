//! Reading the filesystem from scripts.
//!
//! Reading a file's contents and parsing JSON is `std::fs::read_to_string` and
//! `json` from [rune-modules] — the same calls a Rust program uses. What this
//! module adds is the part a standard library does not carry: the questions a
//! cleaner has to ask and the answer to *what did I just delete*.
//!
//! Everything here is synchronous, because a `clean` handler runs inside a
//! cleaning job and its counts must be the counts of what was really removed.
//!
//! [rune-modules]: https://docs.rs/rune-modules

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Reads a file as UTF-8 text. `None` if it is missing, unreadable or not text.
pub(crate) fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Whether `path` is older than `days` days.
///
/// A missing path is not old, it simply is not there: the answer is `false`,
/// because the script's next step is a deletion that would do nothing.
pub(crate) fn older_than(path: &str, days: u64) -> bool {
    let Some(modified) = modified(path) else {
        return false;
    };

    let now = now();

    // WARN: `>=` rather than `>`: "older than zero days" is true of any file that
    // already exists, which is what a script writing `older_than(file, 0)` means.
    // A modification time in the future subtracts to nothing and stays false.
    now.saturating_sub(modified) >= days * 24 * 60 * 60
}

/// Whether the last component of `path` matches `pattern`.
///
/// The pattern is a name glob (`*.log`), not a path glob: comparing a whole
/// path against a pattern is what [`glob`] is for.
pub(crate) fn matches(path: &str, pattern: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();

    glob::Pattern::new(pattern)
        .map(|pattern| pattern.matches(&name))
        .unwrap_or(false)
}

/// Whether `path` can be opened for writing right now.
///
/// A file held open by another process — a log the app is still writing, a
/// cache the browser keeps mapped — fails here, which is when a script should
/// leave it alone. The file is opened without truncating: this asks, it does
/// not damage.
///
/// Also `false` for a missing file and for one the user may not write.
pub(crate) fn unlocked(path: &str) -> bool {
    unlocked_path(Path::new(path))
}

/// [`unlocked`] for a path already in hand, so a walk does not allocate a
/// `String` per file just to ask the question.
fn unlocked_path(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

/// Whether `path` is locked by another process.
pub(crate) fn locked(path: &str) -> bool {
    Path::new(path).exists() && !unlocked(path)
}

/// Every file under `path` that is locked right now, as full paths, sorted.
///
/// What a script uses to leave a whole cache alone while one file in it is
/// still open, rather than deleting half of it.
pub(crate) fn lock_files(path: &str) -> Vec<String> {
    let mut locked_paths = Vec::new();
    let mut pending = vec![std::path::PathBuf::from(path)];

    // Walked in place: building the full file list first would hold one path
    // for every file under the tree, which for a browser cache is most of the
    // memory the script uses.
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() && !unlocked_path(&path) {
                locked_paths.push(path.to_string_lossy().into_owned());
            }
        }
    }

    locked_paths.sort();
    locked_paths
}

/// The current user's name — what `{username}` expands to.
pub(crate) fn user() -> String {
    database::cleaner_database::placeholder_username()
}

/// The current user's home directory — what `{home}` expands to.
pub(crate) fn home() -> String {
    database::cleaner_database::placeholder_home()
}

/// The per-user application data directory: `AppData\Roaming` on Windows,
/// the XDG config directory on Linux, `Application Support` on macOS.
pub(crate) fn appdata() -> String {
    crate::loader::platform_data_dir()
        .to_string_lossy()
        .into_owned()
}

/// Joins path segments with the platform separator.
pub(crate) fn join(parts: Vec<String>) -> String {
    let mut path = std::path::PathBuf::new();

    for part in parts {
        path.push(part);
    }

    path.to_string_lossy().into_owned()
}

/// The directory containing `path`, if it has one.
pub(crate) fn parent(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
}

/// The last component of `path`, file name included.
pub(crate) fn filename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Expands `{home}`, `{username}`, `{steam}` and `{drive}` the way a rule path
/// is expanded, one entry per drive.
pub(crate) fn expand(pattern: &str) -> Vec<String> {
    database::cleaner_database::expand_path(pattern)
}

/// Whether `path` is a system location a cleaner has no business removing.
///
/// This is a question a handler can ask, not a wall: it answers `true` for the
/// places where a wrong path stops being a bug and becomes a reinstall — the
/// Windows directory, the program files, the user's home itself, the mount
/// points of the system — and for anything that contains one of them (removing
/// `C:\Users` is as bad as removing `C:\Users\me\Documents`).
///
/// A path that does not exist is not protected: there is nothing there to lose.
pub(crate) fn is_protected(path: &str) -> bool {
    let Ok(resolved) = fs::canonicalize(path) else {
        return false;
    };

    protected_roots()
        .iter()
        .any(|root| *root == resolved || root.starts_with(&resolved))
}

/// The places [`is_protected`] treats as off limits, as far as they exist.
fn protected_roots() -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();

    let mut push = |path: std::path::PathBuf| {
        if let Ok(resolved) = fs::canonicalize(&path)
            && !roots.contains(&resolved)
        {
            roots.push(resolved);
        }
    };

    #[cfg(windows)]
    {
        for variable in [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
        ] {
            if let Some(value) = std::env::var_os(variable) {
                push(std::path::PathBuf::from(value));
            }
        }

        if let Some(drive) = std::env::var_os("SystemDrive") {
            push(std::path::PathBuf::from(drive).join("\\"));
        }
    }

    #[cfg(unix)]
    {
        for root in [
            "/",
            "/bin",
            "/boot",
            "/etc",
            "/lib",
            "/lib64",
            "/opt",
            "/sbin",
            "/System",
            "/Library",
            "/Applications",
            "/usr",
            "/var",
        ] {
            push(std::path::PathBuf::from(root));
        }
    }

    // The home directory itself, never the things inside it: a cleaner that
    // targets `{home}/Downloads` is doing its job, one that targets `{home}` is
    // not.
    let home = std::path::PathBuf::from(home());
    push(home.clone());

    // On Windows the home directory sits under `C:\Users`, and removing that is
    // every account at once.
    #[cfg(windows)]
    if let Some(parent) = home.parent()
        && parent
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("Users"))
    {
        push(parent.to_path_buf());
    }

    // Cross Cleaner's own data directory, where the scripts and their state
    // live: a script cleaning itself out is never what the user meant.
    push(crate::loader::platform_data_dir());

    roots
}

/// Whether any of `programs` is running right now.
///
/// The same check a rule's `skip_if_running` makes, so a handler and a rule
/// agree on what "the browser is open" means. An empty list is `false`.
pub(crate) fn running(programs: Vec<String>) -> bool {
    cleaner::processes::running(&programs)
}

/// Writes `data` to `path`, replacing the file. `true` on success.
pub(crate) fn write(path: &str, data: &str) -> bool {
    fs::write(path, data).is_ok()
}

/// The entries of a directory as full paths, sorted.
///
/// A missing or unreadable directory is an empty list, not an error: scripts
/// are usually pointed at paths that may not exist on this machine, and a
/// `match` on the result is friendlier than an error they cannot handle.
pub(crate) fn list(path: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };

    let mut paths: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();

    paths.sort();
    paths
}

/// Whether `path` is a regular file.
pub(crate) fn is_file(path: &str) -> bool {
    Path::new(path).is_file()
}

/// Whether `path` is a directory.
pub(crate) fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// Last modification time in seconds since the Unix epoch.
pub(crate) fn modified(path: &str) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Now, in seconds since the Unix epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Expands a glob pattern into full paths, sorted.
pub(crate) fn glob(pattern: &str) -> Vec<String> {
    let Ok(paths) = glob::glob(pattern) else {
        database::diag::warn(format!("script: invalid glob pattern \"{pattern}\""));
        return Vec::new();
    };

    let mut paths: Vec<String> = paths
        .filter_map(Result::ok)
        .map(|path| path.to_string_lossy().into_owned())
        .collect();

    paths.sort();
    paths
}

/// A client with a timeout, so a dead server cannot hang a hook forever.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into()
}

/// Fetches `url` and returns its body as text.
pub(crate) fn http_get(url: &str) -> Option<String> {
    match agent()
        .get(url)
        .header(
            "User-Agent",
            concat!("Cross-Cleaner/", env!("CARGO_PKG_VERSION")),
        )
        .call()
    {
        Ok(mut response) => response.body_mut().read_to_string().ok(),
        Err(error) => {
            database::diag::warn(format!("script: GET {url} failed: {error}"));
            None
        }
    }
}

/// POSTs `body` to `url` as JSON.
pub(crate) fn http_post(url: &str, body: &str) -> bool {
    match agent()
        .post(url)
        .header(
            "User-Agent",
            concat!("Cross-Cleaner/", env!("CARGO_PKG_VERSION")),
        )
        .header("Content-Type", "application/json")
        .send(body)
    {
        Ok(_) => true,
        Err(error) => {
            database::diag::warn(format!("script: POST {url} failed: {error}"));
            false
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_finds_files_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("top.tmp"), "x").unwrap();
        fs::write(dir.path().join("nested/deep.tmp"), "x").unwrap();

        let pattern = format!("{}/**/*.tmp", dir.path().to_string_lossy());
        let found = glob(&pattern);

        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn list_is_empty_for_a_missing_directory() {
        assert!(list("/definitely/not/here/at/all").is_empty());
    }
}
