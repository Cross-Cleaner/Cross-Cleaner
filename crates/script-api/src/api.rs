//! The `cleaner` module scripts talk to.
//!
//! Everything a script can do is declared here: the builders that register
//! rules, the filesystem and registry calls a `clean` handler works with, the
//! [`Stats`] structure it reports through, and the hook that runs after a
//! cleaning run.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(windows)]
use database::structures::CleanerDataRegistry;
use database::structures::{
    CleanerData, CleanerFilter, CleanerFlags, Cleared, CustomCleaner, SharedPath,
};
use rune::{Any, ContextError, Module};

use crate::collect::{self, CustomDefinition, HookKind};
use crate::files;
use crate::registry_api;
use crate::remove::{self, Stats};
use crate::session;

/// Builds the Rune context scripts are compiled and run in.
///
/// It is the stock Rune context plus the modules scripts are documented
/// against:
///
/// * `std::*` — strings, vectors, iterators, `Option`/`Result`, `format!`;
/// * `std::fs` — reading files (`read_to_string`);
/// * `json` — building and parsing webhook payloads;
/// * `rand`, `time` — jitter and deadlines;
/// * `process` — programs that are only scriptable through their command
///   line, and only when the user allowed it in the config;
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

    // `process` is the one library that can bypass everything this crate
    // counts: a script that shells out can delete without a `remove_*` call and
    // without a rule the user can see. It is installed only when the user said
    // yes, so a script that needs it fails to load, loudly, on a machine where
    // nobody allowed it.
    if session::process_allowed() {
        context.install(rune_modules::process::module(false)?)?;
    }

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
    module.ty::<files::FileInfo>()?;
    module.ty::<files::HttpResponse>()?;
    module.function_meta(files::HttpResponse::body)?;
    module.function_meta(files::HttpResponse::ok)?;
    module.function_meta(files::HttpResponse::header)?;
    module.function_meta(files::HttpResponse::headers)?;
    module.ty::<remove::Failure>()?;
    module.function_meta(remove::Failure::locked)?;
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
    module.function_meta(is_symlink)?;
    module.function_meta(is_empty_dir)?;
    module.function_meta(canonical)?;
    module.function_meta(extension)?;
    module.function_meta(stem)?;
    module.function_meta(count)?;
    module.function_meta(stat)?;
    module.function_meta(modified)?;
    module.function_meta(now)?;
    module.function_meta(os)?;
    module.function_meta(glob)?;

    // Decisions: the questions a cleaner asks before deleting.
    module.function_meta(older_than)?;
    module.function_meta(matches)?;
    module.function_meta(unlocked)?;
    module.function_meta(locked)?;
    module.function_meta(lock_files)?;

    // Paths: the same placeholders a rule path uses, available by hand.
    module.function_meta(user)?;
    module.function_meta(home)?;
    module.function_meta(appdata)?;
    module.function_meta(local_appdata)?;
    module.function_meta(program_data)?;
    module.function_meta(program_files)?;
    module.function_meta(temp)?;
    module.function_meta(env)?;
    module.function_meta(arch)?;
    module.function_meta(drive)?;
    module.function_meta(drives)?;
    module.function_meta(steam)?;
    module.function_meta(join)?;
    module.function_meta(parent)?;
    module.function_meta(filename)?;
    module.function_meta(expand)?;
    module.function_meta(is_protected)?;
    module.function_meta(running)?;

    // The run the handler is part of.
    module.function_meta(cancelled)?;
    module.function_meta(dry_run)?;
    module.function_meta(progress)?;
    module.function_meta(progress_add)?;
    module.function_meta(progress_files)?;
    module.function_meta(notify)?;
    module.function_meta(version)?;
    module.function_meta(about)?;
    module.function_meta(require_api)?;
    module.function_meta(script_path)?;
    module.function_meta(state_get)?;
    module.function_meta(state_set)?;
    module.function_meta(state_delete)?;
    module.function_meta(state_keys)?;

    module.function_meta(remove_all)?;
    module.function_meta(remove_files)?;
    module.function_meta(remove_directories)?;
    module.function_meta(remove_file)?;
    module.function_meta(remove_directory)?;
    module.function_meta(size)?;
    module.function_meta(exists)?;
    module.function_meta(trash)?;
    module.function_meta(optimize_image)?;
    module.function_meta(optimize_images)?;
    module.function_meta(human_size)?;

    // Removals that were refused, for a handler that wants to explain why.
    module.function_meta(failures)?;
    // Diagnostics and network, for the finished hook.
    module.function_meta(log)?;
    module.function_meta(warn)?;
    module.function_meta(http_get)?;
    module.function_meta(http_post)?;
    module.function_meta(http_request)?;

    // Hooks.
    module.function_meta(on_started)?;
    module.function_meta(on_finished)?;
    module.function_meta(on_error)?;
    module.function_meta(on_scan)?;
    module.function_meta(on_program_done)?;
    module.function_meta(on_cancelled)?;

    // The report a finished hook receives.
    module.ty::<ProgramReport>()?;
    module.ty::<CategoryReport>()?;
    module.ty::<Report>()?;
    module.function_meta(ProgramReport::categories)?;
    module.function_meta(ProgramReport::in_category)?;
    module.function_meta(ProgramReport::size)?;
    module.function_meta(ProgramReport::line)?;
    module.function_meta(CategoryReport::size)?;
    module.function_meta(Report::entries)?;
    module.function_meta(Report::categories)?;
    module.function_meta(Report::incomplete)?;
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
    module.function_meta(FileCleaner::paths)?;
    module.function_meta(FileCleaner::exclude)?;
    module.function_meta(FileCleaner::older_than)?;
    module.function_meta(FileCleaner::min_size)?;
    module.function_meta(FileCleaner::max_size)?;
    module.function_meta(FileCleaner::max_depth)?;
    module.function_meta(FileCleaner::skip_if_running)?;
    module.function_meta(FileCleaner::to_trash)?;
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
    module.function_meta(CustomCleanerBuilder::paths)?;
    module.function_meta(CustomCleanerBuilder::within)?;
    module.function_meta(CustomCleanerBuilder::os)?;
    module.function_meta(CustomCleanerBuilder::handler)?;
    module.function_meta(CustomCleanerBuilder::glob)?;
    module.function_meta(CustomCleanerBuilder::sequential)?;
    module.function_meta(CustomCleanerBuilder::id)?;
    module.function_meta(CustomCleanerBuilder::register)?;

    Ok(module)
}

// --- the run a handler is part of -----------------------------------------

/// Whether the user asked the run to stop.
///
/// Cancelling is cooperative: the engine stops starting new cleaners, and a
/// handler that walks a large tree should check this between directories rather
/// than finish work nobody is waiting for any more.
///
/// ```rune
/// pub fn clean(path) {
///     for entry in cleaner::list(path) {
///         if cleaner::cancelled() {
///             return;
///         }
///         // ...
///     }
/// }
/// ```
#[rune::function]
fn cancelled() -> bool {
    session::cancelled()
}

/// Whether this run only measures: the handler is being asked what it *would*
/// remove, and nothing it deletes survives.
///
/// `remove_*` refuses every call while this is true and counts nothing, so a
/// handler written for a real run answers a preview with `0` unless it checks
/// here and reports what it found instead.
#[rune::function]
fn dry_run() -> bool {
    session::dry_run()
}

/// Says what the handler is doing, in the place the frontend shows progress.
///
/// The text is a plain line (`"My App: checking 1200 files"`), not a percentage:
/// the engine owns the counters and a script's own idea of its progress is not
/// something it can add to them.
#[rune::function]
fn progress(text: &str) {
    session::progress(text);
}

/// Adds `files` to the progress counter, for a handler that walks files itself.
///
/// The engine already counts what its own cleaners remove; this is for the
/// files a handler counts while looking, so a long handler does not look
/// stuck. It is a hint, not an accounting record: the totals in the report come
/// from what was actually removed.
#[rune::function]
fn progress_add(files: u64) {
    session::progress(&format!("+{files} files"));
}

/// Says `done` of `total` files are checked.
///
/// For the same case as [`progress_add`], a handler walking a tree in its own
/// loop.
#[rune::function]
fn progress_files(done: u64, total: u64) {
    session::progress(&format!("{done}/{total} files"));
}

/// Shows a desktop notification with `title` and `body`.
///
/// The same notification the app itself shows, on every platform, without a
/// script calling `notify-send` or `powershell`. Safe to call from a finished
/// hook: it does not block the cleaning job.
#[rune::function]
fn notify(title: &str, body: &str) {
    session::notify(title, body);
}

/// The Cross Cleaner version running the script, e.g. `"2.0.4.2.1"`.
#[rune::function]
fn version() -> String {
    session::version()
}

/// Describes the script itself, for `cli scripts` and a maintenance page.
///
/// Purely informational: a script without it works exactly the same, it just
/// shows up under its file name with no description. Call it from `main()`.
///
/// ```rune
/// pub fn main() {
///     cleaner::about("Cache Sweeper", "1.0", "you", "clears my build caches");
/// }
/// ```
#[rune::function]
fn about(name: &str, version: &str, author: &str, description: &str) {
    collect::about(name, version, author, description);
}

/// Fails when the build's script API is older than `level`.
///
/// Call it first in `main()`: a script written against a newer API otherwise
/// fails with "item not found", which reads as a broken script rather than an
/// old build.
#[rune::function]
fn require_api(level: u64) -> Result<(), String> {
    session::require_api(level)
}

/// The script file whose handler is running.
///
/// For a `User-Agent`, a log line or a message that says *which* of several
/// scripts reported, without the script hard-coding its own name.
#[rune::function]
fn script_path() -> Option<String> {
    session::script_path()
}

/// A value this script saved with [`state_set`] in a previous run.
///
/// The store is per script file, kept beside the scripts in `.state`, and
/// survives restarts, updates and a new scan. It is plain strings: numbers,
/// dates and lists are the script's business to format and parse.
#[rune::function]
fn state_get(key: &str) -> Option<String> {
    crate::state::get(key)
}

/// Saves `value` under `key` for the next run. `true` on success.
///
/// Writes are atomic: a run killed mid-save keeps the previous value instead
/// of a half written file. A preview may also save; the store is script
/// bookkeeping, not user data.
#[rune::function]
fn state_set(key: &str, value: &str) -> bool {
    crate::state::set(key, value)
}

/// Forgets `key`. `true` when it existed and the store was written.
#[rune::function]
fn state_delete(key: &str) -> bool {
    crate::state::delete(key)
}

/// Every key this script has saved, sorted.
#[rune::function]
fn state_keys() -> Vec<String> {
    crate::state::keys()
}

/// Whether `path` is a system location a cleaner should not remove.
///
/// `true` for the Windows directory, the program files, the user's home itself,
/// the system directories, and for anything that contains one of them. It is a
/// question, not a wall: checking it before a `remove_all` turns "the path was
/// built wrong" into a refusal instead of a reinstall.
#[rune::function]
fn is_protected(path: &str) -> bool {
    files::is_protected(path)
}

/// Whether any of `programs` is running right now.
///
/// The same check a rule's `skip_if_running` makes, so a handler and a rule
/// agree on what "the browser is open" means.
#[rune::function]
fn running(programs: Vec<String>) -> bool {
    files::running(programs)
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

/// The removals this handler attempted that did not happen, with reasons.
///
/// A locked file or a directory the system refuses leaves the counters at
/// zero, exactly like a file that was not there. This is how a handler tells
/// the two apart: log the reason, remove the rest, or report that an
/// application has to be closed first. The list is per `clean` call and holds
/// at most 1024 entries.
///
/// ```rune
/// pub fn clean(path) {
///     for failure in cleaner::failures() {
///         cleaner::warn(`${failure.path}: ${failure.reason}`);
///     }
/// }
/// ```
#[rune::function]
fn failures() -> Vec<remove::Failure> {
    remove::failures()
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

/// Whether `path` is a symlink, without following it.
///
/// A cleaner that removes a link with `remove_file` removes the link itself,
/// never the directory it points at; this is how a handler can tell the two
/// apart before it decides.
#[rune::function]
fn is_symlink(path: &str) -> bool {
    files::is_symlink(path)
}

/// Whether `path` is a directory with no entries at all.
///
/// A missing path is `false`: ask [`is_dir`] first if that distinction matters.
#[rune::function]
fn is_empty_dir(path: &str) -> bool {
    files::is_empty_dir(path)
}

/// The canonical, absolute form of `path`, or `None` when it does not exist.
///
/// Symlinks are resolved. Compare a path against `canonical("{home}")` when a
/// rule has to know whether it is looking at the real profile or a link to it.
#[rune::function]
fn canonical(path: &str) -> Option<String> {
    files::canonical(path)
}

/// The extension of `path` without the dot, lowercased as written on disk.
#[rune::function]
fn extension(path: &str) -> Option<String> {
    files::extension(path)
}

/// The file name of `path` without its extension.
#[rune::function]
fn stem(path: &str) -> Option<String> {
    files::stem(path)
}

/// How many entries `base/pattern` matches, without removing anything.
///
/// The pattern is the one [`remove_files`] takes. A handler that walks a huge
/// cache can use this to decide whether the cleanup is worth starting.
#[rune::function]
fn count(base: &str, pattern: &str) -> u64 {
    files::count(base, pattern)
}

/// What the path is, in one call: size, timestamps and file type.
///
/// The path itself is examined, never a symlink target, so the answer matches
/// what [`remove_file`] would do with it.
///
/// ```rune
/// let info = cleaner::stat(path)?;
/// if info.is_file && info.size > 100 * 1024 * 1024 {
///     cleaner::remove_file(path);
/// }
/// ```
#[rune::function]
fn stat(path: &str) -> Option<files::FileInfo> {
    files::stat(path)
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

/// Whether `path` is older than `days` days.
///
/// The one question almost every hand-written cleaner asks, and the five lines
/// it used to take — read the modification time, match on it, compare against
/// `now()` — collapsed into one call. A path that does not exist is `false`.
///
/// ```rune
/// for file in cleaner::glob(`${path}/*.log`) {
///     if cleaner::older_than(file, 7) {
///         cleaner::remove_file(file);
///     }
/// }
/// ```
#[rune::function]
fn older_than(path: &str, days: u64) -> bool {
    files::older_than(path, days)
}

/// Whether the last component of `path` matches the name glob `pattern`.
///
/// For deciding about a specific path without expanding a whole pattern, and
/// for `cleaner::remove_*` calls a script builds from its own list.
///
/// ```rune
/// if cleaner::matches(entry, "*.tmp") {
///     cleaner::remove_file(entry);
/// }
/// ```
#[rune::function]
fn matches(path: &str, pattern: &str) -> bool {
    files::matches(path, pattern)
}

/// Whether `path` can be opened for writing right now.
///
/// `false` means another program holds it — the log an app is still writing,
/// the cache a browser keeps mapped — or the user may not write it. What a
/// cleaner checks before it spends time on a directory that will refuse to go.
#[rune::function]
fn unlocked(path: &str) -> bool {
    files::unlocked(path)
}

/// Whether `path` is locked by another program.
///
/// The inverse of [`unlocked`], named for the branch that reads better.
#[rune::function]
fn locked(path: &str) -> bool {
    files::locked(path)
}

/// Every file under `path` that is locked right now, sorted.
///
/// A script that has to leave a directory alone while one file in it is open
/// asks this once rather than walking the tree itself.
///
/// ```rune
/// let busy = cleaner::lock_files(cache);
///
/// if busy.is_empty() {
///     cleaner::remove_all(cache);
/// } else {
///     cleaner::log(`left ${cache} alone: ${busy.len()} locked`);
/// }
/// ```
#[rune::function]
fn lock_files(path: &str) -> Vec<String> {
    files::lock_files(path)
}

// --- paths -----------------------------------------------------------------

/// The current user's name — what `{username}` expands to.
#[rune::function]
fn user() -> String {
    files::user()
}

/// The current user's home directory — what `{home}` expands to.
#[rune::function]
fn home() -> String {
    files::home()
}

/// The per-user application data directory: `AppData\Roaming` on Windows, the
/// XDG config directory on Linux, `Application Support` on macOS.
#[rune::function]
fn appdata() -> String {
    files::appdata()
}

/// The per-user application data directory: `%LOCALAPPDATA%` on Windows,
/// `$XDG_DATA_HOME` (or `~/.local/share`) on Linux, `Application Support` on
/// macOS.
///
/// This is where other programs keep their per-user caches. It is not
/// [`appdata`], which is Cross Cleaner's own directory.
#[rune::function]
fn local_appdata() -> String {
    files::local_appdata()
}

/// `%PROGRAMDATA%` on Windows; `None` on the platforms that have no equivalent.
#[rune::function]
fn program_data() -> Option<String> {
    files::program_data()
}

/// `%ProgramFiles%` on Windows; `None` on the platforms that have no equivalent.
#[rune::function]
fn program_files() -> Option<String> {
    files::program_files()
}

/// The platform's temporary directory.
///
/// The same directory `std::env::temp_dir()` returns, already expanded.
#[rune::function]
fn temp() -> String {
    files::temp()
}

/// The value of the environment variable `name`, or `None`.
///
/// Useful for paths a script should follow rather than guess:
/// `cleaner::env("STEAM_PATH")`.
#[rune::function]
fn env(name: &str) -> Option<String> {
    files::env(name)
}

/// The CPU architecture this build runs on, e.g. `"x86_64"` or `"aarch64"`.
#[rune::function]
fn arch() -> String {
    files::arch()
}

/// The drives of the machine as `C:` style roots; empty when there are none.
#[rune::function]
fn drives() -> Vec<String> {
    files::drives()
}

/// Moves `path` to the system trash instead of deleting it. `true` on success.
///
/// A preview never moves anything, so this is `false` while [`dry_run`] is
/// true. The trash is bounded by the platform, not by this call: a file too
/// large for it fails, and the handler still has `remove_file` for that case.
#[rune::function]
fn trash(path: &str) -> bool {
    if session::dry_run() {
        return false;
    }

    cleaner::trash::send(std::path::Path::new(path)).is_ok()
}

/// Rewrites an image in a smaller format, in place, and reports the bytes saved.
///
/// Only the formats the engine itself optimizes are handled (PNG, JPEG, WebP,
/// GIF, BMP); anything else, and any image that would not get smaller, returns
/// `None`. The file is replaced atomically, so a crash cannot leave a half
/// written image behind. A preview measures the savings without rewriting the file.
#[rune::function]
fn optimize_image(path: &str) -> Option<Stats> {
    remove::optimize_image(path)
}

/// Rewrites every image under `path` that matches `pattern` (a glob) in place,
/// and reports the total files optimized and bytes saved.
///
/// Non-image files and images that would not get smaller are skipped. A preview
/// measures the savings without rewriting the files.
#[rune::function]
fn optimize_images(path: &str, pattern: &str) -> Stats {
    remove::optimize_images(path, pattern)
}

/// Formats `bytes` the way the UI shows sizes: `"1.4 MB"`.
#[rune::function]
fn human_size(bytes: u64) -> String {
    database::utils::get_file_size_string(bytes)
}

/// The first drive, or an empty string when the machine reports none.
///
/// A convenience for the common `for drive in cleaner::drives()` loop that
/// only needs the system drive.
#[rune::function]
fn drive() -> String {
    files::drive()
}

/// The Steam installation directory, or an empty string when Steam is absent.
#[rune::function]
fn steam() -> String {
    files::steam()
}

/// Joins path segments with the platform separator.
///
/// The alternative — `format!("{}/{}", a, b)` — produces a doubled or stray
/// separator on one platform and a broken path on the other.
///
/// ```rune
/// let logs = cleaner::join([cleaner::home(), "AppData", "Local", "My App", "logs"]);
/// ```
#[rune::function]
fn join(parts: Vec<String>) -> String {
    files::join(parts)
}

/// The directory containing `path`, if it has one.
#[rune::function]
fn parent(path: &str) -> Option<String> {
    files::parent(path)
}

/// The last component of `path` — the file or directory name.
#[rune::function]
fn filename(path: &str) -> Option<String> {
    files::filename(path)
}

/// Expands `{home}`, `{username}`, `{steam}` and `{drive}` the way a rule path
/// is expanded, one path per drive.
///
/// The same expansion the engine applies to a declared rule, so a handler that
/// computes a path itself gets the answer the rule would have got.
///
/// ```rune
/// for cache in cleaner::expand("{drive}/Users/{username}/AppData/Local/My App/cache") {
///     cleaner::log(cache);
/// }
/// ```
#[rune::function]
fn expand(pattern: &str) -> Vec<String> {
    files::expand(pattern)
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

/// Sends one HTTP request and returns the response, or `None` on failure.
///
/// `http_get` and `http_post` cover the common cases; this is for the rest: a
/// `PUT` to an upload endpoint, a `HEAD` to see whether a file is still there,
/// a custom header the server wants. `headers` are `"Name: value"` strings
/// and `body` is sent as-is — a JSON payload is `json::to_string(...)?`.
///
/// ```rune
/// pub async fn notify(report) {
///     let body = json::to_string(#{ "text": report.summary() })?;
///     let response = cleaner::http_request(
///         "PUT",
///         "https://example.com/status",
///         Some(body),
///         ["Authorization: Bearer token"],
///         0,
///     );
///
///     if let Some(response) = response {
///         cleaner::log(format!("server said {}", response.status));
///     }
/// }
/// ```
#[rune::function]
fn http_request(
    method: &str,
    url: &str,
    body: Option<String>,
    headers: Vec<String>,
    timeout: u64,
) -> Option<files::HttpResponse> {
    files::http_request(method, url, body, headers, timeout)
}

// --- hooks -----------------------------------------------------------------

/// Registers `handler` to run once, before a cleaning run starts deleting.
///
/// This is where a script does what has to happen before the files go: close a
/// background copy, take a backup, check that the machine is idle, or ask a
/// service whether cleaning is allowed right now. It runs synchronously, so a
/// hook that takes ten seconds delays the cleaning by ten seconds — which is
/// exactly what it is for.
///
/// It is not called for a scan or a dry run: nothing is removed, so there is
/// nothing to prepare for.
///
/// ```rune
/// pub fn main() { cleaner::on_started("prepare"); }
///
/// pub fn prepare() {
///     cleaner::log("a cleaning run is starting");
/// }
/// ```
#[rune::function]
fn on_started(handler: &str) {
    collect::hook(HookKind::Started, handler.to_owned());
}

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
    collect::hook(HookKind::Finished, handler.to_owned());
}

/// Registers `handler` to run when a cleaning run finished but left something
/// behind: a file another program still held open, a directory the user may not
/// write, an entry the registry refused.
///
/// The handler receives the report of what *was* cleaned plus a one-line
/// message saying what was not, which is what a monitoring channel wants to
/// hear about — a run that went perfectly is not worth a notification.
///
/// ```rune
/// pub fn main() { cleaner::on_error("report_problem"); }
///
/// pub async fn report_problem(report, problem) {
///     let text = format!("{} — but: {}", report.summary(), problem);
///     let body = json::to_string(#{ "text": text })?;
///     cleaner::http_post(url, body);
/// }
/// ```
#[rune::function]
fn on_error(handler: &str) {
    collect::hook(HookKind::Failed, handler.to_owned());
}

/// Registers `handler` to run once before a scan starts.
///
/// A scan — the preview a frontend shows before anything is removed — is a good
/// time to tell a script that the numbers it is about to produce are estimates.
/// The handler runs on the same thread as the scan, before any cleaner runs.
///
/// ```rune
/// pub fn main() { cleaner::on_scan("mark_estimates"); }
///
/// pub fn mark_estimates() {
///     cleaner::log("the numbers below are estimates");
/// }
/// ```
#[rune::function]
fn on_scan(handler: &str) {
    collect::hook(HookKind::Scan, handler.to_owned());
}

/// Registers `handler` to run after each program is cleaned.
///
/// The handler receives one program's report: its name, files, folders, bytes
/// and categories. It runs synchronously between programs, so keep it short.
///
/// ```rune
/// pub fn main() { cleaner::on_program_done("note_program"); }
///
/// pub fn note_program(report) {
///     cleaner::log(format!("{}: {} files", report.program, report.files));
/// }
/// ```
#[rune::function]
fn on_program_done(handler: &str) {
    collect::hook(HookKind::ProgramDone, handler.to_owned());
}

/// Registers `handler` to run once when the user stops a run.
///
/// The handler is called after the workers have stopped, with no arguments, on
/// a background thread. A script that saves progress in `on_started` can undo
/// or record it here.
///
/// ```rune
/// pub fn main() { cleaner::on_cancelled("clean_up"); }
///
/// pub fn clean_up() {
///     cleaner::log("the run was stopped");
/// }
/// ```
#[rune::function]
fn on_cancelled(handler: &str) {
    collect::hook(HookKind::Cancelled, handler.to_owned());
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
    /// Additional paths, so one rule can cover several places without the
    /// script registering the same rule three times under one name.
    extra_paths: Vec<String>,
    os: Vec<String>,
    files_to_remove: Vec<String>,
    directories_to_remove: Vec<String>,
    flags: u8,
    /// Selection rules, applied per file by the engine.
    exclude: Vec<String>,
    older_than_days: Option<u64>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    max_depth: Option<u32>,
    skip_if_running: Vec<String>,
    /// Move what is removed to the recycle bin instead of deleting it.
    to_trash: bool,
}

/// Starts a file and folder rule for `program`.
#[rune::function]
fn file_cleaner(program: &str) -> FileCleaner {
    FileCleaner {
        program: program.to_owned(),
        category: String::new(),
        sub_category: String::new(),
        class: String::from("Other"),
        path: String::new(),
        extra_paths: Vec::new(),
        os: Vec::new(),
        files_to_remove: Vec::new(),
        directories_to_remove: Vec::new(),
        flags: 0,
        exclude: Vec::new(),
        older_than_days: None,
        min_size: None,
        max_size: None,
        max_depth: None,
        skip_if_running: Vec::new(),
        to_trash: false,
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

    /// Cleans several paths with one rule.
    ///
    /// Each path becomes its own database entry, but they share the program,
    /// the category and the selection rules — the shape the database has for a
    /// program that keeps its cache in two places.
    ///
    /// ```rune
    /// cleaner::file_cleaner("My App")
    ///     .category("Cache")
    ///     .paths(["{username}/AppData/Local/My App/Cache",
    ///             "{username}/AppData/Local/My App/GPUCache"])
    ///     .remove_all_in_dir()
    ///     .register();
    /// ```
    #[rune::function(instance)]
    fn paths(&mut self, values: Vec<String>) -> Self {
        self.extra_paths = values;
        self.clone()
    }

    /// Never touches these names, even when they match the pattern.
    ///
    /// A pattern is matched against both the full path and the bare name, so
    /// `"*.lock"` protects a lock file anywhere in the tree and `"user-data"`
    /// protects a directory of that name with everything inside it.
    #[rune::function(instance)]
    fn exclude(&mut self, values: Vec<String>) -> Self {
        self.exclude = values;
        self.clone()
    }

    /// Only removes files last modified at least `days` days ago.
    ///
    /// A file whose modification time cannot be read is left alone: "older than
    /// a week" cannot be proven, and this filter exists to prevent deleting on a
    /// guess. This is the declarative form of `cleaner::older_than` — use the
    /// method when the whole rule can decide by age, the function inside a
    /// `clean` handler when it cannot.
    #[rune::function(instance)]
    fn older_than(&mut self, days: u64) -> Self {
        self.older_than_days = Some(days);
        self.clone()
    }

    /// Only removes files at least `size` large, e.g. `"10 MB"`.
    ///
    /// The size is a number plus one of `B`, `KB`, `MB`, `GB`, `TB`; a bare
    /// number means bytes. Both binary (1024) and decimal-looking sizes are
    /// read as the same unit the sizes in the app are shown in.
    #[rune::function(instance)]
    fn min_size(&mut self, size: &str) -> Self {
        match parse_size(size) {
            Some(bytes) => self.min_size = Some(bytes),
            None => collect::error(format!(
                "file cleaner {}: min_size({size:?}) is not a size",
                display_name(&self.program)
            )),
        }
        self.clone()
    }

    /// Only removes files at most `size` large, e.g. `"512 KB"`.
    #[rune::function(instance)]
    fn max_size(&mut self, size: &str) -> Self {
        match parse_size(size) {
            Some(bytes) => self.max_size = Some(bytes),
            None => collect::error(format!(
                "file cleaner {}: max_size({size:?}) is not a size",
                display_name(&self.program)
            )),
        }
        self.clone()
    }

    /// How deep the walk may go, counting the entries directly inside the path
    /// as depth 1.
    ///
    /// A rule for `node_modules` almost never means "and every nested copy of
    /// it"; a depth keeps one runaway tree from being walked forever.
    #[rune::function(instance)]
    fn max_depth(&mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self.clone()
    }

    /// Skips the whole rule while any of these programs is running.
    ///
    /// Cleaning a browser cache under a running browser mostly just makes the
    /// browser write everything back on its next tick.
    #[rune::function(instance)]
    fn skip_if_running(&mut self, programs: Vec<String>) -> Self {
        self.skip_if_running = programs;
        self.clone()
    }

    /// Moves what is removed to the recycle bin instead of deleting it.
    ///
    /// Right for a user's own documents and project caches, where being able to
    /// undo the cleaning matters; the built-in catalog deletes, because system
    /// junk is not worth a trip through the bin.
    #[rune::function(instance)]
    fn to_trash(&mut self) -> Self {
        self.to_trash = true;
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
            Ok(entries) => {
                for entry in entries {
                    collect::file(entry);
                }
            }
            Err(message) => collect::error(message),
        }
    }

    fn build(&self) -> Result<Vec<CleanerData>, String> {
        let name = display_name(&self.program);

        if self.category.trim().is_empty() {
            return Err(format!(
                "file cleaner {name}: category(...) is required - it is what the user \
                 selects to include this cleaner"
            ));
        }

        if self.path.trim().is_empty() && self.extra_paths.is_empty() {
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

        if self.older_than_days == Some(0) {
            return Err(format!(
                "file cleaner {name}: older_than(0) would match every file"
            ));
        }

        let filter = CleanerFilter {
            exclude: self
                .exclude
                .iter()
                .map(|value| Arc::<str>::from(value.as_str()))
                .collect(),
            older_than_seconds: self.older_than_days.map(|days| days.saturating_mul(86_400)),
            min_size: self.min_size,
            max_size: self.max_size,
            max_depth: self.max_depth,
            skip_if_running: self
                .skip_if_running
                .iter()
                .map(|value| Arc::<str>::from(value.as_str()))
                .collect(),
            to_trash: self.to_trash,
        };

        let mut paths = Vec::with_capacity(1 + self.extra_paths.len());

        if !self.path.trim().is_empty() {
            paths.push(self.path.clone());
        }

        paths.extend(self.extra_paths.iter().cloned());

        Ok(paths
            .into_iter()
            .map(|path| CleanerData {
                path: SharedPath::new(&path),
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
                filter: filter.clone(),
            })
            .collect())
    }
}

/// Reads `"10 MB"`, `"512KB"` or `"123"` into bytes.
///
/// The units are the ones the app prints sizes in, and both `KB` and `KiB`
/// mean 1024: a size in a rule is a threshold, not an accounting figure, and
/// accepting both spellings is friendlier than refusing one.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());

    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;

    if !number.is_finite() || number < 0.0 {
        return None;
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };

    Some((number * multiplier) as u64)
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
        category: String::new(),
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

        if self.category.trim().is_empty() {
            return Err(format!(
                "registry cleaner {name}: category(...) is required - it is what the user \
                 selects to include this cleaner"
            ));
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
    /// Additional paths for the same handler, one call per path.
    extra_paths: Vec<String>,
    os: Vec<String>,
    handler: String,
    glob: bool,
    sequential: bool,
    within: String,
}

/// Starts a script-driven cleaner for `program`.
#[rune::function]
fn custom_cleaner(program: &str) -> CustomCleanerBuilder {
    CustomCleanerBuilder {
        id: String::new(),
        program: program.to_owned(),
        category: String::new(),
        sub_category: String::new(),
        path: String::new(),
        extra_paths: Vec::new(),
        os: Vec::new(),
        handler: String::from("clean"),
        glob: false,
        sequential: false,
        within: String::new(),
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

    /// Additional paths for the same handler.
    ///
    /// The handler is called once per path, and everything it removes still
    /// folds under one program in the results. Use it instead of registering the
    /// same cleaner three times under one name.
    #[rune::function(instance)]
    fn paths(&mut self, values: Vec<String>) -> Self {
        self.extra_paths = values;
        self.clone()
    }

    /// A fence for everything the handler removes.
    ///
    /// With it set, every `cleaner::remove_*` call that resolves outside `root`
    /// is refused instead of performed — the handler still runs, but a path built
    /// by a buggy `join` cannot turn "clean my app's cache" into "clean the
    /// disk". Deletions inside the fence count and report as usual.
    ///
    /// ```rune
    /// let root = cleaner::join([cleaner::user(), "AppData", "Local", "My App"]);
    ///
    /// cleaner::custom_cleaner("My App")
    ///     .category("Cache")
    ///     .path(root)
    ///     .within(root)          // nothing outside My App can be removed
    ///     .handler("clean_mine")
    ///     .register();
    /// ```
    #[rune::function(instance)]
    fn within(&mut self, root: &str) -> Self {
        self.within = root.to_owned();
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

        if self.path.trim().is_empty() && self.extra_paths.is_empty() {
            collect::error(format!(
                "custom cleaner {}: path(...) is required",
                display_name(&self.program)
            ));
            return;
        }

        if self.category.trim().is_empty() {
            collect::error(format!(
                "custom cleaner {}: category(...) is required - it is what the user \
                 selects to include this cleaner",
                display_name(&self.program)
            ));
            return;
        }

        let base = if self.id.is_empty() {
            self.program.clone()
        } else {
            self.id.clone()
        };

        let within = if self.within.trim().is_empty() {
            None
        } else {
            Some(self.within.clone())
        };

        // One entry per path: the engine calls the handler once per entry, so a
        // handler that cleans one directory needs no loop over `paths()` of its
        // own, and the results still fold under one program.
        let paths = std::iter::once(&self.path)
            .filter(|path| !path.trim().is_empty())
            .chain(
                self.extra_paths
                    .iter()
                    .filter(|path| !path.trim().is_empty()),
            );

        for (index, path) in paths.enumerate() {
            collect::custom(CustomDefinition {
                id: if index == 0 {
                    base.clone()
                } else {
                    format!("{base}#{index}")
                },
                program: self.program.clone(),
                category: self.category.clone(),
                sub_category: self.sub_category.clone(),
                path: path.clone(),
                os: self.os.clone(),
                handler: self.handler.clone(),
                glob: self.glob,
                sequential: self.sequential,
                within: within.clone(),
            });
        }
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
    /// Files the run could not remove because something still held them open.
    ///
    /// A run that reports these is not necessarily a failure — one locked file
    /// in a cache is normal — but a script that wants to warn about it can read
    /// the number instead of parsing the one-line problem it gets in `on_error`.
    #[rune(get)]
    pub locked_files: u64,
    /// How much the locked files were worth, in bytes.
    #[rune(get)]
    pub locked_bytes: u64,
    /// Files the run could not remove because it was not allowed to.
    #[rune(get)]
    pub denied_files: u64,
    /// How much the denied files were worth, in bytes.
    #[rune(get)]
    pub denied_bytes: u64,
    /// When the run started, in seconds since the Unix epoch.
    ///
    /// `0` when the start was not recorded — a frontend that never called
    /// `cleaner::on_started`'s counterpart, which in practice means an older
    /// build.
    #[rune(get)]
    pub started_at: u64,
    /// How long the run took, in seconds. `0` when the start was not recorded.
    #[rune(get)]
    pub duration: u64,
    /// One entry per program, largest first.
    entries: Vec<ProgramReport>,
    /// One entry per category, largest first.
    categories: Vec<CategoryReport>,
    // Read by scripts through the getters below; unreachable for the compiler.
    #[allow(dead_code)]
    summary: String,
    #[allow(dead_code)]
    details: String,
}

/// What one category contributed to a finished run.
#[derive(Debug, Clone, Any)]
pub struct CategoryReport {
    /// The category's name, e.g. `"Cache"`.
    #[rune(get)]
    pub category: String,
    #[rune(get)]
    pub files: u64,
    #[rune(get)]
    pub folders: u64,
    #[rune(get)]
    pub bytes: u64,
    // Read through `size()`; unreachable for the compiler.
    #[allow(dead_code)]
    size: String,
}

impl CategoryReport {
    /// The size as a human string, e.g. `"1.2 GB"`.
    #[rune::function(instance)]
    fn size(&self) -> String {
        self.size.clone()
    }
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

    /// One entry per category, largest first — what a per-category message
    /// needs (`"Cache: 1.2 GB, Logs: 300 MB"`).
    #[rune::function(instance)]
    fn categories(&self) -> Vec<CategoryReport> {
        self.categories.clone()
    }

    /// Whether the run left anything behind that it was not allowed to remove.
    #[rune::function(instance)]
    fn incomplete(&self) -> bool {
        self.denied_files > 0 || self.locked_files > 0
    }
}

/// The totals a finished run reports, in the one shape both hooks take.
///
/// The frontends pass these straight through from the cleaning job; keeping
/// them together is what stops `notify_finished` and `notify_failed` from
/// drifting apart in the numbers they show.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunTotals {
    pub bytes: u64,
    pub files: u64,
    pub folders: u64,
    /// Files the run could not remove because something held them open.
    pub locked_files: u64,
    pub locked_bytes: u64,
    /// Files the run was not allowed to remove.
    pub denied_files: u64,
    pub denied_bytes: u64,
}

/// What one cleaned program reports to an `on_program_done` handler.
pub(crate) fn program_report(cleared: &Cleared) -> ProgramReport {
    ProgramReport {
        program: cleared.program.to_string(),
        files: cleared.removed_files,
        folders: cleared.removed_directories,
        bytes: cleared.removed_bytes,
        categories: cleared
            .affected_categories
            .iter()
            .map(|category| category.to_string())
            .collect(),
        size: database::utils::get_file_size_string(cleared.removed_bytes),
    }
}
/// Builds the report from a finished run.
pub(crate) fn report(
    totals: RunTotals,
    cleared: &[Cleared],
    started_at: u64,
    duration: u64,
) -> Report {
    let RunTotals {
        bytes,
        files,
        folders,
        locked_files,
        locked_bytes,
        denied_files,
        denied_bytes,
    } = totals;

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

    // One row per category. The per-category shares are what `Cleared` carries
    // for this purpose: splitting a program's total by its categories instead
    // would divide the bytes by guesswork.
    let mut categories: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();

    for entry in cleared {
        for total in &entry.category_totals {
            let row = categories.entry(total.category.as_str()).or_default();
            row.0 += total.files;
            row.1 += total.folders;
            row.2 += total.bytes;
        }
    }

    let mut categories: Vec<CategoryReport> = categories
        .into_iter()
        .map(|(category, (files, folders, bytes))| CategoryReport {
            category: category.to_owned(),
            files,
            folders,
            bytes,
            size: database::utils::get_file_size_string(bytes),
        })
        .collect();

    categories.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.category.cmp(&b.category))
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
        locked_files,
        locked_bytes,
        denied_files,
        denied_bytes,
        started_at,
        duration,
        entries,
        categories,
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
