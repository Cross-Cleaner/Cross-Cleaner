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

    /// The `REG_DWORD` value under `name`, as the number it is.
    ///
    /// `None` when the key, the value or the type is not what was asked for —
    /// `read` instead turns a `DWORD` into text and leaves the script to parse
    /// it back.
    pub(crate) fn read_dword(path: &str, name: &str) -> Option<u32> {
        let (_, root, rest) = split(path)?;
        let key = root.open_subkey_with_flags(&rest, KEY_READ).ok()?;
        key.get_value::<u32, _>(name).ok()
    }

    /// The `REG_QWORD` value under `name`, as the number it is.
    pub(crate) fn read_qword(path: &str, name: &str) -> Option<u64> {
        let (_, root, rest) = split(path)?;
        let key = root.open_subkey_with_flags(&rest, KEY_READ).ok()?;
        key.get_value::<u64, _>(name).ok()
    }

    /// Every subkey under `path`, down to `depth` levels, as paths relative to
    /// it — `keys` is one level and stops there.
    ///
    /// Deeper keys come after their parents, so deleting them front to back
    /// never leaves a parent a later path still needed.
    pub(crate) fn subkeys(path: &str, depth: u32) -> Vec<String> {
        let Some((_, root, rest)) = split(path) else {
            return Vec::new();
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_READ) else {
            return Vec::new();
        };

        let mut found = Vec::new();
        walk(&key, String::new(), depth, &mut found);
        found
    }

    fn walk(key: &RegKey, prefix: String, depth: u32, found: &mut Vec<String>) {
        let Ok(names) = key.enum_keys().collect::<Result<Vec<String>, _>>() else {
            return;
        };

        for name in names {
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}\\{name}")
            };

            found.push(full.clone());

            if depth > 1
                && let Ok(child) = key.open_subkey_with_flags(&name, KEY_READ)
            {
                walk(&child, full, depth - 1, found);
            }
        }
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

    /// The name a value's type is written with: `REG_DWORD` and so on.
    fn type_name(vtype: winreg::enums::RegType) -> &'static str {
        use winreg::enums::RegType::{
            REG_BINARY, REG_DWORD, REG_DWORD_BIG_ENDIAN, REG_EXPAND_SZ, REG_LINK, REG_MULTI_SZ,
            REG_NONE, REG_QWORD, REG_RESOURCE_LIST, REG_SZ,
        };

        match vtype {
            REG_NONE => "REG_NONE",
            REG_SZ => "REG_SZ",
            REG_EXPAND_SZ => "REG_EXPAND_SZ",
            REG_BINARY => "REG_BINARY",
            REG_DWORD => "REG_DWORD",
            REG_DWORD_BIG_ENDIAN => "REG_DWORD_BIG_ENDIAN",
            REG_LINK => "REG_LINK",
            REG_MULTI_SZ => "REG_MULTI_SZ",
            REG_RESOURCE_LIST => "REG_RESOURCE_LIST",
            REG_QWORD => "REG_QWORD",
            _ => "REG_UNKNOWN",
        }
    }

    /// One value per row: name, type and text.
    pub(crate) fn values_full(path: &str) -> Vec<(String, String, String)> {
        let Some((_, root, rest)) = split(path) else {
            return Vec::new();
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_READ) else {
            return Vec::new();
        };

        key.enum_values()
            .flatten()
            .map(|(name, value)| {
                let kind = type_name(value.vtype.clone());
                let shown = match value.vtype {
                    winreg::enums::RegType::REG_DWORD if value.bytes.len() == 4 => {
                        let bytes: [u8; 4] = value.bytes[..4].try_into().unwrap_or_default();
                        u32::from_le_bytes(bytes).to_string()
                    }
                    winreg::enums::RegType::REG_QWORD if value.bytes.len() == 8 => {
                        let bytes: [u8; 8] = value.bytes[..8].try_into().unwrap_or_default();
                        u64::from_le_bytes(bytes).to_string()
                    }
                    _ => key
                        .get_value::<String, _>(&name)
                        .unwrap_or_else(|_| format!("<{} bytes>", value.bytes.len())),
                };

                (name, kind.to_owned(), shown)
            })
            .collect()
    }

    /// The key and everything under it as a `.reg` file's text.
    ///
    /// Writes the format `regedit` imports, so a script can save a key before
    /// deleting it and the user can restore it by double-clicking the file.
    /// Values keep their type; binary data is written as `hex:`.
    pub(crate) fn export(path: &str) -> String {
        let Some((_, root, rest)) = split(path) else {
            return String::new();
        };

        let Ok(key) = root.open_subkey_with_flags(&rest, KEY_READ) else {
            return String::new();
        };

        let mut text = String::from("Windows Registry Editor Version 5.00\r\n");
        write_key(&mut text, path.trim(), &key);

        text
    }

    fn write_key(text: &mut String, path: &str, key: &RegKey) {
        text.push_str(&format!("\r\n[{path}]\r\n"));

        for (name, value) in key.enum_values().flatten() {
            let shown = match value.vtype {
                winreg::enums::RegType::REG_SZ | winreg::enums::RegType::REG_EXPAND_SZ => {
                    match key.get_value::<String, _>(&name) {
                        Ok(string) => format!("\"{}\"", escape(&string)),
                        Err(_) => continue,
                    }
                }
                winreg::enums::RegType::REG_DWORD if value.bytes.len() == 4 => {
                    let bytes: [u8; 4] = value.bytes[..4].try_into().unwrap_or_default();
                    format!("dword:{:08x}", u32::from_le_bytes(bytes))
                }
                winreg::enums::RegType::REG_QWORD if value.bytes.len() == 8 => {
                    let bytes: [u8; 8] = value.bytes[..8].try_into().unwrap_or_default();
                    let digits: Vec<String> = u64::from_le_bytes(bytes)
                        .to_le_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    format!("hex(b):{}", digits.join(","))
                }
                _ => {
                    let digits: Vec<String> = value
                        .bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    format!("hex:{}", digits.join(","))
                }
            };

            text.push_str(&format!("\"{}\"={shown}\r\n", escape(&name)));
        }

        if let Ok(names) = key.enum_keys().collect::<Result<Vec<String>, _>>() {
            for name in names {
                if let Ok(child) = key.open_subkey_with_flags(&name, KEY_READ) {
                    write_key(text, &format!("{path}\\{name}"), &child);
                }
            }
        }
    }

    /// Escapes a name or a string the way a `.reg` file expects.
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    pub(crate) fn write_dword(path: &str, name: &str, value: u32) -> bool {
        let Some((_, root, rest)) = split(path) else {
            return false;
        };

        match root.create_subkey(&rest) {
            Ok((key, _)) => key.set_value(name, &value).is_ok(),
            Err(_) => false,
        }
    }

    pub(crate) fn write_binary(path: &str, name: &str, value: &[u8]) -> bool {
        let Some((_, root, rest)) = split(path) else {
            return false;
        };

        match root.create_subkey(&rest) {
            Ok((key, _)) => key
                .set_raw_value(
                    name,
                    &winreg::RegValue {
                        bytes: value.to_vec().into(),
                        vtype: winreg::enums::RegType::REG_BINARY,
                    },
                )
                .is_ok(),
            Err(_) => false,
        }
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

    /// The values with their type, as `(name, type, text)`.
    pub(crate) fn values_full(_path: &str) -> Vec<(String, String, String)> {
        Vec::new()
    }

    pub(crate) fn export(_path: &str) -> String {
        String::new()
    }

    pub(crate) fn write_dword(_path: &str, _name: &str, _value: u32) -> bool {
        false
    }

    pub(crate) fn write_binary(_path: &str, _name: &str, _value: &[u8]) -> bool {
        false
    }

    pub(crate) fn exists(_path: &str) -> bool {
        false
    }

    pub(crate) fn read(_path: &str, _name: &str) -> Option<String> {
        None
    }

    pub(crate) fn read_dword(_path: &str, _name: &str) -> Option<u32> {
        None
    }

    pub(crate) fn read_qword(_path: &str, _name: &str) -> Option<u64> {
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

    pub(crate) fn subkeys(_path: &str, _depth: u32) -> Vec<String> {
        Vec::new()
    }

    pub(crate) fn delete_value(_path: &str, _name: &str) -> bool {
        false
    }

    pub(crate) fn delete_key(_path: &str) -> bool {
        false
    }
}

pub(crate) use imp::{
    delete_key, delete_value, exists, export, keys, read, read_dword, read_qword, subkeys, values,
    values_full, write, write_binary, write_dword,
};
