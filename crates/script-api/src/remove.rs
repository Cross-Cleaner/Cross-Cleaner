//! The removal primitives scripts clean files with.
//!
//! Scripts never touch the filesystem directly — the Rune context is built
//! without the `io`/`fs` modules — so everything they delete goes through these
//! functions. That is what makes the result honest: the counts a `clean`
//! handler reports are the counts of what these functions actually removed, not
//! numbers a script could invent.
//!
//! Symlinks are removed as links and never followed, so a script cannot be
//! tricked into deleting outside the path it was pointed at by a link someone
//! else placed there.

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rune::Any;

use crate::session;

/// What a `clean` handler removed: files, directories and bytes.
///
/// Scripts receive this from [`crate::api`]'s `cleaner::stats()` and the
/// `cleaner::remove_*` functions, read its fields and return it to the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Any)]
pub struct Stats {
    /// Files removed.
    #[rune(get)]
    pub(crate) files: u64,
    /// Directories removed, including directories removed with their contents.
    #[rune(get)]
    pub(crate) folders: u64,
    /// Bytes freed (the size of the files that were removed).
    #[rune(get)]
    pub(crate) bytes: u64,
}

impl Stats {
    pub(crate) const EMPTY: Stats = Stats {
        files: 0,
        folders: 0,
        bytes: 0,
    };
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, other: Stats) {
        self.files += other.files;
        self.folders += other.folders;
        self.bytes += other.bytes;
    }
}

/// A removal that did not happen, and why.
///
/// Without this a locked or forbidden file looks the same as a file that was
/// not there: `Stats` stays zero either way, and the user sees a cleaner that
/// silently removed less than promised. `cleaner::failures()` hands the list
/// to the handler so a script can log it, try another approach, or tell the
/// user what to close.
#[derive(Debug, Clone, Any)]
pub struct Failure {
    /// The path that could not be removed.
    #[rune(get)]
    pub(crate) path: String,
    /// Why it could not be removed, in words a log line can show.
    #[rune(get)]
    pub(crate) reason: String,
    /// Size of the file that stayed behind, in bytes.
    #[rune(get)]
    pub(crate) size: u64,
    /// Whether something is holding it open rather than the ACL refusing.
    pub(crate) holds_the_file: bool,
}

impl Failure {
    /// Whether another program holds the file open, rather than the system
    /// refusing the delete. The two need different advice: one is "close it",
    /// the other is "this needs an elevated profile".
    #[rune::function(instance)]
    fn locked(&self) -> bool {
        self.holds_the_file
    }
}
/// Most failures kept per path: enough to diagnose, bounded so a script that
/// walks a locked tree cannot grow the list without limit.
const MAX_FAILURES: usize = 1024;

thread_local! {
    /// What the `clean` handler currently running has removed.
    ///
    /// A thread-local rather than a shared counter because one handler call
    /// completes without ever awaiting, so it always runs to the end on the
    /// thread it started on.
    static ACCUMULATED: Cell<Stats> = const { Cell::new(Stats::EMPTY) };

    /// The fence a handler asked for with `within()`.
    ///
    /// `None` in the outer `Option` means no fence at all; `Some(None)` means a
    /// fence was declared but its root could not be resolved, and then every
    /// removal is refused — a fence that silently protects nothing is worse
    /// than no fence. Resolved once per cleaner run, since every path that
    /// cleaner hands the handler shares the same root.
    static WITHIN: std::cell::RefCell<Option<Option<PathBuf>>> =
        const { std::cell::RefCell::new(None) };

    /// Removals that were refused since [`reset`], newest last.
    static FAILURES: std::cell::RefCell<Vec<Failure>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Forgets what a previous call counted.
pub(crate) fn reset() {
    ACCUMULATED.with(|cell| cell.set(Stats::EMPTY));
    FAILURES.with(|cell| cell.borrow_mut().clear());
}

/// Resolves a declared fence root, keeping "declared but unresolvable" distinct
/// from "not declared".
///
/// A cleaner that runs one handler per matched path shares that root across all
/// of them, so it resolves it once with this and installs it with
/// [`set_within_resolved`] instead of canonicalising the same directory per path.
pub(crate) fn resolve_within(root: Option<&str>) -> Option<Option<PathBuf>> {
    match root {
        None => None,
        // A declared fence whose root cannot be resolved means "remove
        // nothing", not "remove anything": the caller asked for a guarantee.
        Some(root) => Some(fs::canonicalize(root).ok()),
    }
}

/// Installs a fence [`resolve_within`] has already resolved.
pub(crate) fn set_within_resolved(fence: Option<Option<PathBuf>>) {
    WITHIN.with(|cell| *cell.borrow_mut() = fence);
}

/// The removals refused since [`reset`], as the handler may inspect them.
pub(crate) fn failures() -> Vec<Failure> {
    FAILURES.with(|cell| cell.borrow().clone())
}

/// Takes the failure list and forgets it, for the host after a handler returns.
pub(crate) fn take_failures() -> Vec<Failure> {
    FAILURES.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// Records a refusal, bounded so a locked tree cannot grow the list forever.
fn note(path: &Path, reason: impl Into<String>, holds_the_file: bool) {
    let size = fs::symlink_metadata(path)
        .map(|meta| if meta.is_file() { meta.len() } else { 0 })
        .unwrap_or(0);

    let failure = Failure {
        path: path.to_string_lossy().into_owned(),
        reason: reason.into(),
        size,
        holds_the_file,
    };

    FAILURES.with(|cell| {
        let mut list = cell.borrow_mut();

        if list.len() < MAX_FAILURES {
            list.push(failure);
        }
    });
}

/// Whether opening `path` for writing succeeds, which is how a file held open
/// by another program fails: the metadata is readable, the handle is not.
fn writable(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

/// Records why `fs::remove_*` refused `path`.
fn failed(path: &Path, error: &io::Error) {
    // A file that vanished between the listing and the call is not a failure:
    // the outcome the script wanted is the outcome that happened.
    if error.kind() == io::ErrorKind::NotFound {
        return;
    }

    let holds_the_file = error.kind() == io::ErrorKind::PermissionDenied && writable(path);

    let reason = if holds_the_file {
        "still open by another program".to_owned()
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        "the system denied access".to_owned()
    } else {
        format!("the system refused: {error}")
    };

    note(path, reason, holds_the_file);
}

/// Whether `path` may be removed under the current fence.
///
/// A path is allowed when it is inside the canonical root. `..` is resolved by
/// canonicalising the parent, which also catches a path that leaves the root
/// through a chain of links: the fence is about where the bytes are, not how the
/// path is spelled.
fn permitted(path: &Path) -> bool {
    WITHIN.with(|cell| {
        let borrowed = cell.borrow();

        let Some(fence) = borrowed.as_ref() else {
            return true;
        };

        let Some(root) = fence else {
            return false;
        };

        let Ok(resolved) = fs::canonicalize(path) else {
            // The path is gone or unreadable; nothing will be removed from it
            // either, so answering "no" changes nothing and keeps the fence
            // from being bypassed by a path that does not exist yet.
            return false;
        };

        resolved.starts_with(root)
    })
}

/// Whether `path` is a directory that lives inside the fence, so its contents
/// can be walked. A reparse point is never descended into.
fn inside_fence(path: &Path) -> bool {
    WITHIN.with(|cell| match cell.borrow().as_ref() {
        None => true,
        Some(None) => false,
        Some(Some(root)) => match fs::canonicalize(path) {
            Ok(resolved) => resolved.starts_with(root),
            Err(_) => false,
        },
    })
}

/// Reports a removal the fence stopped, so a silent `Stats::EMPTY` does not read
/// as "there was nothing to remove".
fn denied(path: &Path) -> Stats {
    database::diag::warn(format!(
        "script: refused to remove {} because it is outside the within() fence",
        path.display()
    ));

    note(path, "outside the within() fence", false);

    Stats::EMPTY
}

/// What has been removed since [`reset`].
pub(crate) fn current() -> Stats {
    ACCUMULATED.with(Cell::get)
}

/// Takes the totals and forgets them, so the next call starts from zero.
pub(crate) fn take() -> Stats {
    ACCUMULATED.with(|cell| cell.replace(Stats::EMPTY))
}

/// Adds to the totals without removing anything.
///
/// A script that deletes with its own code (`std::fs`-style calls, a directory
/// walk of its own) reports what it did here, so the cleaning result stays
/// truthful instead of showing zero for work that really happened.
pub(crate) fn track(stats: Stats) {
    add(stats);
}

fn add(stats: Stats) {
    ACCUMULATED.with(|cell| {
        let mut total = cell.get();
        total += stats;
        cell.set(total);
    });
}

/// Removes everything inside `path`, keeping `path` itself.
pub(crate) fn remove_all(path: &str) -> Stats {
    let mut stats = Stats::EMPTY;

    if !permitted(Path::new(path)) {
        return denied(Path::new(path));
    }

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            stats += remove_entry(&entry.path());
        }
    }

    add(stats);
    stats
}

/// Removes the files under `path` that match `pattern` (a glob).
pub(crate) fn remove_files(path: &str, pattern: &str) -> Stats {
    let mut stats = Stats::EMPTY;

    for candidate in expand(path, pattern) {
        if !permitted(&candidate) {
            denied(&candidate);
            continue;
        }

        let Ok(meta) = fs::symlink_metadata(&candidate) else {
            continue;
        };

        // Only files: a directory matched by the pattern is left for
        // `remove_directories`, so the two cannot fight over the same entry.
        if meta.is_dir() {
            continue;
        }

        if session::dry_run() {
            stats.files += 1;

            if !meta.file_type().is_symlink() {
                stats.bytes += meta.len();
            }

            continue;
        }

        match fs::remove_file(&candidate) {
            Ok(()) => {
                stats.files += 1;

                if !meta.file_type().is_symlink() {
                    stats.bytes += meta.len();
                }
            }
            Err(error) => failed(&candidate, &error),
        }
    }

    add(stats);
    stats
}

/// Removes the directories under `path` that match `pattern` (a glob), each
/// with everything inside it.
pub(crate) fn remove_directories(path: &str, pattern: &str) -> Stats {
    let mut stats = Stats::EMPTY;

    for candidate in expand(path, pattern) {
        if !permitted(&candidate) {
            denied(&candidate);
            continue;
        }

        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                stats += remove_directory_tree(&candidate);
            }
            Err(error) => failed(&candidate, &error),
            _ => continue,
        }
    }

    add(stats);
    stats
}

/// Removes one file.
pub(crate) fn remove_file(path: &str) -> Stats {
    let mut stats = Stats::EMPTY;

    if !permitted(Path::new(path)) {
        return denied(Path::new(path));
    }

    if session::dry_run() {
        let stats = measure_entry(Path::new(path));
        add(stats);
        return stats;
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => match fs::remove_file(path) {
            Ok(()) => {
                stats.files += 1;

                if !meta.file_type().is_symlink() {
                    stats.bytes += meta.len();
                }
            }
            Err(error) => failed(Path::new(path), &error),
        },
        Err(error) => failed(Path::new(path), &error),
        _ => {}
    }

    add(stats);
    stats
}

/// Removes one directory with everything inside it.
pub(crate) fn remove_directory(path: &str) -> Stats {
    if !permitted(Path::new(path)) {
        return denied(Path::new(path));
    }

    let stats = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            remove_directory_tree(Path::new(path))
        }
        _ => Stats::EMPTY,
    };

    add(stats);
    stats
}

/// Rewrites an image in a smaller format in place, or measures the savings in
/// a dry run.
pub(crate) fn optimize_image(path: &str) -> Option<Stats> {
    let target = Path::new(path);

    if !permitted(target) {
        denied(target);
        return None;
    }

    match cleaner::image_optimizer::optimize_single_with(target, session::dry_run()) {
        Ok(saved) => {
            if saved.bytes == 0 {
                return None;
            }

            let stats = Stats {
                files: saved.files,
                folders: 0,
                bytes: saved.bytes,
            };
            add(stats);
            Some(stats)
        }
        Err(error) => {
            if error.kind() != io::ErrorKind::InvalidData {
                failed(target, &error);
            }
            None
        }
    }
}

/// Rewrites the images under `path` that match `pattern` (a glob) in place, or
/// measures the savings in a dry run.
pub(crate) fn optimize_images(path: &str, pattern: &str) -> Stats {
    let mut stats = Stats::EMPTY;

    for candidate in expand(path, pattern) {
        if !permitted(&candidate) {
            denied(&candidate);
            continue;
        }

        match cleaner::image_optimizer::optimize_single_with(&candidate, session::dry_run()) {
            Ok(saved) => {
                stats.files += saved.files;
                stats.bytes += saved.bytes;
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::InvalidData {
                    failed(&candidate, &error);
                }
            }
        }
    }

    add(stats);
    stats
}

/// Total size of the files under `path`, in bytes.
pub(crate) fn size(path: &str) -> u64 {
    fn walk(path: &Path) -> u64 {
        let Ok(entries) = fs::read_dir(path) else {
            return 0;
        };

        let mut total = 0;

        for entry in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };

            if meta.file_type().is_symlink() {
                continue;
            }

            total += if meta.is_dir() {
                walk(&entry.path())
            } else {
                meta.len()
            };
        }

        total
    }

    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => walk(Path::new(path)),
        Ok(meta) if !meta.is_dir() => meta.len(),
        _ => 0,
    }
}

/// Whether anything exists at `path`.
pub(crate) fn exists(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn remove_entry(path: &Path) -> Stats {
    if !permitted(path) {
        return denied(path);
    }

    // A scan counts what this call would remove instead of removing it, so the
    // numbers it shows are the ones a real run would report.
    if session::dry_run() {
        return measure_entry(path);
    }

    let Ok(meta) = fs::symlink_metadata(path) else {
        return Stats::EMPTY;
    };

    let kind = meta.file_type();

    if kind.is_symlink() {
        // The link itself, never its target.
        return match fs::remove_file(path) {
            Ok(()) => Stats {
                files: 1,
                folders: 0,
                bytes: 0,
            },
            Err(error) => {
                failed(path, &error);
                Stats::EMPTY
            }
        };
    }

    if kind.is_dir() {
        // A directory that is a reparse point can point anywhere; descending
        // into it would delete outside the fence. Remove the link itself, which
        // `remove_dir` does without following it, and never its contents.
        if !inside_fence(path) {
            return match fs::remove_dir(path) {
                Ok(()) => Stats {
                    files: 0,
                    folders: 1,
                    bytes: 0,
                },
                Err(error) => {
                    failed(path, &error);
                    Stats::EMPTY
                }
            };
        }

        return remove_directory_tree(path);
    }

    match fs::remove_file(path) {
        Ok(()) => Stats {
            files: 1,
            folders: 0,
            bytes: meta.len(),
        },
        Err(error) => {
            failed(path, &error);
            Stats::EMPTY
        }
    }
}

/// What removing `path` would count, without removing it.
///
/// Mirrors [`remove_entry`] exactly, including the rule that a link is counted
/// as a file and a reparse-point directory as a folder, so a preview and the
/// real run report the same numbers.
fn measure_entry(path: &Path) -> Stats {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Stats::EMPTY;
    };

    let kind = meta.file_type();

    if kind.is_symlink() {
        return Stats {
            files: 1,
            folders: 0,
            bytes: 0,
        };
    }

    if kind.is_dir() {
        if !inside_fence(path) {
            return Stats {
                files: 0,
                folders: 1,
                bytes: 0,
            };
        }

        return measure_directory_tree(path);
    }

    Stats {
        files: 1,
        folders: 0,
        bytes: meta.len(),
    }
}

/// What removing `path` and everything inside it would count.
fn measure_directory_tree(path: &Path) -> Stats {
    let mut stats = Stats::EMPTY;

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            stats += measure_entry(&entry.path());
        }
    }

    stats.folders += 1;

    stats
}

fn remove_directory_tree(path: &Path) -> Stats {
    if session::dry_run() {
        return measure_directory_tree(path);
    }

    let mut stats = Stats::EMPTY;

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            stats += remove_entry(&entry.path());
        }
    }

    // A directory that could not be emptied stays: the engine never reports a
    // folder it did not remove.
    match fs::remove_dir(path) {
        Ok(()) => stats.folders += 1,
        Err(error) => failed(path, &error),
    }

    stats
}

/// Expands `pattern` under `base` into concrete paths.
fn expand(base: &str, pattern: &str) -> Vec<PathBuf> {
    let pattern_path = Path::new(pattern);

    let joined = if pattern_path.is_absolute() {
        pattern.to_owned()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches(['/', '\\']),
            pattern.trim_start_matches(['/', '\\'])
        )
    };

    match glob::glob(&joined) {
        Ok(paths) => paths.filter_map(Result::ok).collect(),
        Err(error) => {
            database::diag::warn(format!(
                "script: invalid glob pattern \"{joined}\": {error}"
            ));
            Vec::new()
        }
    }
}
