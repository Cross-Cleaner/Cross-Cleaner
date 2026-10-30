//! User scripts: where they live, and how their declarations reach cleaning.
//!
//! Everything a script *declares* goes through the wider `script-api` crate;
//! this module is the thin seam the frontends use, so the CLI, the TUI and both
//! window builds wire scripts up identically instead of each keeping its own
//! idea of the order the calls go in.
//!
//! The order matters, and it is always:
//!
//! 1. [`register_custom_cleaners`] — before the global custom-cleaner registry
//!    is read, or the scripts' cleaners are not in it;
//! 2. [`with_rules`] (and [`with_registry_rules`]) — before [`AppState`] scans
//!    the databases, because the scan is what builds the category list.
//!
//! [`AppState`]: crate::AppState

use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;

pub use script_api::{ScriptError, Scripts, notify_finished, scripts_dir, wait_for_hooks};

/// Layers the rules the user's scripts declared over `database`.
///
/// The rules are joined to the built-in catalog: they appear under the same
/// categories, count like any other entry and are cleaned by the same job. A
/// user with no scripts gets the database back unchanged.
pub fn with_rules(database: CleanerDatabase) -> CleanerDatabase {
    script_api::with_scripts(database)
}

/// Layers the registry rules the user's scripts declared over `database`.
///
/// Windows-only because the registry database is; on other platforms scripts can
/// still declare registry rules and they simply have nothing to join.
#[cfg(windows)]
pub fn with_registry_rules(database: RegistryDatabase) -> RegistryDatabase {
    script_api::with_registry_scripts(database)
}

/// Registers the scripts' custom cleaners next to the built-in ones.
///
/// Call it after `cleaner::custom_cleaners::register_all()`: both write into the
/// same registry, and `database::custom_cleaners::get_custom_cleaners()` reads
/// it once.
pub fn register_custom_cleaners() {
    script_api::register_custom_cleaners();
}
