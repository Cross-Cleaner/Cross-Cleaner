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
//!
//! Nothing is deleted by path either, and that is the second half of the same
//! guarantee. Cleaning a system temporary directory means deleting inside a
//! directory anything on the machine can write to, so a check followed by a
//! delete on the same *path* leaves a window: another process can replace a
//! directory with a junction between the two and the delete then happens
//! wherever the junction points. Here the path is resolved once, component by
//! component without following links, into a directory handle ([`Entry`]), and
//! every removal is `remove_file(name)`/`remove_dir(name)` relative to that
//! handle — a path is never resolved a second time, so there is no second answer
//! to disagree with the first.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, Metadata, OpenOptions};
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
fn note(path: &Path, reason: impl Into<String>, size: u64, holds_the_file: bool) {
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

/// One entry, addressed by the directory that holds it and its own name.
///
/// The handle was walked from the filesystem root one component at a time,
/// refusing links, so it names the directory the fence was checked against. A
/// link dropped where that directory used to be cannot redirect a removal,
/// because this type never resolves a path again: the leaf is `remove_file(name)`
/// on the handle. The leaf itself is never followed either, so a link in its
/// place is removed as a link.
struct Entry {
    dir: Dir,
    name: OsString,
}

impl Entry {
    /// Pins the entry `path` names.
    fn open(path: &Path) -> io::Result<Entry> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path names no entry"))?
            .to_os_string();

        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };

        Ok(Entry {
            dir: cleaner::open_dir_without_links(parent)?,
            name,
        })
    }

    /// What is at the entry, seen without following a link.
    fn metadata(&self) -> io::Result<Metadata> {
        metadata_at(&self.dir, &self.name)
    }

    fn dir(&self) -> &Dir {
        &self.dir
    }

    fn name(&self) -> &OsStr {
        &self.name
    }
}

/// Metadata of `name` inside `dir`, without following a link.
fn metadata_at(dir: &Dir, name: &OsStr) -> io::Result<Metadata> {
    dir.symlink_metadata(name)
}

/// Opens `name` inside `dir` as a directory.
///
/// The leaf was already inspected without following links, and cap-std resolves
/// this path one component at a time and refuses to leave `dir`, so a junction
/// planted in the window between the two cannot lead outside the handle tree.
fn open_child_dir(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
    dir.open_dir(name)
}

/// Whether opening the entry for writing succeeds inside its pinned directory,
/// which is how a file held open by another program fails: the metadata is
/// readable, the handle is not. Relative to the same handle the delete used, so
/// the answer describes the entry that was refused and not whatever the path
/// names by now.
fn writable(dir: &Dir, name: &OsStr) -> bool {
    let mut options = OpenOptions::new();
    options.write(true);
    options.follow(FollowSymlinks::No);

    dir.open_with(name, &options).is_ok()
}

/// Records why a removal relative to a handle was refused.
fn failed_at(dir: &Dir, name: &OsStr, path: &Path, error: &io::Error) {
    // A file that vanished between the listing and the call is not a failure:
    // the outcome the script wanted is the outcome that happened.
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        return;
    }

    let holds_the_file = error.kind() == io::ErrorKind::PermissionDenied && writable(dir, name);

    let reason = if holds_the_file {
        "still open by another program".to_owned()
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        "the system denied access".to_owned()
    } else {
        format!("the system refused: {error}")
    };

    let size = metadata_at(dir, name)
        .map(|meta| if meta.is_file() { meta.len() } else { 0 })
        .unwrap_or(0);

    note(path, reason, size, holds_the_file);
}

/// Records why a path-based operation was refused.
///
/// Only the image optimizer still works by path; everything that deletes goes
/// through [`failed_at`].
fn failed_path(path: &Path, error: &io::Error) {
    if error.kind() == io::ErrorKind::NotFound {
        return;
    }

    let holds_the_file = error.kind() == io::ErrorKind::PermissionDenied
        && fs::OpenOptions::new().write(true).open(path).is_ok();

    let reason = if holds_the_file {
        "still open by another program".to_owned()
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        "the system denied access".to_owned()
    } else {
        format!("the system refused: {error}")
    };

    let size = fs::symlink_metadata(path)
        .map(|meta| if meta.is_file() { meta.len() } else { 0 })
        .unwrap_or(0);

    note(path, reason, size, holds_the_file);
}

/// Records a path that could not even be pinned to a directory handle.
fn unresolved(path: &Path, error: &io::Error) {
    // Nothing to report when the entry is simply gone.
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        return;
    }

    note(path, format!("the system refused: {error}"), 0, false);
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

/// Reports a removal the fence stopped, so a silent `Stats::EMPTY` does not read
/// as "there was nothing to remove".
fn denied(path: &Path) -> Stats {
    database::diag::warn(format!(
        "script: refused to remove {} because it is outside the within() fence",
        path.display()
    ));

    note(path, "outside the within() fence", 0, false);

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
    let target = Path::new(path);

    if !permitted(target) {
        return denied(target);
    }

    let dir = match cleaner::open_dir_without_links(target) {
        Ok(dir) => dir,
        Err(error) => {
            unresolved(target, &error);
            return Stats::EMPTY;
        }
    };

    let stats = if session::dry_run() {
        measure_children(&dir)
    } else {
        remove_children(&dir, target)
    };

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

        let entry = match Entry::open(&candidate) {
            Ok(entry) => entry,
            Err(error) => {
                unresolved(&candidate, &error);
                continue;
            }
        };

        let Ok(meta) = entry.metadata() else {
            continue;
        };

        // Only files: a directory matched by the pattern is left for
        // `remove_directories`, so the two cannot fight over the same entry.
        if meta.is_dir() {
            continue;
        }

        let counted = Stats {
            files: 1,
            folders: 0,
            bytes: if meta.file_type().is_symlink() {
                0
            } else {
                meta.len()
            },
        };

        if session::dry_run() {
            stats += counted;
            continue;
        }

        match entry.dir().remove_file(entry.name()) {
            Ok(()) => stats += counted,
            Err(error) => failed_at(entry.dir(), entry.name(), &candidate, &error),
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

        let entry = match Entry::open(&candidate) {
            Ok(entry) => entry,
            Err(error) => {
                unresolved(&candidate, &error);
                continue;
            }
        };

        match entry.metadata() {
            // A link that looks like a directory is left alone: `remove_files`
            // is where a link is removed, and descending is never an option.
            Ok(meta) if meta.is_dir() => {
                stats += remove_tree(entry.dir(), entry.name(), &candidate)
            }
            Ok(_) => {}
            Err(error) => failed_at(entry.dir(), entry.name(), &candidate, &error),
        }
    }

    add(stats);
    stats
}

/// Removes one file.
pub(crate) fn remove_file(path: &str) -> Stats {
    let target = Path::new(path);

    if !permitted(target) {
        return denied(target);
    }

    let entry = match Entry::open(target) {
        Ok(entry) => entry,
        Err(error) => {
            unresolved(target, &error);
            return Stats::EMPTY;
        }
    };

    let meta = match entry.metadata() {
        Ok(meta) => meta,
        Err(error) => {
            failed_at(entry.dir(), entry.name(), target, &error);
            return Stats::EMPTY;
        }
    };

    // A directory is `remove_directory`'s business: this primitive removes one
    // entry and never a tree by accident.
    if meta.is_dir() {
        return Stats::EMPTY;
    }

    let stats = if session::dry_run() {
        Stats {
            files: 1,
            folders: 0,
            bytes: if meta.file_type().is_symlink() {
                0
            } else {
                meta.len()
            },
        }
    } else {
        match entry.dir().remove_file(entry.name()) {
            Ok(()) => Stats {
                files: 1,
                folders: 0,
                bytes: if meta.file_type().is_symlink() {
                    0
                } else {
                    meta.len()
                },
            },
            Err(error) => {
                failed_at(entry.dir(), entry.name(), target, &error);
                Stats::EMPTY
            }
        }
    };

    add(stats);
    stats
}

/// Removes one directory with everything inside it.
pub(crate) fn remove_directory(path: &str) -> Stats {
    let target = Path::new(path);

    if !permitted(target) {
        return denied(target);
    }

    let entry = match Entry::open(target) {
        Ok(entry) => entry,
        Err(error) => {
            unresolved(target, &error);
            return Stats::EMPTY;
        }
    };

    let stats = match entry.metadata() {
        Ok(meta) if meta.is_dir() => remove_tree(entry.dir(), entry.name(), target),
        Ok(_) => Stats::EMPTY,
        Err(error) => {
            failed_at(entry.dir(), entry.name(), target, &error);
            Stats::EMPTY
        }
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
                failed_path(target, &error);
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
                    failed_path(&candidate, &error);
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

/// Removes every entry inside `dir`.
fn remove_children(dir: &Dir, path: &Path) -> Stats {
    let mut stats = Stats::EMPTY;

    if let Ok(entries) = dir.entries() {
        for entry in entries.flatten() {
            let name = entry.file_name();
            stats += remove_entry(dir, &name, &path.join(&name));
        }
    }

    stats
}

/// Removes one entry inside `dir`, or counts what removing it would free.
///
/// `path` is the same entry spelled out, for the failure log and the nested
/// joins only; nothing here resolves it.
fn remove_entry(dir: &Dir, name: &OsStr, path: &Path) -> Stats {
    // A scan counts what this call would remove instead of removing it, so the
    // numbers it shows are the ones a real run would report.
    if session::dry_run() {
        return measure_entry(dir, name);
    }

    let Ok(meta) = metadata_at(dir, name) else {
        return Stats::EMPTY;
    };

    let kind = meta.file_type();

    if kind.is_symlink() {
        // The link itself, never its target.
        return match dir.remove_file(name) {
            Ok(()) => Stats {
                files: 1,
                folders: 0,
                bytes: 0,
            },
            Err(error) => {
                failed_at(dir, name, path, &error);
                Stats::EMPTY
            }
        };
    }

    if kind.is_dir() {
        return remove_tree(dir, name, path);
    }

    match dir.remove_file(name) {
        Ok(()) => Stats {
            files: 1,
            folders: 0,
            bytes: meta.len(),
        },
        Err(error) => {
            failed_at(dir, name, path, &error);
            Stats::EMPTY
        }
    }
}

/// What removing `name` inside `dir` would count, without removing it.
///
/// Mirrors [`remove_entry`] exactly, including the rule that a link is counted
/// as a file and a directory that cannot be opened as a folder, so a preview and
/// the real run report the same numbers.
fn measure_entry(dir: &Dir, name: &OsStr) -> Stats {
    let Ok(meta) = metadata_at(dir, name) else {
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
        return match open_child_dir(dir, name) {
            Ok(child) => {
                let mut stats = measure_children(&child);
                stats.folders += 1;
                stats
            }
            // A directory that is a reparse point is counted as the folder it
            // looks like: the run removes the entry itself, and a preview that
            // disagreed with the run would be worse than one that is optimistic.
            Err(_) => Stats {
                files: 0,
                folders: 1,
                bytes: 0,
            },
        };
    }

    Stats {
        files: 1,
        folders: 0,
        bytes: meta.len(),
    }
}

/// What removing every entry inside `dir` would count.
fn measure_children(dir: &Dir) -> Stats {
    let mut stats = Stats::EMPTY;

    if let Ok(entries) = dir.entries() {
        for entry in entries.flatten() {
            stats += measure_entry(dir, &entry.file_name());
        }
    }

    stats
}

/// Removes the directory `name` inside `dir`, everything in it, and then the
/// directory itself through the handle that holds it.
fn remove_tree(dir: &Dir, name: &OsStr, path: &Path) -> Stats {
    if session::dry_run() {
        return measure_entry(dir, name);
    }

    let mut stats = Stats::EMPTY;

    match open_child_dir(dir, name) {
        Ok(child) => {
            stats += remove_children(&child, path);

            // Closed before the directory is removed: a cap-std handle is
            // normally opened without delete sharing, so a live handle on the
            // directory would make its own removal fail on Windows.
            drop(child);
        }
        Err(error) => {
            // Not a directory after all — a reparse point is the usual reason.
            // Remove the entry itself, never what it points at: descending here
            // is exactly the mistake this module is built to avoid.
            return match metadata_at(dir, name) {
                Ok(meta) if meta.file_type().is_symlink() => match dir.remove_file(name) {
                    Ok(()) => Stats {
                        files: 1,
                        folders: 0,
                        bytes: 0,
                    },
                    Err(remove_error) => {
                        failed_at(dir, name, path, &remove_error);
                        Stats::EMPTY
                    }
                },
                Ok(_) => remove_dir_only(dir, name, path),
                Err(_) => {
                    failed_at(dir, name, path, &error);
                    Stats::EMPTY
                }
            };
        }
    }
    stats += remove_dir_only(dir, name, path);

    stats
}

/// Removes one directory entry, counting it as a folder only if it went.
fn remove_dir_only(dir: &Dir, name: &OsStr, path: &Path) -> Stats {
    match dir.remove_dir(name) {
        Ok(()) => Stats {
            files: 0,
            folders: 1,
            bytes: 0,
        },
        Err(error) => {
            failed_at(dir, name, path, &error);
            Stats::EMPTY
        }
    }
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
