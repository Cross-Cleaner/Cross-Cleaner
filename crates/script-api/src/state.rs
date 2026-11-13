//! A small key/value store a script may keep between runs.
//!
//! Cleaning is repetitive: the same cache is checked every week, and a script
//! that wants to say "the last scan found nothing, skip until Friday" or
//! "these files were already reported" needs somewhere to write that down.
//! The store is one JSON object per script, kept under the scripts directory
//! so uninstalling a script (removing its file) leaves its state behind only
//! as an orphan the user can delete with the rest.
//!
//! Values are strings on purpose: numbers and dates format differently in
//! every locale, and a script that stores a timestamp can parse it back with
//! `time` or `std::int::parse`. The store is not a database and does not try
//! to be one.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Where this script's state file lives, or `None` when no script is running
/// (the state functions are only reachable from a script).
fn state_file() -> Option<PathBuf> {
    let script = crate::session::script_path()?;
    let name = PathBuf::from(&script)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())?;

    let name: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();

    Some(
        crate::loader::scripts_dir()
            .join(".state")
            .join(format!("{name}.json")),
    )
}

fn read() -> BTreeMap<String, String> {
    let Some(path) = state_file() else {
        return BTreeMap::new();
    };

    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };

    serde_json::from_str(&text).unwrap_or_default()
}

fn write(state: &BTreeMap<String, String>) -> bool {
    let Some(path) = state_file() else {
        return false;
    };

    let Some(directory) = path.parent() else {
        return false;
    };

    if fs::create_dir_all(directory).is_err() {
        return false;
    }

    let Ok(text) = serde_json::to_string(state) else {
        return false;
    };

    // WARN: write beside the target and rename, so a killed run leaves the old
    // state intact instead of a truncated file that reads as empty.
    let temporary = path.with_extension("json.tmp");

    if fs::write(&temporary, text).is_err() {
        return false;
    }

    if fs::rename(&temporary, &path).is_err() {
        let _ = fs::remove_file(&temporary);
        return false;
    }

    true
}

/// The value stored under `key`, or `None`.
pub(crate) fn get(key: &str) -> Option<String> {
    read().remove(key)
}

/// Stores `value` under `key`. `true` on success.
pub(crate) fn set(key: &str, value: &str) -> bool {
    let mut state = read();
    state.insert(key.to_owned(), value.to_owned());
    write(&state)
}

/// Removes `key`. `true` when it was there and the file was written.
pub(crate) fn delete(key: &str) -> bool {
    let mut state = read();

    if state.remove(key).is_none() {
        return false;
    }

    write(&state)
}

/// Every key in this script's state, sorted.
pub(crate) fn keys() -> Vec<String> {
    read().into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_is_per_script_and_sanitized() {
        crate::session::set_running_script("C:/Scripts/my cleaner.rn");
        let path = state_file().unwrap();
        crate::session::clear_running_script();

        assert!(path.ends_with(".state/my_cleaner.rn.json"), "{path:?}");
    }

    #[test]
    fn a_missing_key_is_none() {
        crate::session::set_running_script("missing-state.rn");

        assert_eq!(get("never-written"), None);
        assert!(!delete("never-written"));

        crate::session::clear_running_script();
    }
}
