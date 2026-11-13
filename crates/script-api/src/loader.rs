//! Finding, compiling and running the user's scripts.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::{CleanerResult, Cleared, ClearedPath, CustomCleaner, SharedPath};
use rune::runtime::RuntimeContext;
use rune::{Diagnostics, Source, Sources, Unit, Vm};
use tokio::sync::mpsc::Sender;

use crate::api::{self, RegistryRule, Report};
use crate::collect;
use crate::collect::HookKind;
use crate::registry;
use crate::remove::{self, Stats};

/// How many removed paths a script cleaner lists before it starts counting the
/// rest as omitted. The results page never shows more than this many rows for a
/// single cleaner anyway, and a script pointed at a large tree must not build a
/// list proportional to it.
const MAX_LISTED_PATHS: usize = 1024;

/// One thing that stopped a script from contributing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    /// The script the problem came from.
    pub path: PathBuf,
    /// What went wrong, already readable.
    pub message: String,
}

impl ScriptError {
    pub(crate) fn new(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.as_os_str().is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path.display(), self.message)
        }
    }
}

impl std::error::Error for ScriptError {}

/// Everything the scripts contributed, plus why some of them could not.
///
/// Not `Debug`: the rows are the database's own types, and a script's cleaner is
/// a closure - `reports` is the printable view of the same load.
#[derive(Clone)]
pub struct Scripts {
    /// Declarative file and folder rules, to be layered over the cleaner
    /// database.
    pub files: Vec<database::structures::CleanerData>,
    /// Declarative registry rules (applied on Windows only).
    pub registry: Vec<RegistryRule>,
    /// Script-driven cleaners, ready for [`database::custom_cleaners`].
    pub custom: Vec<CustomCleaner>,
    /// Scripts that failed, in the order they were read. A script that failed
    /// contributes nothing: its registrations are rolled back.
    pub errors: Vec<ScriptError>,
    /// One entry per script file, whether it loaded or not.
    pub reports: Vec<ScriptSummary>,
    /// The directory these scripts came from.
    pub directory: PathBuf,
}

/// What one script declared, for `cli scripts` and any other listing.
///
/// A script that failed shows up with the error that stopped it and zero rules:
/// "nothing was declared" and "the file is not there" look identical otherwise,
/// and the second is the one a user has to fix.
#[derive(Debug, Clone)]
pub struct ScriptSummary {
    /// The file the declarations came from.
    pub path: PathBuf,
    /// File and folder rules.
    pub files: usize,
    /// Registry rules.
    pub registry: usize,
    /// Custom cleaners with a handler.
    pub custom: usize,
    /// Hook handlers: `on_started`, `on_scan`, `on_program_done`, `on_finished`,
    /// `on_cancelled` and `on_error`.
    pub hooks: usize,
    /// Rules the script asked for but that could not be used, such as a
    /// cleaner without a path.
    pub complaints: usize,
    /// Why the script did not load. `None` when it did.
    pub error: Option<String>,
    /// What the script said about itself with `cleaner::about`, if anything.
    pub about: Option<collect::About>,
}

impl Default for Scripts {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            registry: Vec::new(),
            custom: Vec::new(),
            errors: Vec::new(),
            reports: Vec::new(),
            directory: PathBuf::new(),
        }
    }
}

/// What one handler call removed.
struct Removed {
    stats: Stats,
    registry: Vec<String>,
    failures: Vec<remove::Failure>,
}

/// A compiled script plus the handlers one of its registrations calls.
struct ScriptProgram {
    context: Arc<RuntimeContext>,
    unit: Arc<Unit>,
    handler: String,
    glob: bool,
    /// Fence for the handler's removals; see `CustomCleanerBuilder::within`.
    within: Option<String>,
    /// The file this handler came from, for `cleaner::script_path()`.
    script: String,
}

impl ScriptProgram {
    /// Runs the handler for every target of one cleaner and folds what each
    /// path removed into `result`.
    ///
    /// Blocking, so the caller runs it on a blocking thread. One thread and one
    /// reactor serve the whole cleaner: a glob matched against thousands of
    /// files would otherwise pay for a thread and a runtime per file — far more
    /// than the handler itself costs — and the removal counters are thread
    /// locals, so every path of one cleaner has to run on that one thread.
    fn clean_all(
        &self,
        targets: Vec<PathBuf>,
        mut result: CleanerResult,
        sender: Option<Sender<String>>,
        id: String,
    ) -> CleanerResult {
        let handler = self.handler.as_str();
        let script = self.script.as_str();
        let context = self.context.clone();
        let unit = self.unit.clone();
        let total = targets.len();

        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                database::diag::warn(format!("{id}: cannot start the script runtime: {error}"));
                return result;
            }
        };

        // One root for every path this cleaner hands the handler, so it is
        // resolved once rather than canonicalised per path.
        remove::set_within_resolved(remove::resolve_within(self.within.as_deref()));

        // A panic that somehow escapes a path's own guard must not unwind out of
        // the blocking pool and take the counts with it.
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            for (index, target) in targets.into_iter().enumerate() {
                let path = target.to_string_lossy().into_owned();

                // One path that panics is a failure of that path, not of the
                // cleaner: the rest of the tree is still worth cleaning.
                let removed = panic::catch_unwind(AssertUnwindSafe(|| {
                    runtime.block_on(async {
                        remove::reset();
                        registry::reset();
                        crate::session::set_running_script(script);

                        let vm = Vm::new(context.clone(), unit.clone());
                        let execution = vm
                            .send_execute([handler], (path,))
                            .map_err(|error| format!("{handler}(): {error}"))?;

                        // The handler's return value is deliberately ignored:
                        // what it removed is what the removal calls — or
                        // `cleaner::track` — really counted, which a script
                        // cannot overstate by returning a number.
                        execution
                            .async_complete()
                            .await
                            .into_result()
                            .map_err(|error| format!("{handler}(): {error}"))?;

                        Ok(Removed {
                            stats: remove::take(),
                            registry: registry::take(),
                            failures: remove::take_failures(),
                        })
                    })
                }))
                .unwrap_or_else(|_| Err(format!("the {handler}() handler panicked")));

                match removed {
                    Ok(removed) => {
                        record(&mut result, &target, removed.stats);
                        record_registry(&mut result, removed.registry);
                        record_failures(&mut result, removed.failures);
                    }
                    Err(message) => database::diag::warn(format!("{id}: {message}")),
                }

                if index % 5 == 0
                    && let Some(sender) = &sender
                {
                    let _ = sender.try_send(format!(
                        "Cleaning {}/{} paths... {}",
                        index + 1,
                        total,
                        database::utils::get_file_size_string(result.bytes)
                    ));
                }
            }
        }));

        crate::session::clear_running_script();

        result
    }

    /// Runs an `on_finished` handler with the report of a finished run.
    fn finished(&self, report: &Report) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let report = report.clone();
        let script = self.script.clone();

        drive(async move {
            crate::session::set_running_script(&script);
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], (report,))
                .map_err(|error| format!("{handler}(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(())
        })
    }

    /// Runs an `on_started` handler, with no arguments.
    ///
    /// Called before the deletions begin, and synchronously: a hook that takes
    /// five seconds is meant to take five seconds, not to race the cleaning.
    fn started(&self) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let script = self.script.clone();

        drive(async move {
            crate::session::set_running_script(&script);
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], ())
                .map_err(|error| format!("{handler}(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(())
        })
    }

    /// Runs an `on_scan` handler, with no arguments.
    ///
    /// Called before a scan starts. A scan is a preview: it deletes nothing, so
    /// the handler is free to look around but must not prepare anything.
    fn scan(&self) -> Result<(), String> {
        self.no_args()
    }

    /// Runs an `on_cancelled` handler, with no arguments.
    ///
    /// Called once when the user stops a run, after the workers have stopped.
    fn cancelled(&self) -> Result<(), String> {
        self.no_args()
    }

    /// Runs an `on_program_done` handler with one program's report.
    fn program_done(&self, report: &api::ProgramReport) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let report = report.clone();
        let script = self.script.clone();

        drive(async move {
            crate::session::set_running_script(&script);
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], (report,))
                .map_err(|error| format!("{handler}(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(())
        })
    }

    /// Runs the handler on a Rune VM with no arguments.
    fn no_args(&self) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let script = self.script.clone();

        drive(async move {
            crate::session::set_running_script(&script);
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], ())
                .map_err(|error| format!("{handler}(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(())
        })
    }

    /// Runs an `on_error` handler with the report and why the run was not clean.
    fn failed(&self, report: &Report, problem: &str) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let report = report.clone();
        let problem = problem.to_owned();
        let script = self.script.clone();

        drive(async move {
            crate::session::set_running_script(&script);
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], (report, problem))
                .map_err(|error| format!("{handler}(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(())
        })
    }
}

/// Runs a script future to completion.
///
/// Scripts may `.await` — `std::fs::read_to_string`, `process::Command`,
/// `time::sleep` all do — and a Rune VM can only be driven by a real reactor.
/// The future therefore runs on a thread of its own with a current-thread
/// runtime, which also keeps a handler's counters on one thread: the removal
/// functions accumulate in a thread local, so a future that hopped threads
/// between two awaits would lose what it counted. Working on a private thread
/// means `block_on` is always legal, even when the caller is itself a tokio
/// task.
fn drive<T, F>(future: F) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(future),
            Err(error) => Err(format!("cannot start the script runtime: {error}")),
        };

        let _ = sender.send(outcome);
    });

    receiver
        .recv()
        .map_err(|_| String::from("the script thread died"))?
}

static SCRIPTS: OnceLock<Scripts> = OnceLock::new();
static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

/// Keys of the cleaners whose script touches the filesystem on its own.
///
/// A preview runs a script cleaner to find out what it would remove, and the
/// preview flag is only honoured by the `cleaner::remove_*` functions. A script
/// that calls the `fs` module directly would really delete during a preview,
/// so the engine refuses to preview those and names them instead.
fn own_file_code() -> &'static Mutex<std::collections::HashSet<String>> {
    static KEYS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    KEYS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Whether a custom cleaner registered by a script can be measured by running it
/// with the preview flag on.
///
/// `id` is the database id, `script:<the script's own id>:<key>`.
pub fn can_preview(id: &str) -> bool {
    let Some((_, key)) = id.rsplit_once(':') else {
        return false;
    };

    match own_file_code().lock() {
        Ok(keys) => !keys.contains(key),
        Err(_) => false,
    }
}

/// Whether the source mentions a module that deletes without the preview flag.
fn touches_files_directly(source: &str) -> bool {
    let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();

    compact.contains("fs::") || compact.contains("process::")
}

fn programs() -> &'static RwLock<HashMap<String, Arc<ScriptProgram>>> {
    static PROGRAMS: OnceLock<RwLock<HashMap<String, Arc<ScriptProgram>>>> = OnceLock::new();
    PROGRAMS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// A hook, with the phase it belongs to.
struct Hook {
    kind: HookKind,
    program: Arc<ScriptProgram>,
}

fn hooks() -> &'static RwLock<Vec<Hook>> {
    static HOOKS: OnceLock<RwLock<Vec<Hook>>> = OnceLock::new();
    HOOKS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Snapshot of the hooks of one phase, so a hook cannot deadlock the registry.
fn hooks_of(kind: HookKind) -> Vec<Arc<ScriptProgram>> {
    let list = match hooks().read() {
        Ok(list) => list,
        Err(poisoned) => poisoned.into_inner(),
    };

    list.iter()
        .filter(|hook| hook.kind == kind)
        .map(|hook| hook.program.clone())
        .collect()
}

fn program(key: &str) -> Option<Arc<ScriptProgram>> {
    match programs().read() {
        Ok(map) => map.get(key).cloned(),
        Err(poisoned) => poisoned.into_inner().get(key).cloned(),
    }
}

fn add_program(key: String, program: Arc<ScriptProgram>) {
    match programs().write() {
        Ok(mut map) => {
            map.insert(key, program);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(key, program);
        }
    }
}

fn add_hook(kind: HookKind, program: Arc<ScriptProgram>) {
    let hook = Hook { kind, program };

    match hooks().write() {
        Ok(mut list) => list.push(hook),
        Err(poisoned) => poisoned.into_inner().push(hook),
    }
}

/// The directory scripts are read from.
///
/// `CROSS_CLEANER_SCRIPTS_DIR` overrides it, which is what the tests use and
/// what lets a user keep several sets of scripts side by side.
pub fn scripts_dir() -> PathBuf {
    if let Some(directory) = std::env::var_os("CROSS_CLEANER_SCRIPTS_DIR") {
        return PathBuf::from(directory);
    }

    platform_data_dir().join("Cross-Cleaner").join("Scripts")
}

/// The platform's per-user data directory.
pub(crate) fn platform_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata);
        }

        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join("AppData").join("Roaming");
        }
    }

    #[cfg(not(windows))]
    {
        if cfg!(target_os = "macos")
            && let Some(home) = home_dir()
        {
            return home.join("Library").join("Application Support");
        }

        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME")
            && !config.is_empty()
        {
            return PathBuf::from(config);
        }

        if let Some(home) = home_dir() {
            return home.join(".config");
        }
    }

    PathBuf::from(".")
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Loads the scripts once and caches the result.
///
/// Every failed script is reported through [`database::diag`] the first time
/// this is called — once per process, not once per frontend query.
pub fn load() -> &'static Scripts {
    SCRIPTS.get_or_init(|| {
        let scripts = build(scripts_dir());

        for error in &scripts.errors {
            database::diag::warn(error.to_string());
        }

        scripts
    })
}

/// Loads scripts from an explicit directory, without the process-wide cache.
///
/// For tests and embedders that keep their scripts somewhere else than
/// [`scripts_dir`]. The custom cleaners in the result are runnable immediately.
pub fn load_from(directory: impl AsRef<Path>) -> Scripts {
    let scripts = build(directory.as_ref().to_path_buf());

    for error in &scripts.errors {
        database::diag::warn(error.to_string());
    }

    scripts
}

/// Registers the script-driven cleaners into
/// [`database::custom_cleaners`], so the frontends pick them up next to the
/// built-in ones.
///
/// Call it after `cleaner::custom_cleaners::register_all()` and before
/// `database::custom_cleaners::get_custom_cleaners()`.
pub fn register_custom_cleaners() {
    for cleaner in &load().custom {
        database::custom_cleaners::register_custom_cleaner(cleaner.clone());
    }
}

/// Records that a cleaning run is starting, and runs the `on_started` hooks.
///
/// Called by the frontends before the first deletion of a real run — never for a
/// scan or a dry run. Unlike [`notify_finished`] this is synchronous: a hook here
/// exists to do something *before* the files go (close a copy, take a backup,
/// check that the machine is idle), so finishing it quickly is exactly wrong.
///
/// The start time it records is what the finished report's `duration` and
/// `started_at` come from; a frontend that never calls this reports `0` for
/// both.
pub fn notify_started() {
    let hooks = hooks_of(HookKind::Started);

    let now = crate::files::now();
    let instant = std::time::Instant::now();

    let mut start = match STARTED.lock() {
        Ok(start) => start,
        Err(poisoned) => poisoned.into_inner(),
    };
    *start = Some((now, instant));
    drop(start);

    for hook in hooks {
        if let Err(error) = hook.started() {
            database::diag::warn(format!("started hook: {error}"));
        }
    }
}

/// Runs the `on_scan` hooks of every loaded script.
///
/// Called by the frontends before a preview, and synchronously: a scan handler
/// is the script's chance to say what it is about to measure, and it holds no
/// locks a longer run would make awkward.
pub fn notify_scan() {
    for hook in hooks_of(HookKind::Scan) {
        if let Err(error) = hook.scan() {
            database::diag::warn(format!("scan hook: {error}"));
        }
    }
}

/// Runs the `on_program_done` hooks for one cleaned program.
///
/// Called after a program's files were removed. A handler that fails is named
/// in the diagnostics and the run continues: a broken webhook must not stop a
/// cleanup half-way.
pub fn notify_program_done(cleared: &Cleared) {
    let hooks = hooks_of(HookKind::ProgramDone);

    if hooks.is_empty() {
        return;
    }

    let report = api::program_report(cleared);

    for hook in hooks {
        if let Err(error) = hook.program_done(&report) {
            database::diag::warn(format!("program done hook: {error}"));
        }
    }
}

/// Runs the `on_cancelled` hooks of every loaded script.
///
/// Called once after the user stopped a run, on a background thread for the same
/// reason as [`notify_finished`]: a handler is free to take its time and must not
/// hold the window that asked for the cancel.
pub fn notify_cancelled() {
    let hooks = hooks_of(HookKind::Cancelled);

    if hooks.is_empty() {
        return;
    }

    let handle = std::thread::spawn(move || {
        for hook in hooks {
            if let Err(error) = hook.cancelled() {
                database::diag::warn(format!("cancelled hook: {error}"));
            }
        }
    });

    let mut running = match HOOK_THREADS.lock() {
        Ok(running) => running,
        Err(poisoned) => poisoned.into_inner(),
    };
    running.push(handle);
}

/// When the current run started: the wall clock and when it began.
static STARTED: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

/// The run's start, or `(0, 0)` when no frontend recorded one.
fn started() -> (u64, u64) {
    let start = match STARTED.lock() {
        Ok(start) => *start,
        Err(poisoned) => *poisoned.into_inner(),
    };

    match start {
        Some((clock, instant)) => (clock, instant.elapsed().as_secs()),
        None => (0, 0),
    }
}

/// Runs the `on_finished` hooks of every loaded script.
///
/// Called by the frontends once a real cleaning run has completed — never for a
/// scan, and never while scripts are disabled, because hooks only exist if the
/// scripts that declared them were loaded. The handlers run on a background
/// thread: a webhook that takes five seconds must not freeze the results page
/// or hold the process open.
///
/// A frontend that is about to exit calls [`wait_for_hooks`] first, so a short
/// webhook still gets out instead of dying with the process.
pub fn notify_finished(totals: api::RunTotals, cleared: &[Cleared]) {
    let hooks = hooks_of(HookKind::Finished);

    if hooks.is_empty() {
        return;
    }

    let (started_at, duration) = started();
    let report = api::report(totals, cleared, started_at, duration);

    let handle = std::thread::spawn(move || {
        for hook in hooks {
            if let Err(error) = hook.finished(&report) {
                database::diag::warn(format!("finished hook: {error}"));
            }
        }
    });

    // Kept so `wait_for_hooks` can join it; a long-running one is simply left
    // to be detached when the caller stops waiting.
    let mut running = match HOOK_THREADS.lock() {
        Ok(running) => running,
        Err(poisoned) => poisoned.into_inner(),
    };
    running.push(handle);
}

/// Runs the `on_error` hooks of every loaded script.
///
/// Called instead of [`notify_finished`] when a run finished but left something
/// behind — a locked file, a denied directory. `problem` is the one-line
/// explanation the handler receives next to the report, so a monitoring channel
/// can say what went wrong without parsing totals.
///
/// Hooks only exist if the scripts that declared them were loaded: a frontend
/// with scripts disabled needs no separate guard.
pub fn notify_failed(totals: api::RunTotals, cleared: &[Cleared], problem: &str) {
    let hooks = hooks_of(HookKind::Failed);

    if hooks.is_empty() {
        return;
    }

    let (started_at, duration) = started();
    let report = api::report(totals, cleared, started_at, duration);
    let problem = problem.to_owned();

    let handle = std::thread::spawn(move || {
        for hook in hooks {
            if let Err(error) = hook.failed(&report, &problem) {
                database::diag::warn(format!("error hook: {error}"));
            }
        }
    });

    let mut running = match HOOK_THREADS.lock() {
        Ok(running) => running,
        Err(poisoned) => poisoned.into_inner(),
    };
    running.push(handle);
}

static HOOK_THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// How many hooks the loaded scripts registered, of every phase.
///
/// For diagnostics and tests: the hooks are global rather than part of
/// [`Scripts`], because a script declares them in `main` and they outlive the
/// load that discovered them.
pub fn finished_hooks() -> usize {
    match hooks().read() {
        Ok(list) => list.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

/// Waits up to `timeout` for the hooks started by [`notify_finished`].
///
/// Returns `true` when every hook finished. For the terminal frontend, which is
/// asked to notify from a script (`cron`-style runs) and then exits immediately:
/// without this the process could tear the request down before it was sent.
/// The window frontends never call it — their hooks have as long as the window
/// stays open.
pub fn wait_for_hooks(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    loop {
        let mut running = match HOOK_THREADS.lock() {
            Ok(running) => running,
            Err(poisoned) => poisoned.into_inner(),
        };

        running.retain(|handle| !handle.is_finished());

        if running.is_empty() {
            return true;
        }

        if std::time::Instant::now() >= deadline {
            return false;
        }

        drop(running);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Layers the declarative rules over `database`.
pub fn with_scripts(database: CleanerDatabase) -> CleanerDatabase {
    database.with_entries(load().files.iter().cloned())
}

/// Layers the declarative registry rules over `database`.
///
/// The rules are collected on every platform, but the registry database only
/// exists on Windows, so this is Windows-only.
#[cfg(windows)]
pub fn with_registry_scripts(database: RegistryDatabase) -> RegistryDatabase {
    let rules = load().registry.iter().map(RegistryRule::to_database_rule);
    database.with_entries(rules)
}

fn build(directory: PathBuf) -> Scripts {
    let mut scripts = Scripts {
        directory: directory.clone(),
        ..Scripts::default()
    };

    // The directory is never created here: a user who writes no scripts should
    // not find an empty folder appear in their data directory because the
    // program was started. A missing directory simply holds no scripts.
    let files = script_files(&directory);

    // Nothing to load: do not build the Rune context at all. It carries every
    // module the scripts may use and costs several megabytes, which a user who
    // writes no scripts should not pay on every run.
    if files.is_empty() {
        return scripts;
    }

    let context = match api::context() {
        Ok(context) => context,
        Err(error) => {
            scripts.errors.push(ScriptError::new(
                directory,
                format!("cannot build the script context: {error}"),
            ));
            return scripts;
        }
    };

    let runtime = match context.runtime() {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            scripts.errors.push(ScriptError::new(
                directory,
                format!("cannot build the script runtime: {error}"),
            ));
            return scripts;
        }
    };

    collect::begin();

    if let Ok(mut keys) = own_file_code().lock() {
        keys.clear();
    }

    for path in files {
        collect::set_script(&path);
        collect::clear_about();
        let mark = collect::mark();
        let errors_before = scripts.errors.len();
        // Read once: whether this script can be previewed safely does not
        // depend on which of its cleaners is being measured.
        let direct = fs::read_to_string(&path)
            .map(|source| touches_files_directly(&source))
            .unwrap_or(true);

        match compile_and_run(&context, &runtime, &path) {
            Ok(unit) => {
                for definition in collect::custom_since(mark) {
                    let key = format!("script-{}", NEXT_KEY.fetch_add(1, Ordering::Relaxed));

                    if direct && let Ok(mut keys) = own_file_code().lock() {
                        keys.insert(key.clone());
                    }

                    add_program(
                        key.clone(),
                        Arc::new(ScriptProgram {
                            context: runtime.clone(),
                            unit: unit.clone(),
                            handler: definition.handler.clone(),
                            glob: definition.glob,
                            within: definition.within.clone(),
                            script: path.to_string_lossy().into_owned(),
                        }),
                    );

                    scripts
                        .custom
                        .push(api::custom_to_database(key, definition));
                }

                for hook in collect::hooks_since(mark) {
                    add_hook(
                        hook.kind,
                        Arc::new(ScriptProgram {
                            context: runtime.clone(),
                            unit: unit.clone(),
                            handler: hook.handler,
                            glob: false,
                            within: None,
                            script: path.to_string_lossy().into_owned(),
                        }),
                    );
                }
            }
            Err(error) => {
                collect::rollback(mark);
                scripts.errors.push(error);
            }
        }

        let after = collect::counts();
        let failure = scripts
            .errors
            .get(errors_before)
            .map(|error| error.message.clone());

        scripts.reports.push(ScriptSummary {
            about: collect::take_about(),
            path: path.clone(),
            files: after.files.saturating_sub(mark.files),
            registry: after.registry.saturating_sub(mark.registry),
            custom: after.custom.saturating_sub(mark.custom),
            hooks: after.hooks.saturating_sub(mark.hooks),
            complaints: after.errors.saturating_sub(mark.errors),
            error: failure,
        });
    }

    let collected = collect::finish();
    scripts.files = collected.files;
    scripts.registry = collected.registry;
    scripts.errors.extend(collected.errors);

    scripts
}

/// Every `*.rn` / `*.rune` file in `directory`, in a stable order.
fn script_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        matches!(extension.to_ascii_lowercase().as_str(), "rn" | "rune")
                    })
        })
        .collect();

    files.sort();
    files
}

fn compile_and_run(
    context: &rune::Context,
    runtime: &Arc<RuntimeContext>,
    path: &Path,
) -> Result<Arc<Unit>, ScriptError> {
    let mut sources = Sources::new();
    let source = Source::from_path(path)
        .map_err(|error| ScriptError::new(path, format!("cannot read the script: {error}")))?;
    sources
        .insert(source)
        .map_err(|error| ScriptError::new(path, format!("cannot load the script: {error}")))?;

    let mut diagnostics = Diagnostics::new();

    let built = rune::prepare(&mut sources)
        .with_context(context)
        .with_diagnostics(&mut diagnostics)
        .build();

    if diagnostics.has_error() || built.is_err() {
        return Err(ScriptError::new(
            path,
            render_diagnostics(&diagnostics, &sources),
        ));
    }

    if !diagnostics.is_empty() {
        database::diag::warn(format!(
            "{}: {}",
            path.display(),
            render_diagnostics(&diagnostics, &sources)
        ));
    }

    let unit = Arc::new(
        built.map_err(|error| ScriptError::new(path, format!("cannot compile: {error}")))?,
    );

    // `main` registers the script's rules. It may `.await` — a script that
    // reads a config file or asks a local service what to clean does — so it is
    // driven like any other handler.
    drive({
        let context = runtime.clone();
        let unit = unit.clone();

        async move {
            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute(["main"], ())
                .map_err(|error| format!("main(): {error}"))?;

            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("main(): {error}"))?;

            Ok(())
        }
    })
    .map_err(|error| ScriptError::new(path, error))?;

    Ok(unit)
}

fn render_diagnostics(diagnostics: &Diagnostics, sources: &Sources) -> String {
    let mut buffer = rune::termcolor::Buffer::no_color();

    match diagnostics.emit(&mut buffer, sources) {
        Ok(()) => String::from_utf8_lossy(buffer.as_slice()).trim().to_owned(),
        Err(error) => format!("cannot render diagnostics: {error}"),
    }
}

/// The cleaning function script-driven cleaners are registered with.
pub(crate) fn script_entry(
    data: &CustomCleaner,
    sender: Option<Sender<String>>,
    _dry_run: bool,
) -> Pin<Box<dyn Future<Output = CleanerResult> + Send>> {
    let data = data.clone();

    Box::pin(async move {
        let result = blank_result(&data);

        let Some(key) = data.args.first().cloned() else {
            database::diag::warn(format!("{}: script cleaner has no program", data.id));
            return result;
        };

        let Some(program) = program(&key) else {
            database::diag::warn(format!("{}: its script is no longer loaded", data.id));
            return result;
        };

        let pattern = data.path.to_string();

        let targets: Vec<PathBuf> = if program.glob {
            match glob::glob(&pattern) {
                Ok(paths) => paths.filter_map(Result::ok).collect(),
                Err(error) => {
                    database::diag::warn(format!(
                        "{}: invalid glob \"{pattern}\": {error}",
                        data.id
                    ));
                    return result;
                }
            }
        } else {
            vec![PathBuf::from(&pattern)]
        };

        // The whole cleaner runs on one blocking thread: see `clean_all`. The
        // fallback is only for a blocking task that never ran — the app shutting
        // down mid-clean — where nothing was removed anyway.
        let fallback = blank_result(&data);
        let id = data.id.clone();
        let mut result = match tokio::task::spawn_blocking(move || {
            program.clean_all(targets, result, sender, id)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                database::diag::warn(format!("{}: the cleaner task failed: {error}", data.id));
                fallback
            }
        };

        result.working = result.files > 0 || result.folders > 0 || !result.paths.is_empty();

        result
    })
}

/// An empty result for a cleaner that has not removed anything yet.
fn blank_result(data: &CustomCleaner) -> CleanerResult {
    CleanerResult {
        files: 0,
        folders: 0,
        bytes: 0,
        working: false,
        path: data.path.clone(),
        paths: Vec::new(),
        paths_omitted: 0,
        program: data.program.clone(),
        category: data.category.clone(),
        sub_category: data.sub_category.clone(),
        // A script cleaner runs arbitrary code, so nothing can be measured
        // ahead of it: a scan cannot walk it. These stay zero, the same way
        // they do for the built-in custom cleaners.
        locked_files: 0,
        locked_bytes: 0,
        locked: Vec::new(),
        denied_files: 0,
        denied_bytes: 0,
        denied: Vec::new(),
    }
}

/// Folds one path's removals into the result.
fn record(result: &mut CleanerResult, target: &Path, stats: Stats) {
    if stats.files == 0 && stats.folders == 0 && stats.bytes == 0 {
        return;
    }

    result.files += stats.files;
    result.folders += stats.folders;
    result.bytes += stats.bytes;

    if result.paths.len() < MAX_LISTED_PATHS {
        result.paths.push(ClearedPath {
            path: SharedPath::new(&target.to_string_lossy()),
            removed_bytes: stats.bytes,
            removed_files: stats.files,
            removed_directories: stats.folders,
        });
    } else {
        result.paths_omitted += 1;
    }
}

/// Folds refused removals into the result, apart from what was removed.
///
/// A locked file is counted with the locked ones and a permission problem with
/// the denied ones, because the remedy differs: closing a program versus
/// elevating. The reason itself stays in the script's log; the result carries
/// paths the UI can list.
fn record_failures(result: &mut CleanerResult, failures: Vec<remove::Failure>) {
    for failure in failures {
        let entry = ClearedPath {
            path: SharedPath::new(&failure.path),
            removed_bytes: failure.size,
            removed_files: 1,
            removed_directories: 0,
        };

        if failure.holds_the_file {
            result.locked_files += 1;
            result.locked_bytes += failure.size;

            if result.locked.len() < MAX_LISTED_PATHS {
                result.locked.push(entry);
            }
        } else {
            result.denied_files += 1;
            result.denied_bytes += failure.size;

            if result.denied.len() < MAX_LISTED_PATHS {
                result.denied.push(entry);
            }
        }
    }
}

/// Adds the registry keys and values the handler removed. A registry entry has
/// no size, so it shows up as a path with zero counts — the same shape the
/// engine's own registry cleaners report.
fn record_registry(result: &mut CleanerResult, removed: Vec<String>) {
    for path in removed {
        if result.paths.len() < MAX_LISTED_PATHS {
            result.paths.push(ClearedPath {
                path: SharedPath::new(&path),
                removed_bytes: 0,
                removed_files: 0,
                removed_directories: 0,
            });
        } else {
            result.paths_omitted += 1;
        }
    }
}
