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

pub use script_api::{
    RunTotals, ScriptError, ScriptSummary, Scripts, load, load_from, notify_failed,
    notify_finished, notify_started, scripts_dir, wait_for_hooks,
};

/// Prepares the script runtime for this process.
///
/// Call it once, before any script is loaded. It decides what a script is
/// allowed to do — `process` is opt-in, because a handler that shells out can
/// delete anything without going through the cleaning engine — and points the
/// parts of the API that need a frontend (`progress()` and `notify()`) at the
/// sinks this build has.
pub fn init() {
    script_api::session::allow_process(process_allowed());
    script_api::session::set_progress_sink(Some(Box::new(|text: &str| {
        database::diag::info(format!("script: {text}"));
    })));
    script_api::session::set_notify_sink(Some(Box::new(|title: &str, body: &str| {
        crate::cleaning::show_notification(title, body);
    })));
}

/// Whether a script may run programs of its own.
///
/// Off unless the user turned it on in the config, or set
/// `CROSS_CLEANER_SCRIPTS_PROCESS=1`. A handler that calls `process::Command`
/// deletes outside everything the engine counts and every rule the user can
/// see, so it has to be a deliberate choice rather than a default.
fn process_allowed() -> bool {
    if std::env::var_os("CROSS_CLEANER_SCRIPTS_PROCESS").is_some_and(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "off" | "no")
    }) {
        return true;
    }

    crate::config::get().scripts_allow_process
}

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

/// Asks the running cleaning job, and the scripts taking part in it, to stop.
///
/// Cooperative: the job stops starting new cleaners and a handler that checks
/// `cleaner::cancelled()` returns early. A frontend that can abort the task
/// outright still should — this is what lets a script be a good citizen, not a
/// replacement for stopping the job.
pub fn cancel() {
    script_api::session::cancel();
}

/// Tells the scripts that the run they were taking part in was stopped.
///
/// A frontend that aborted the job calls this after the abort: the run itself
/// never reached its end, so `on_cancelled` is the only hook that can still run.
pub fn notify_cancelled() {
    script_api::notify_cancelled();
}

/// Tells the scripts that a scan is starting.
pub fn notify_scan() {
    script_api::notify_scan();
}

/// Tells the scripts that one program was cleaned.
pub fn notify_program_done(cleared: &database::structures::Cleared) {
    script_api::notify_program_done(cleared);
}
