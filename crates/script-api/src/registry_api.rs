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
    module.function_meta(write)?;
    module.function_meta(values)?;
    module.function_meta(keys)?;
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
