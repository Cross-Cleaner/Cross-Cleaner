//! Finding, compiling and running the user's scripts.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::future::Future;
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
    /// The directory these scripts came from.
    pub directory: PathBuf,
}

impl Default for Scripts {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            registry: Vec::new(),
            custom: Vec::new(),
            errors: Vec::new(),
            directory: PathBuf::new(),
        }
    }
}

/// What one handler call removed.
struct Removed {
    stats: Stats,
    registry: Vec<String>,
}

/// A compiled script plus the handlers one of its registrations calls.
struct ScriptProgram {
    context: Arc<RuntimeContext>,
    unit: Arc<Unit>,
    handler: String,
    glob: bool,
}

impl ScriptProgram {
    /// Runs the handler for one path and returns what it removed.
    fn clean(&self, path: &Path) -> Result<Removed, String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let path = path.to_string_lossy().into_owned();

        drive(async move {
            remove::reset();
            registry::reset();

            let vm = Vm::new(context, unit);
            let execution = vm
                .send_execute([handler.as_str()], (path,))
                .map_err(|error| format!("{handler}(): {error}"))?;

            // The handler's return value is deliberately ignored: what it
            // removed is what the removal calls — or `cleaner::track` — really
            // counted, which a script cannot overstate by returning a number.
            execution
                .async_complete()
                .await
                .into_result()
                .map_err(|error| format!("{handler}(): {error}"))?;

            Ok(Removed {
                stats: remove::take(),
                registry: registry::take(),
            })
        })
    }

    /// Runs an `on_finished` handler with the report of a finished run.
    fn finished(&self, report: &Report) -> Result<(), String> {
        let context = self.context.clone();
        let unit = self.unit.clone();
        let handler = self.handler.clone();
        let report = report.clone();

        drive(async move {
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

fn programs() -> &'static RwLock<HashMap<String, Arc<ScriptProgram>>> {
    static PROGRAMS: OnceLock<RwLock<HashMap<String, Arc<ScriptProgram>>>> = OnceLock::new();
    PROGRAMS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn hooks() -> &'static RwLock<Vec<Arc<ScriptProgram>>> {
    static HOOKS: OnceLock<RwLock<Vec<Arc<ScriptProgram>>>> = OnceLock::new();
    HOOKS.get_or_init(|| RwLock::new(Vec::new()))
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

fn add_hook(hook: Arc<ScriptProgram>) {
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
fn platform_data_dir() -> PathBuf {
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
pub fn notify_finished(bytes: u64, files: u64, folders: u64, cleared: &[Cleared]) {
    // Hooks only exist if the scripts that declared them were loaded, so a
    // frontend with scripts disabled needs no separate guard: it never called
    // `load`, and the list below is empty.
    let hooks: Vec<Arc<ScriptProgram>> = match hooks().read() {
        Ok(list) => list.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };

    if hooks.is_empty() {
        return;
    }

    let report = api::report(bytes, files, folders, cleared);

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

static HOOK_THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// How many `on_finished` hooks the loaded scripts registered.
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

    if let Err(error) = fs::create_dir_all(&directory) {
        scripts.errors.push(ScriptError::new(
            directory,
            format!("cannot create the scripts directory: {error}"),
        ));
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

    for path in script_files(&directory) {
        collect::set_script(&path);
        let mark = collect::mark();

        match compile_and_run(&context, &runtime, &path) {
            Ok(unit) => {
                for definition in collect::custom_since(mark) {
                    let key = format!("script-{}", NEXT_KEY.fetch_add(1, Ordering::Relaxed));

                    add_program(
                        key.clone(),
                        Arc::new(ScriptProgram {
                            context: runtime.clone(),
                            unit: unit.clone(),
                            handler: definition.handler.clone(),
                            glob: definition.glob,
                        }),
                    );

                    scripts
                        .custom
                        .push(api::custom_to_database(key, definition));
                }

                for handler in collect::hooks_since(mark) {
                    add_hook(Arc::new(ScriptProgram {
                        context: runtime.clone(),
                        unit: unit.clone(),
                        handler,
                        glob: false,
                    }));
                }
            }
            Err(error) => {
                collect::rollback(mark);
                scripts.errors.push(error);
            }
        }
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
) -> Pin<Box<dyn Future<Output = CleanerResult> + Send>> {
    let data = data.clone();

    Box::pin(async move {
        let mut result = CleanerResult {
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
        };

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

        let total = targets.len();

        for (index, target) in targets.iter().enumerate() {
            // The handler runs on a blocking thread: a script may wait on a
            // file, a process or a network call, and that must not stall the
            // runtime driving every other cleaner.
            let removed = {
                let program = program.clone();
                let target = target.clone();

                match tokio::task::spawn_blocking(move || program.clean(&target)).await {
                    Ok(result) => result,
                    Err(error) => Err(format!("the cleaner task failed: {error}")),
                }
            };

            match removed {
                Ok(removed) => {
                    record(&mut result, target, removed.stats);
                    record_registry(&mut result, removed.registry);
                }
                Err(message) => database::diag::warn(format!("{}: {message}", data.id)),
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

        result.working = result.files > 0 || result.folders > 0 || !result.paths.is_empty();

        result
    })
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
