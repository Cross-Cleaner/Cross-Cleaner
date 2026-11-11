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
    module.function_meta(keys)?;
    module.function_meta(subkeys)?;
    module.function_meta(delete_value)?;
    module.function_meta(delete_key)?;

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
