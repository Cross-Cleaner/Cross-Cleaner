//! Registry access for scripts.
//!
//! The declarative `cleaner::registry_cleaner(...)` covers the common case: a
//! key, and what to remove under it. When a script has to *look* before it
//! deletes — check a value, walk subkeys, remove only the ones that match — it
//! uses `cleaner::registry` instead.
//!
//! Every successful removal is recorded, so the cleaning result lists the key
//! or value that went away even though a registry entry has no size.

use std::cell::RefCell;

thread_local! {
    /// Registry paths removed by the handler currently running.
    static REMOVED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn reset() {
    REMOVED.with(|removed| removed.borrow_mut().clear());
}

pub(crate) fn take() -> Vec<String> {
    REMOVED.with(|removed| std::mem::take(&mut *removed.borrow_mut()))
}

fn note(path: String) {
    REMOVED.with(|removed| removed.borrow_mut().push(path));
}

#[cfg(windows)]
mod imp {
    use winreg::RegKey;
    use winreg::enums::{
        HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_USERS,
        KEY_READ, KEY_SET_VALUE, KEY_WRITE,
    };

    use super::note;

    /// The hives an entry may name, with the abbreviation regedit also accepts.
    const HIVES: [(&str, &str, winreg::HKEY); 5] = [
        ("HKEY_CURRENT_USER", "HKCU", HKEY_CURRENT_USER),
        ("HKEY_CURRENT_CONFIG", "HKCC", HKEY_CURRENT_CONFIG),
        ("HKEY_LOCAL_MACHINE", "HKLM", HKEY_LOCAL_MACHINE),
        ("HKEY_CLASSES_ROOT", "HKCR", HKEY_CLASSES_ROOT),
        ("HKEY_USERS", "HKU", HKEY_USERS),
    ];

    /// Splits `HKEY_CURRENT_USER\Software\App` into the hive, its handle and
    /// the path relative to it.
    fn split(path: &str) -> Option<(&'static str, RegKey, String)> {
        let trimmed = path.trim();
        let (hive, rest) = trimmed.split_once('\\')?;
        let hive = hive.trim().to_ascii_uppercase();
        let rest = rest.trim().trim_matches('\\');

        // A bare hive names every key in the registry; no script means that.
        if rest.is_empty() {
            return None;
        }

        let (name, _, handle) = HIVES.iter().find(|(name, short, _)| {
            hive.eq_ignore_ascii_case(name) || hive.eq_ignore_ascii_case(short)
        })?;

        Some((name, RegKey::predef(*handle), rest.to_owned()))
    }

    pub(crate) fn exists(path: &str) -> bool {
        let Some((_, root, rest)) = split(path) else {
            return false;
        };

        root.open_subkey_with_flags(&rest, KEY_READ).is_ok()
    }

    pub(crate) fn read(path: &str, name: &str) -> Option<String> {
        let (_, root, rest) = split(path)?;
        let key = root.open_subkey_with_flags(&rest, KEY_READ).ok()?;

        // Strings first, then the integer types scripts are most likely to
        // meet (`DWORD`), so a value can be inspected without a type switch.
        if let Ok(value) = key.get_value::<String, _>(name) {
            return Some(value);
        }

        if let Ok(value) = key.get_value::<u32, _>(name) {
            return Some(value.to_string());
        }

        if let Ok(value) = key.get_value::<u64, _>(name) {
            return Some(value.to_string());
        }

        None
    }

    pub(crate) fn write(path: &str, name: &str, data: &str) -> bool {
        let Some((_, root, rest)) = split(path) else {
            return false;
        };

        match root.create_subkey(&rest) {
            Ok((key, _)) => key.set_value(name, &data).is_ok(),
            Err(_) => false,
        }
    }

    pub(crate) fn values(path: &str) -> Vec<String> {
        let Some((_, root, rest)) = split(path) else {
            return Vec::new();
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_READ) else {
            return Vec::new();
        };

        key.enum_values().flatten().map(|(name, _)| name).collect()
    }

    pub(crate) fn keys(path: &str) -> Vec<String> {
        let Some((_, root, rest)) = split(path) else {
            return Vec::new();
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_READ) else {
            return Vec::new();
        };

        key.enum_keys().flatten().collect()
    }

    pub(crate) fn delete_value(path: &str, name: &str) -> bool {
        let Some((hive, root, rest)) = split(path) else {
            return false;
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_SET_VALUE) else {
            return false;
        };

        if key.delete_value(name).is_ok() {
            note(format!("{hive}\\{rest}\\{name}"));
            return true;
        }

        false
    }

    pub(crate) fn delete_key(path: &str) -> bool {
        let Some((hive, root, rest)) = split(path) else {
            return false;
        };

        // `delete_subkey_all` is called on the parent, so the key itself —
        // including everything under it — is what goes away.
        let (parent, name) = match rest.rsplit_once('\\') {
            Some((parent, name)) => (parent.to_owned(), name.to_owned()),
            None => (String::new(), rest.clone()),
        };

        let Ok(parent_key) = root.open_subkey_with_flags(&parent, KEY_WRITE) else {
            return false;
        };

        if parent_key.delete_subkey_all(&name).is_ok() {
            note(format!("{hive}\\{rest}"));
            return true;
        }

        false
    }
}

#[cfg(not(windows))]
mod imp {
    // The registry does not exist off Windows, so the calls compile everywhere
    // but never touch anything. A script that mentions the registry stays
    // portable instead of failing to load on Linux and macOS.

    pub(crate) fn exists(_path: &str) -> bool {
        false
    }

    pub(crate) fn read(_path: &str, _name: &str) -> Option<String> {
        None
    }

    pub(crate) fn write(_path: &str, _name: &str, _data: &str) -> bool {
        false
    }

    pub(crate) fn values(_path: &str) -> Vec<String> {
        Vec::new()
    }

    pub(crate) fn keys(_path: &str) -> Vec<String> {
        Vec::new()
    }

    pub(crate) fn delete_value(_path: &str, _name: &str) -> bool {
        false
    }

    pub(crate) fn delete_key(_path: &str) -> bool {
        false
    }
}

pub(crate) use imp::{delete_key, delete_value, exists, keys, read, values, write};
