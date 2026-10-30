//! The `cleaner` module scripts talk to.
//!
//! Everything a script can do is declared here: the builders that register
//! rules, the filesystem and registry calls a `clean` handler works with, the
//! [`Stats`] structure it reports through, and the hook that runs after a
//! cleaning run.

use std::collections::BTreeMap;

#[cfg(windows)]
use database::structures::CleanerDataRegistry;
use database::structures::{CleanerData, CleanerFlags, Cleared, CustomCleaner, SharedPath};
use rune::{Any, ContextError, Module};

use crate::collect::{self, CustomDefinition};
use crate::files;
use crate::registry_api;
use crate::remove::{self, Stats};

/// Builds the Rune context scripts are compiled and run in.
///
/// It is the stock Rune context plus the modules scripts are documented
/// against:
///
/// * `std::*` — strings, vectors, iterators, `Option`/`Result`, `format!`;
/// * `std::fs` — reading files (`read_to_string`);
/// * `json` — building and parsing webhook payloads;
/// * `rand`, `time`, `process` — jitter, deadlines, programs that are only
///   scriptable through their command line;
/// * `cleaner` and `cleaner::registry` — this crate.
///
/// Stdio is off unless `CROSS_CLEANER_SCRIPTS_STDIO` is set: a script printing
/// while the TUI owns the terminal would tear the frame apart. Scripts that
/// want to say something use [`log`]/[`warn`], which go to the app's log file.
pub(crate) fn context() -> Result<rune::Context, ContextError> {
    let mut context = rune::Context::with_config(stdio_enabled())?;

    context.install(rune_modules::fs::module(false)?)?;
    context.install(rune_modules::json::module(false)?)?;
    context.install(rune_modules::rand::module(false)?)?;
    context.install(rune_modules::time::module(false)?)?;
    context.install(rune_modules::process::module(false)?)?;

    context.install(module()?)?;
    // `cleaner::registry` lives in its own file because `#[rune::module]`
    // names its generated metadata after the module it declares.
    context.install(registry_api::module()?)?;
    Ok(context)
}

/// Whether scripts may write to the terminal.
///
/// Off by default, because the TUI and the window app own the screen. Setting
/// `CROSS_CLEANER_SCRIPTS_STDIO=1` turns `print!`/`println!`/`dbg!` back on,
/// which is what a script author wants while debugging from the CLI.
fn stdio_enabled() -> bool {
    std::env::var_os("CROSS_CLEANER_SCRIPTS_STDIO").is_some_and(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "off" | "no")
    })
}

/// The `cleaner` module itself.
#[rune::module(::cleaner)]
pub fn module() -> Result<Module, ContextError> {
    let mut module = Module::from_meta(self::module_meta)?;

    module.ty::<Stats>()?;
    module.function_meta(stats)?;
    module.function_meta(track)?;

    // Filesystem: what is there, and what should not be. Reading file contents
    // is `std::fs::read_to_string` from rune-modules — these are the queries it
    // does not offer and a cleaner cannot do without.
    module.function_meta(read)?;
    module.function_meta(write)?;
    module.function_meta(list)?;
    module.function_meta(is_file)?;
    module.function_meta(is_dir)?;
    module.function_meta(modified)?;
    module.function_meta(now)?;
    module.function_meta(os)?;
    module.function_meta(glob)?;

    module.function_meta(remove_all)?;
    module.function_meta(remove_files)?;
    module.function_meta(remove_directories)?;
    module.function_meta(remove_file)?;
    module.function_meta(remove_directory)?;
    module.function_meta(size)?;
    module.function_meta(exists)?;

    // Diagnostics and network, for the finished hook.
    module.function_meta(log)?;
    module.function_meta(warn)?;
    module.function_meta(http_get)?;
    module.function_meta(http_post)?;

    // Hooks.
    module.function_meta(on_finished)?;

    // The report a finished hook receives.
    module.ty::<ProgramReport>()?;
    module.ty::<Report>()?;
    module.function_meta(ProgramReport::categories)?;
    module.function_meta(ProgramReport::in_category)?;
    module.function_meta(ProgramReport::size)?;
    module.function_meta(ProgramReport::line)?;
    module.function_meta(Report::entries)?;
    module.function_meta(Report::summary)?;
    module.function_meta(Report::details)?;
    module.function_meta(Report::size)?;
    module.function_meta(Report::program)?;
    module.function_meta(Report::category)?;

    module.ty::<FileCleaner>()?;
    module.function_meta(file_cleaner)?;
    module.function_meta(FileCleaner::category)?;
    module.function_meta(FileCleaner::sub_category)?;
    module.function_meta(FileCleaner::class)?;
    module.function_meta(FileCleaner::path)?;
    module.function_meta(FileCleaner::os)?;
    module.function_meta(FileCleaner::files_to_remove)?;
    module.function_meta(FileCleaner::directories_to_remove)?;
    module.function_meta(FileCleaner::remove_all_in_dir)?;
    module.function_meta(FileCleaner::remove_directory_after_clean)?;
    module.function_meta(FileCleaner::remove_directories)?;
    module.function_meta(FileCleaner::remove_files)?;
    module.function_meta(FileCleaner::register)?;

    module.ty::<RegistryCleaner>()?;
    module.function_meta(registry_cleaner)?;
    module.function_meta(RegistryCleaner::category)?;
    module.function_meta(RegistryCleaner::sub_category)?;
    module.function_meta(RegistryCleaner::class)?;
    module.function_meta(RegistryCleaner::path)?;
    module.function_meta(RegistryCleaner::os)?;
    module.function_meta(RegistryCleaner::values_to_remove)?;
    module.function_meta(RegistryCleaner::keys_to_remove)?;
    module.function_meta(RegistryCleaner::remove_values)?;
    module.function_meta(RegistryCleaner::remove_trees)?;
    module.function_meta(RegistryCleaner::remove_all_in_tree)?;
    module.function_meta(RegistryCleaner::remove_all_in_registry)?;
    module.function_meta(RegistryCleaner::register)?;

    module.ty::<CustomCleanerBuilder>()?;
    module.function_meta(custom_cleaner)?;
    module.function_meta(CustomCleanerBuilder::category)?;
    module.function_meta(CustomCleanerBuilder::sub_category)?;
    module.function_meta(CustomCleanerBuilder::path)?;
    module.function_meta(CustomCleanerBuilder::os)?;
    module.function_meta(CustomCleanerBuilder::handler)?;
    module.function_meta(CustomCleanerBuilder::glob)?;
    module.function_meta(CustomCleanerBuilder::sequential)?;
    module.function_meta(CustomCleanerBuilder::id)?;
    module.function_meta(CustomCleanerBuilder::register)?;

    Ok(module)
}

// --- results ---------------------------------------------------------------

/// What this `clean` handler removed so far: files, folders and bytes.
///
/// The removal functions below already add to it, so a handler only calls
/// [`track`] when it deletes with code of its own.
#[rune::function]
fn stats() -> Stats {
    remove::current()
}

/// Records work the handler did itself, without a `cleaner::remove_*` call.
///
/// ```rune
/// pub fn clean(path) {
///     for entry in cleaner::list(path) {
///         if entry.ends_with(".tmp") {
///             let bytes = cleaner::size(entry);
///             cleaner::remove_file(entry);
///             // `remove_file` already counted it; this would count it twice.
///         }
///     }
/// }
/// ```
#[rune::function]
fn track(files: u64, folders: u64, bytes: u64) {
    remove::track(Stats {
        files,
        folders,
        bytes,
    });
}

// --- filesystem ------------------------------------------------------------

/// Reads a file as UTF-8 text. `None` if it is missing or unreadable.
///
/// `fs::read_to_string` does the same thing, but it is `async` — a
/// non-`async` handler cannot `.await` it, so this is the call to reach for in
/// the common case of a handler that just looks at a file and decides.
#[rune::function]
fn read(path: &str) -> Option<String> {
    files::read(path)
}

/// Writes `data` to `path`, replacing the file. `true` on success.
#[rune::function]
fn write(path: &str, data: &str) -> bool {
    files::write(path, data)
}

/// The entries of a directory as full paths, sorted.
///
/// A missing directory is an empty list, so a `for` loop over it is safe even
/// when the program it belongs to was never installed.
#[rune::function]
fn list(path: &str) -> Vec<String> {
    files::list(path)
}

/// Whether `path` is a regular file.
#[rune::function]
fn is_file(path: &str) -> bool {
    files::is_file(path)
}

/// Whether `path` is a directory.
#[rune::function]
fn is_dir(path: &str) -> bool {
    files::is_dir(path)
}

/// Last modification time in seconds since the Unix epoch.
///
/// `None` when the path is missing or has no timestamp. Compare it with [`now`]
/// to delete only what is old enough:
///
/// ```rune
/// let week = 7 * 24 * 60 * 60;
/// let cutoff = cleaner::now() - week;
///
/// for entry in cleaner::list(logs) {
///     let old = match cleaner::modified(entry) {
///         Some(when) => when < cutoff,
///         None => false,
///     };
///
///     if old {
///         cleaner::remove_file(entry);
///     }
/// }
/// ```
#[rune::function]
fn modified(path: &str) -> Option<u64> {
    files::modified(path)
}

/// Now, in seconds since the Unix epoch.
#[rune::function]
fn now() -> u64 {
    files::now()
}

/// The operating system this build runs on: `"windows"`, `"linux"`,
/// `"macos"` or `"android"`.
///
/// `os([cleaner::os()])` restricts a rule to the machine it is already on,
/// which is how a shared script can load everywhere and still be explicit.
#[rune::function]
fn os() -> String {
    std::env::consts::OS.to_owned()
}

/// Expands a glob pattern into full paths, sorted.
#[rune::function]
fn glob(pattern: &str) -> Vec<String> {
    files::glob(pattern)
}

// --- removal ---------------------------------------------------------------

/// Removes everything inside `path`, keeping `path` itself.
#[rune::function]
fn remove_all(path: &str) -> Stats {
    remove::remove_all(path)
}

/// Removes the files under `path` that match `pattern`.
#[rune::function]
fn remove_files(path: &str, pattern: &str) -> Stats {
    remove::remove_files(path, pattern)
}

/// Removes the directories under `path` that match `pattern`, with everything
/// inside them.
#[rune::function]
fn remove_directories(path: &str, pattern: &str) -> Stats {
    remove::remove_directories(path, pattern)
}

/// Removes one file.
#[rune::function]
fn remove_file(path: &str) -> Stats {
    remove::remove_file(path)
}

/// Removes one directory with everything inside it.
#[rune::function]
fn remove_directory(path: &str) -> Stats {
    remove::remove_directory(path)
}

/// Total size of the files under `path`, in bytes.
#[rune::function]
fn size(path: &str) -> u64 {
    remove::size(path)
}

/// Whether anything exists at `path`.
#[rune::function]
fn exists(path: &str) -> bool {
    remove::exists(path)
}

// --- diagnostics and network ----------------------------------------------

/// Writes a line to the application log, for a script that wants to say
/// something without touching the terminal.
#[rune::function]
fn log(message: &str) {
    database::diag::info(format!("script: {message}"));
}

/// Writes a warning to the application log.
#[rune::function]
fn warn(message: &str) {
    database::diag::warn(format!("script: {message}"));
}

/// Fetches `url` and returns its body as text. `None` on any failure.
#[rune::function]
fn http_get(url: &str) -> Option<String> {
    files::http_get(url)
}

/// POSTs `body` to `url` as JSON. `true` when the server accepted it.
///
/// This is what the finished hook uses to reach a webhook — a Telegram bot,
/// a Discord webhook, a home server. Building the payload with `json` is
/// easier than quoting it by hand: `json::to_string(#{ ... })` takes a Rune
/// object and returns the JSON text.
///
/// ```rune
/// pub async fn notify(report) {
///     let body = json::to_string(#{
///         "chat_id": "12345",
///         "text": report.summary(),
///     })?;
///
///     cleaner::http_post("https://api.telegram.org/bot<token>/sendMessage", body);
/// }
/// ```
#[rune::function]
fn http_post(url: &str, body: &str) -> bool {
    files::http_post(url, body)
}

// --- hooks -----------------------------------------------------------------

/// Registers `handler` to run once after a cleaning run finishes.
///
/// The handler is called with a report of the whole run, and runs on a
/// background thread, so a slow webhook never holds up the results page. A
/// script with a hook still has to declare `main`, even if it is empty.
///
/// ```rune
/// pub fn main() {
///     cleaner::on_finished("notify");
/// }
///
/// pub async fn notify(report) {
///     let body = json::to_string(#{ "text": report.summary() })?;
///     cleaner::http_post(url, body);
/// }
/// ```
#[rune::function]
fn on_finished(handler: &str) {
    collect::hook(handler.to_owned());
}

// --- builders --------------------------------------------------------------

/// A declarative file and folder rule.
///
/// One rule is one path pattern plus what to remove there: the engine expands
/// `{username}`, `{steam}` and `{drive}` the same way it does for the built-in
/// catalog.
///
/// ```rune
/// cleaner::file_cleaner("My App")
///     .category("Cache")
///     .path("{username}/AppData/Local/My App/Cache")
///     .remove_all_in_dir()
///     .register();
/// ```
#[derive(Debug, Clone, Any)]
pub struct FileCleaner {
    program: String,
    category: String,
    sub_category: String,
    class: String,
    path: String,
    os: Vec<String>,
    files_to_remove: Vec<String>,
    directories_to_remove: Vec<String>,
    flags: u8,
}

/// Starts a file and folder rule for `program`.
#[rune::function]
fn file_cleaner(program: &str) -> FileCleaner {
    FileCleaner {
        program: program.to_owned(),
        category: String::from("Other"),
        sub_category: String::new(),
        class: String::from("Other"),
        path: String::new(),
        os: Vec::new(),
        files_to_remove: Vec::new(),
        directories_to_remove: Vec::new(),
        flags: 0,
    }
}

impl FileCleaner {
    /// The category the rule is listed under, e.g. `"Cache"`.
    #[rune::function(instance)]
    fn category(&mut self, value: &str) -> Self {
        self.category = value.to_owned();
        self.clone()
    }

    /// The sub-category shown under the category.
    #[rune::function(instance)]
    fn sub_category(&mut self, value: &str) -> Self {
        self.sub_category = value.to_owned();
        self.clone()
    }

    /// The program class (`"Utility"`, `"Game"`, ...).
    #[rune::function(instance)]
    fn class(&mut self, value: &str) -> Self {
        self.class = value.to_owned();
        self.clone()
    }

    /// The path to clean. Supports `{username}`, `{steam}` and `{drive}`.
    #[rune::function(instance)]
    fn path(&mut self, value: &str) -> Self {
        self.path = value.to_owned();
        self.clone()
    }

    /// Restricts the rule to some operating systems, e.g. `["windows"]`.
    ///
    /// Without a call the rule applies everywhere.
    #[rune::function(instance)]
    fn os(&mut self, values: Vec<String>) -> Self {
        self.os = values;
        self.clone()
    }

    /// Removes the files whose names match one of these globs inside the path.
    #[rune::function(instance)]
    fn files_to_remove(&mut self, values: Vec<String>) -> Self {
        self.files_to_remove = values;
        self.clone()
    }

    /// Removes the directories whose names match one of these globs inside the
    /// path.
    #[rune::function(instance)]
    fn directories_to_remove(&mut self, values: Vec<String>) -> Self {
        self.directories_to_remove = values;
        self.clone()
    }

    /// Removes everything inside the path and keeps the path.
    #[rune::function(instance)]
    fn remove_all_in_dir(&mut self) -> Self {
        self.flags |= CleanerFlags::REMOVE_ALL_IN_DIR.bits();
        self.clone()
    }

    /// Removes the path itself once it has been emptied.
    #[rune::function(instance)]
    fn remove_directory_after_clean(&mut self) -> Self {
        self.flags |= CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN.bits();
        self.clone()
    }

    /// Removes the sub-directories of the path.
    #[rune::function(instance)]
    fn remove_directories(&mut self) -> Self {
        self.flags |= CleanerFlags::REMOVE_DIRECTORIES.bits();
        self.clone()
    }

    /// Removes the files directly inside the path.
    #[rune::function(instance)]
    fn remove_files(&mut self) -> Self {
        self.flags |= CleanerFlags::REMOVE_FILES.bits();
        self.clone()
    }

    /// Registers the rule. Call it last: everything set before it is what gets
    /// registered.
    #[rune::function(instance)]
    fn register(&self) {
        if !os_matches(&self.os) {
            return;
        }

        match self.build() {
            Ok(entry) => collect::file(entry),
            Err(message) => collect::error(message),
        }
    }

    fn build(&self) -> Result<CleanerData, String> {
        let name = display_name(&self.program);

        if self.path.trim().is_empty() {
            return Err(format!("file cleaner {name}: path(...) is required"));
        }

        let flags = CleanerFlags::from_bits_truncate(self.flags);

        if flags.is_empty()
            && self.files_to_remove.is_empty()
            && self.directories_to_remove.is_empty()
        {
            return Err(format!(
                "file cleaner {name}: nothing to remove — call remove_all_in_dir(), \
                 remove_files(), remove_directories() or set files_to_remove(...)"
            ));
        }

        Ok(CleanerData {
            path: SharedPath::new(&self.path),
            category: self.category.as_str().into(),
            program: self.program.as_str().into(),
            class: self.class.as_str().into(),
            sub_category: self.sub_category.as_str().into(),
            files_to_remove: self
                .files_to_remove
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            directories_to_remove: self
                .directories_to_remove
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            flags,
        })
    }
}

/// A registry rule, declared with the same chainable shape as a file rule and
/// applied on Windows only.
///
/// ```rune
/// // Rune has no raw strings: a backslash in a path is written doubled, the
/// // same way it is in Rust.
/// cleaner::registry_cleaner("My App")
///     .category("LastActivity")
///     .path("HKEY_CURRENT_USER\\Software\\My App\\Recent")
///     .remove_values("*")
///     .register();
/// ```
#[derive(Debug, Clone, Any)]
pub struct RegistryCleaner {
    program: String,
    category: String,
    sub_category: String,
    class: String,
    path: String,
    os: Vec<String>,
    values_to_remove: Vec<String>,
    keys_to_remove: Vec<String>,
    remove_values: String,
    remove_trees: String,
    remove_all_in_tree: bool,
    remove_all_in_registry: bool,
}

/// Starts a registry rule for `program`.
#[rune::function]
fn registry_cleaner(program: &str) -> RegistryCleaner {
    RegistryCleaner {
        program: program.to_owned(),
        category: String::from("Other"),
        sub_category: String::new(),
        class: String::from("Other"),
        path: String::new(),
        os: Vec::new(),
        values_to_remove: Vec::new(),
        keys_to_remove: Vec::new(),
        remove_values: String::new(),
        remove_trees: String::new(),
        remove_all_in_tree: false,
        remove_all_in_registry: false,
    }
}

impl RegistryCleaner {
    /// The category the rule is listed under.
    #[rune::function(instance)]
    fn category(&mut self, value: &str) -> Self {
        self.category = value.to_owned();
        self.clone()
    }

    /// The sub-category shown under the category.
    #[rune::function(instance)]
    fn sub_category(&mut self, value: &str) -> Self {
        self.sub_category = value.to_owned();
        self.clone()
    }

    /// The program class, mirroring the catalog's field.
    #[rune::function(instance)]
    fn class(&mut self, value: &str) -> Self {
        self.class = value.to_owned();
        self.clone()
    }

    /// The registry key to clean, e.g. `HKEY_CURRENT_USER\Software\App`.
    #[rune::function(instance)]
    fn path(&mut self, value: &str) -> Self {
        self.path = value.to_owned();
        self.clone()
    }

    /// Restricts the rule to some operating systems.
    #[rune::function(instance)]
    fn os(&mut self, values: Vec<String>) -> Self {
        self.os = values;
        self.clone()
    }

    /// Removes these values by exact name.
    #[rune::function(instance)]
    fn values_to_remove(&mut self, values: Vec<String>) -> Self {
        self.values_to_remove = values;
        self.clone()
    }

    /// Removes these sub-keys by exact name.
    #[rune::function(instance)]
    fn keys_to_remove(&mut self, values: Vec<String>) -> Self {
        self.keys_to_remove = values;
        self.clone()
    }

    /// Removes the values whose names match this glob. `"*"` removes them all.
    #[rune::function(instance)]
    fn remove_values(&mut self, value: &str) -> Self {
        self.remove_values = value.to_owned();
        self.clone()
    }

    /// Removes the sub-keys whose names match this glob.
    #[rune::function(instance)]
    fn remove_trees(&mut self, value: &str) -> Self {
        self.remove_trees = value.to_owned();
        self.clone()
    }

    /// Removes every value found anywhere under the key.
    #[rune::function(instance)]
    fn remove_all_in_tree(&mut self) -> Self {
        self.remove_all_in_tree = true;
        self.clone()
    }

    /// Removes every value directly under the key.
    #[rune::function(instance)]
    fn remove_all_in_registry(&mut self) -> Self {
        self.remove_all_in_registry = true;
        self.clone()
    }

    /// Registers the rule. Call it last.
    #[rune::function(instance)]
    fn register(&self) {
        if !os_matches(&self.os) {
            return;
        }

        match self.build() {
            Ok(rule) => collect::registry(rule),
            Err(message) => collect::error(message),
        }
    }

    fn build(&self) -> Result<RegistryRule, String> {
        let name = display_name(&self.program);

        if self.path.trim().is_empty() {
            return Err(format!("registry cleaner {name}: path(...) is required"));
        }

        let has_action = self.remove_all_in_tree
            || self.remove_all_in_registry
            || !self.values_to_remove.is_empty()
            || !self.keys_to_remove.is_empty()
            || !self.remove_values.is_empty()
            || !self.remove_trees.is_empty();

        if !has_action {
            return Err(format!(
                "registry cleaner {name}: nothing to remove — call remove_values(...), \
                 remove_trees(...), values_to_remove(...) or remove_all_in_tree()"
            ));
        }

        Ok(RegistryRule {
            program: self.program.clone(),
            category: self.category.clone(),
            sub_category: self.sub_category.clone(),
            class: self.class.clone(),
            path: self.path.clone(),
            values_to_remove: self.values_to_remove.clone(),
            keys_to_remove: self.keys_to_remove.clone(),
            remove_values: self.remove_values.clone(),
            remove_trees: self.remove_trees.clone(),
            remove_all_in_tree: self.remove_all_in_tree,
            remove_all_in_registry: self.remove_all_in_registry,
        })
    }
}

/// A registry rule collected from a script.
///
/// Platform-neutral on purpose: the database's registry structures only exist
/// on Windows, but a script has to compile on every platform, so on Linux and
/// macOS the rules are collected and then simply not applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryRule {
    pub program: String,
    pub category: String,
    pub sub_category: String,
    pub class: String,
    pub path: String,
    pub values_to_remove: Vec<String>,
    pub keys_to_remove: Vec<String>,
    pub remove_values: String,
    pub remove_trees: String,
    pub remove_all_in_tree: bool,
    pub remove_all_in_registry: bool,
}

impl RegistryRule {
    /// Converts to the database's registry entry.
    #[cfg(windows)]
    pub fn to_database_rule(&self) -> CleanerDataRegistry {
        CleanerDataRegistry {
            category: self.category.as_str().into(),
            program: self.program.as_str().into(),
            class: self.class.as_str().into(),
            sub_category: self.sub_category.as_str().into(),
            remove_all_in_tree: self.remove_all_in_tree,
            remove_all_in_registry: self.remove_all_in_registry,
            path: SharedPath::new(&self.path),
            values_to_remove: self
                .values_to_remove
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            keys_to_remove: self
                .keys_to_remove
                .iter()
                .map(|value| value.as_str().into())
                .collect(),
            remove_values: self.remove_values.clone(),
            remove_trees: self.remove_trees.clone(),
        }
    }
}

/// A cleaner with its own handler, for things a path rule cannot express.
///
/// ```rune
/// cleaner::custom_cleaner("My App")
///     .category("Cache")
///     .path("{username}/AppData/Local/My App/cache")
///     .handler("clean_cache")
///     .register();
///
/// pub fn clean_cache(path) {
///     for entry in cleaner::list(path) {
///         if entry.ends_with(".tmp") {
///             cleaner::remove_file(entry);
///         }
///     }
/// }
/// ```
#[derive(Debug, Clone, Any)]
pub struct CustomCleanerBuilder {
    id: String,
    program: String,
    category: String,
    sub_category: String,
    path: String,
    os: Vec<String>,
    handler: String,
    glob: bool,
    sequential: bool,
}

/// Starts a script-driven cleaner for `program`.
#[rune::function]
fn custom_cleaner(program: &str) -> CustomCleanerBuilder {
    CustomCleanerBuilder {
        id: String::new(),
        program: program.to_owned(),
        category: String::from("Other"),
        sub_category: String::new(),
        path: String::new(),
        os: Vec::new(),
        handler: String::from("clean"),
        glob: false,
        sequential: false,
    }
}

impl CustomCleanerBuilder {
    /// The category the cleaner is listed under.
    #[rune::function(instance)]
    fn category(&mut self, value: &str) -> Self {
        self.category = value.to_owned();
        self.clone()
    }

    /// The sub-category shown under the category.
    #[rune::function(instance)]
    fn sub_category(&mut self, value: &str) -> Self {
        self.sub_category = value.to_owned();
        self.clone()
    }

    /// The path the handler receives. Supports `{username}` and `{drive}`.
    #[rune::function(instance)]
    fn path(&mut self, value: &str) -> Self {
        self.path = value.to_owned();
        self.clone()
    }

    /// Restricts the cleaner to some operating systems.
    #[rune::function(instance)]
    fn os(&mut self, values: Vec<String>) -> Self {
        self.os = values;
        self.clone()
    }

    /// The name of the function in this script that does the cleaning.
    /// Defaults to `clean`.
    #[rune::function(instance)]
    fn handler(&mut self, value: &str) -> Self {
        self.handler = value.to_owned();
        self.clone()
    }

    /// Treats the path as a glob and calls the handler once per match.
    #[rune::function(instance)]
    fn glob(&mut self) -> Self {
        self.glob = true;
        self.clone()
    }

    /// Runs this cleaner on its own, so its progress is not mixed with others.
    #[rune::function(instance)]
    fn sequential(&mut self) -> Self {
        self.sequential = true;
        self.clone()
    }

    /// An id used to enable and disable the cleaner. Defaults to the program
    /// name; only needed when one program has several script cleaners.
    #[rune::function(instance)]
    fn id(&mut self, value: &str) -> Self {
        self.id = value.to_owned();
        self.clone()
    }

    /// Registers the cleaner. Call it last.
    #[rune::function(instance)]
    fn register(&self) {
        if !os_matches(&self.os) {
            return;
        }

        if self.path.trim().is_empty() {
            collect::error(format!(
                "custom cleaner {}: path(...) is required",
                display_name(&self.program)
            ));
            return;
        }

        collect::custom(CustomDefinition {
            id: if self.id.is_empty() {
                self.program.clone()
            } else {
                self.id.clone()
            },
            program: self.program.clone(),
            category: self.category.clone(),
            sub_category: self.sub_category.clone(),
            path: self.path.clone(),
            os: self.os.clone(),
            handler: self.handler.clone(),
            glob: self.glob,
            sequential: self.sequential,
        });
    }
}

// --- the finished report ---------------------------------------------------

/// What one program contributed to a finished run.
///
/// A program that was cleaned under several categories appears once, with the
/// categories listed rather than the numbers split.
#[derive(Debug, Clone, Any)]
pub struct ProgramReport {
    /// The program's name, exactly as it appears in the results page.
    #[rune(get)]
    pub program: String,
    #[rune(get)]
    pub files: u64,
    #[rune(get)]
    pub folders: u64,
    #[rune(get)]
    pub bytes: u64,
    // Exposed through `categories()`: a `Vec` field cannot be a `#[rune(get)]`,
    // because the generated getter needs a cloneable-by-value field.
    categories: Vec<String>,
    // Read by scripts through the getters below; unreachable for the compiler.
    #[allow(dead_code)]
    size: String,
}

impl ProgramReport {
    /// The categories this program was cleaned under.
    #[rune::function(instance)]
    fn categories(&self) -> Vec<String> {
        self.categories.clone()
    }

    /// Whether this program was cleaned under `category`.
    #[rune::function(instance)]
    fn in_category(&self, category: &str) -> bool {
        self.categories.iter().any(|value| value == category)
    }

    /// The size as a human string, e.g. `"1.2 GB"`.
    #[rune::function(instance)]
    fn size(&self) -> String {
        self.size.clone()
    }

    /// One line: name, files, folders and size.
    #[rune::function(instance)]
    fn line(&self) -> String {
        self.describe()
    }

    /// The text behind `line()`.
    fn describe(&self) -> String {
        format!(
            "{}: {} files, {} folders, {}",
            self.program, self.files, self.folders, self.size,
        )
    }
}

/// What a finished cleaning run did, handed to every `on_finished` handler.
///
/// The totals are the same ones the results page shows; [`Report::entries`]
/// carries the per-program breakdown so a script can build whatever message it
/// wants — a summary line, a table, one message per program.
///
/// ```rune
/// pub async fn notify(report) {
///     let lines = [report.summary()];
///
///     for entry in report.entries() {
///         lines.push(format!(
///             "{} — {} files, {} folders, {}",
///             entry.program, entry.files, entry.folders, entry.size(),
///         ));
///     }
///
///     let body = json::to_string(#{ "text": lines.join("\n") })?;
///     cleaner::http_post(url, body);
/// }
/// ```
#[derive(Debug, Clone, Any)]
pub struct Report {
    #[rune(get)]
    pub files: u64,
    #[rune(get)]
    pub folders: u64,
    #[rune(get)]
    pub bytes: u64,
    /// How many programs contributed to the run.
    #[rune(get)]
    pub programs: u64,
    /// One entry per program, largest first.
    entries: Vec<ProgramReport>,
    // Read by scripts through the getters below; unreachable for the compiler.
    #[allow(dead_code)]
    summary: String,
    #[allow(dead_code)]
    details: String,
}

impl Report {
    /// One entry per program, largest first.
    #[rune::function(instance)]
    fn entries(&self) -> Vec<ProgramReport> {
        self.entries.clone()
    }

    /// One sentence: what the run freed, in files, folders and size.
    #[rune::function(instance)]
    fn summary(&self) -> String {
        self.summary.clone()
    }

    /// One line per program, largest first — ready to drop into a message.
    #[rune::function(instance)]
    fn details(&self) -> String {
        self.details.clone()
    }

    /// The total size as a human string, e.g. `"1.2 GB"`.
    #[rune::function(instance)]
    fn size(&self) -> String {
        database::utils::get_file_size_string(self.bytes)
    }

    /// The entry for `program`, if it was cleaned in this run.
    #[rune::function(instance)]
    fn program(&self, program: &str) -> Option<ProgramReport> {
        self.entries
            .iter()
            .find(|entry| entry.program == program)
            .cloned()
    }

    /// The entries cleaned under `category`.
    #[rune::function(instance)]
    fn category(&self, category: &str) -> Vec<ProgramReport> {
        self.entries
            .iter()
            .filter(|entry| entry.categories.iter().any(|value| value == category))
            .cloned()
            .collect()
    }
}

/// Builds the report from a finished run.
pub(crate) fn report(bytes: u64, files: u64, folders: u64, cleared: &[Cleared]) -> Report {
    // Programs appear once per category, so the rows are folded: a program
    // cleaned under two categories is one entry listing both, not two entries
    // that each hold half the numbers.
    let mut programs: BTreeMap<&str, (u64, u64, u64, Vec<String>)> = BTreeMap::new();

    for entry in cleared {
        let totals = programs.entry(entry.program.as_str()).or_default();
        totals.0 += entry.removed_files;
        totals.1 += entry.removed_directories;
        totals.2 += entry.removed_bytes;

        for category in &entry.affected_categories {
            if !totals.3.iter().any(|value| value == category) {
                totals.3.push(category.clone());
            }
        }
    }

    let mut entries: Vec<ProgramReport> = programs
        .into_iter()
        .map(
            |(program, (files, folders, bytes, categories))| ProgramReport {
                program: program.to_owned(),
                categories,
                files,
                folders,
                bytes,
                size: database::utils::get_file_size_string(bytes),
            },
        )
        .collect();

    entries.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.program.cmp(&b.program))
    });

    let summary = format!(
        "Cross Cleaner freed {}: {} files, {} folders in {} programs.",
        database::utils::get_file_size_string(bytes),
        files,
        folders,
        entries.len(),
    );

    let details = entries
        .iter()
        .map(ProgramReport::describe)
        .collect::<Vec<_>>()
        .join("\n");

    Report {
        files,
        folders,
        bytes,
        programs: entries.len() as u64,
        entries,
        summary,
        details,
    }
}

// --- helpers ---------------------------------------------------------------

fn display_name(program: &str) -> String {
    if program.trim().is_empty() {
        String::from("<unnamed>")
    } else {
        format!("\"{program}\"")
    }
}

/// Whether a rule that names `os` applies to this machine.
///
/// An empty list means "everywhere", which is also what a typo fails to
/// restrict: an unknown name simply never matches, so the rule is skipped
/// rather than silently running on the wrong platform.
fn os_matches(os: &[String]) -> bool {
    if os.is_empty() {
        return true;
    }

    os.iter()
        .any(|value| match value.trim().to_ascii_lowercase().as_str() {
            "windows" | "win" => cfg!(windows),
            "linux" => cfg!(target_os = "linux"),
            "macos" | "mac" | "osx" | "darwin" => cfg!(target_os = "macos"),
            "android" => cfg!(target_os = "android"),
            "unix" => cfg!(unix),
            other => other == std::env::consts::OS,
        })
}

/// A custom cleaner declared by a script, ready to be registered.
pub(crate) fn custom_to_database(key: String, definition: CustomDefinition) -> CustomCleaner {
    CustomCleaner {
        id: format!("script:{}:{}", definition.id, key),
        program: definition.program.as_str().into(),
        category: definition.category.as_str().into(),
        sub_category: definition.sub_category.as_str().into(),
        path: SharedPath::new(&definition.path),
        // The key is how the cleaning entry finds its compiled program.
        args: vec![key],
        os: definition.os,
        function: crate::loader::script_entry,
        sequential: definition.sequential,
    }
}
