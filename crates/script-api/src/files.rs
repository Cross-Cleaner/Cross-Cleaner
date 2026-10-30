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
