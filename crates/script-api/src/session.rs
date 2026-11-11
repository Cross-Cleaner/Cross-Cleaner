//! What a script can ask about the run it is part of.
//!
//! A handler is written once and then called from everywhere: the window app, a
//! scheduled `clean`, a scan. Whether the user is still waiting, whether the run
//! is a preview, and which script is asking are all properties of the call, not
//! of the script, so they live here rather than in a script variable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The API level this build implements.
///
/// Bumped whenever a script-visible function or field is added, so a script can
/// say `cleaner::require_api(2)?` instead of failing on an unknown name — the
/// error a missing function gives is a compile error inside the script, which
/// reads like the script is broken rather than the build being old.
pub const API_LEVEL: u64 = 3;

/// Set while a cleaning run is being cancelled.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Set when scripts may run programs of their own.
static PROCESS_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Set while the current run only measures, never removes.
static DRY_RUN: AtomicBool = AtomicBool::new(false);

/// Where `cleaner::progress` sends its text, installed by the frontend.
static PROGRESS: Mutex<Option<ProgressSink>> = Mutex::new(None);

/// Where `cleaner::notify` shows a notification, installed by the frontend.
static NOTIFY: Mutex<Option<NotifySink>> = Mutex::new(None);

type ProgressSink = Box<dyn Fn(&str) + Send + Sync>;
type NotifySink = Box<dyn Fn(&str, &str) + Send + Sync>;

fn progress_sink() -> MutexGuard<'static, Option<ProgressSink>> {
    match PROGRESS.lock() {
        Ok(sink) => sink,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn notify_sink() -> MutexGuard<'static, Option<NotifySink>> {
    match NOTIFY.lock() {
        Ok(sink) => sink,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Forgets the state of a previous run. Called when a run is about to start.
pub fn begin(dry_run: bool) {
    CANCELLED.store(false, Ordering::Relaxed);
    DRY_RUN.store(dry_run, Ordering::Relaxed);
}

/// Asks the run to stop at its next checkpoint.
///
/// A frontend calls this when the user asks to stop: the cleaning job stops
/// starting new cleaners, and a handler that polls [`cancelled`] can stop its own
/// walk early instead of finishing work the user no longer wants. Cancelling is
/// cooperative — nothing is torn down mid-delete.
pub fn cancel() {
    CANCELLED.store(true, Ordering::Relaxed);
}

/// Whether the run has been asked to stop.
pub fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

/// Whether the current run only measures.
pub fn dry_run() -> bool {
    DRY_RUN.load(Ordering::Relaxed)
}

/// Allows or forbids `process` for scripts in this process.
///
/// Decided once at startup, before any script is compiled: a script that was
/// written to shell out fails to load instead of silently doing nothing, which
/// is the error the user can act on.
pub fn allow_process(allow: bool) {
    PROCESS_ALLOWED.store(allow, Ordering::Relaxed);
}

/// Whether scripts may use the `process` module.
pub fn process_allowed() -> bool {
    PROCESS_ALLOWED.load(Ordering::Relaxed)
}

/// Installs where [`progress`] goes. `None` removes it.
pub fn set_progress_sink(sink: Option<ProgressSink>) {
    *progress_sink() = sink;
}

/// Installs where [`notify`] goes. `None` removes it.
pub fn set_notify_sink(sink: Option<NotifySink>) {
    *notify_sink() = sink;
}

/// Reports what the script is doing, for a frontend that has somewhere to show
/// it. Without a sink the text goes to the log, so a script is never wrong for
/// having asked.
pub fn progress(text: &str) {
    let sink = progress_sink();
    match sink.as_ref() {
        Some(sink) => sink(text),
        None => database::diag::info(format!("script progress: {text}")),
    }
}

/// Shows a desktop notification with `title` and `body`.
///
/// This is the notification the app itself uses, not a command line: it works on
/// every platform the app runs on without a script knowing which one it is.
/// Without a sink (a headless build) the text goes to the log.
pub fn notify(title: &str, body: &str) {
    let sink = notify_sink();
    match sink.as_ref() {
        Some(sink) => sink(title, body),
        None => database::diag::info(format!("script notification: {title}: {body}")),
    }
}

thread_local! {
    /// The script whose handler is running, for `script_path()`.
    static RUNNING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Names the script a handler about to run belongs to.
pub(crate) fn set_running_script(path: &str) {
    RUNNING.with(|slot| *slot.borrow_mut() = Some(path.to_owned()));
}

/// The script file whose handler is running, if one is.
pub fn script_path() -> Option<String> {
    RUNNING.with(|slot| slot.borrow().clone())
}

/// The Cross Cleaner version that is running the script, e.g. `"2.0.4.2.1"`.
pub fn version() -> String {
    database::get_version().to_owned()
}

/// Fails when this build's script API is older than `level`.
///
/// ```rune
/// pub fn main() {
///     cleaner::require_api(2)?;
///     // ...
/// }
/// ```
pub fn require_api(level: u64) -> Result<(), String> {
    if level > API_LEVEL {
        return Err(format!(
            "this script needs script API {level}, but Cross Cleaner {} provides {API_LEVEL}",
            version()
        ));
    }

    Ok(())
}
