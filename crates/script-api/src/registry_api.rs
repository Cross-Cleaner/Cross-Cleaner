//! `cleaner::registry` — reading, inspecting and removing registry entries.
//!
//! The declarative `cleaner::registry_cleaner(...)` covers the common case.
//! These functions are for a script that has to look before it deletes: check
//! whether a value is still there, walk the subkeys, remove only the ones that
//! match. They do nothing off Windows, so a script that mentions them still
//! loads on Linux and macOS.

use rune::{ContextError, Module};

use crate::registry;

/// The `cleaner::registry` module.
#[rune::module(::cleaner::registry)]
pub fn module() -> Result<Module, ContextError> {
    let mut module = Module::from_meta(self::module_meta)?;

    module.function_meta(exists)?;
    module.function_meta(read)?;
    module.function_meta(read_dword)?;
    module.function_meta(read_qword)?;
    module.function_meta(write)?;
    module.function_meta(values)?;
    module.function_meta(values_full)?;
    module.function_meta(export)?;
    module.function_meta(backup)?;
    module.function_meta(write_dword)?;
    module.function_meta(write_binary)?;
    module.function_meta(keys)?;
    module.function_meta(subkeys)?;
    module.function_meta(delete_value)?;
    module.function_meta(delete_key)?;

    module.ty::<Value>()?;

    Ok(module)
}

/// Whether the key exists.
#[rune::function]
fn exists(path: &str) -> bool {
    registry::exists(path)
}

/// Reads the value `name` of the key as text. `None` if it is missing or has no
/// text form.
#[rune::function]
fn read(path: &str, name: &str) -> Option<String> {
    registry::read(path, name)
}

/// Reads the `REG_DWORD` value `name` as the number it is.
///
/// `read` would hand back the same number as text; this one is for comparing it
/// with another number instead of parsing a string back.
#[rune::function]
fn read_dword(path: &str, name: &str) -> Option<u32> {
    registry::read_dword(path, name)
}

/// Reads the `REG_QWORD` value `name` as the number it is.
#[rune::function]
fn read_qword(path: &str, name: &str) -> Option<u64> {
    registry::read_qword(path, name)
}

/// Sets the value `name` of the key to `data`, creating the key if needed.
#[rune::function]
fn write(path: &str, name: &str, data: &str) -> bool {
    registry::write(path, name, data)
}

/// The names of the values directly under the key.
#[rune::function]
fn values(path: &str) -> Vec<String> {
    registry::values(path)
}

/// The names of the sub-keys, relative to the key.
///
/// ```rune
/// // Rune has no raw strings: backslashes are doubled, as in Rust.
/// let base = "HKEY_CURRENT_USER\\Software\\My App\\Recent";
///
/// for sub in cleaner::registry::keys(base) {
///     if sub.starts_with("session-") {
///         cleaner::registry::delete_key(format!("{}\\{}", base, sub));
///     }
/// }
/// ```
#[rune::function]
fn keys(path: &str) -> Vec<String> {
    registry::keys(path)
}

/// Every sub-key under the key, `depth` levels deep, as paths relative to it —
/// [`keys`] is one level and stops there.
///
/// Parents come before their children, so walking the list front to back and
/// deleting is safe.
///
/// ```rune
/// let base = "HKEY_CURRENT_USER\\\\Software\\\\My App";
///
/// for sub in cleaner::registry::subkeys(base, 3) {
///     cleaner::registry::delete_key(format!("{}\\\\{}", base, sub));
/// }
/// ```
#[rune::function]
fn subkeys(path: &str, depth: u32) -> Vec<String> {
    registry::subkeys(path, depth)
}

/// Removes the value `name` of the key. `true` if it was removed.
#[rune::function]
fn delete_value(path: &str, name: &str) -> bool {
    registry::delete_value(path, name)
}

/// Removes the key with everything under it. `true` if it was removed.
#[rune::function]
fn delete_key(path: &str) -> bool {
    registry::delete_key(path)
}

/// One value of a key, with the type the registry stores it as.
///
/// The type is the part that matters for a delete decision: a `REG_SZ` is
/// usually a setting someone typed, a `REG_BINARY` may be a license blob, and
/// neither is the cache a script set out to clear.
#[derive(Debug, Clone, rune::Any)]
#[rune(item = ::cleaner::registry)]
pub struct Value {
    /// The value's name; the default value has an empty one.
    #[rune(get)]
    pub name: String,
    /// The registry type, e.g. `"REG_DWORD"` or `"REG_SZ"`.
    #[rune(get)]
    pub kind: String,
    /// The value as text: the number for `REG_DWORD`/`REG_QWORD`, the string
    /// itself for `REG_SZ`, `<N bytes>` for binary data.
    #[rune(get)]
    pub text: String,
}

/// The values with their type.
///
/// [`values`] answers *which* values exist; this answers what they are, which
/// is what a script needs before deciding that a key is disposable.
///
/// ```rune
/// for value in cleaner::registry::values_full(base) {
///     cleaner::log(`${value.name}: ${value.kind} = ${value.text}`);
/// }
/// ```
#[rune::function]
fn values_full(path: &str) -> Vec<Value> {
    registry::values_full(path)
        .into_iter()
        .map(|(name, kind, text)| Value { name, kind, text })
        .collect()
}

/// The key and everything under it as the text of a `.reg` file.
///
/// Empty when the key does not exist. The text is what `regedit` imports, so a
/// script can save a key before deleting it and tell the user where the file is.
///
/// ```rune
/// let base = "HKEY_CURRENT_USER\\Software\\My App";
/// let backup = cleaner::join([cleaner::appdata(), "My App", "backup.reg"]);
///
/// if cleaner::write(backup, cleaner::registry::export(base)) {
///     cleaner::registry::delete_key(base);
/// }
/// ```
#[rune::function]
fn export(path: &str) -> String {
    registry::export(path)
}

/// Writes `export(path)` to `file`. `true` on success.
///
/// The file is written with [`cleaner::write`](super::write), so a script that
/// wants the text as well can call `export` itself.
#[rune::function]
fn backup(path: &str, file: &str) -> bool {
    let text = registry::export(path);

    if text.is_empty() {
        return false;
    }

    crate::files::write(file, &text)
}

/// Sets the `REG_DWORD` value `name` to `value`, creating the key if needed.
#[rune::function]
fn write_dword(path: &str, name: &str, value: u32) -> bool {
    registry::write_dword(path, name, value)
}

/// Sets the `REG_BINARY` value `name` to `bytes`, creating the key if needed.
#[rune::function]
fn write_binary(path: &str, name: &str, bytes: Vec<u8>) -> bool {
    registry::write_binary(path, name, &bytes)
}
