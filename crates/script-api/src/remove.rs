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
use std::path::{Path, PathBuf};

use rune::Any;

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
}

/// Forgets what a previous call counted.
pub(crate) fn reset() {
    ACCUMULATED.with(|cell| cell.set(Stats::EMPTY));
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

        if fs::remove_file(&candidate).is_ok() {
            stats.files += 1;

            if !meta.file_type().is_symlink() {
                stats.bytes += meta.len();
            }
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

    if let Ok(meta) = fs::symlink_metadata(path)
        && !meta.is_dir()
        && fs::remove_file(path).is_ok()
    {
        stats.files += 1;

        if !meta.file_type().is_symlink() {
            stats.bytes += meta.len();
        }
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
            Err(_) => Stats::EMPTY,
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
                Err(_) => Stats::EMPTY,
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
        Err(_) => Stats::EMPTY,
    }
}

fn remove_directory_tree(path: &Path) -> Stats {
    let mut stats = Stats::EMPTY;

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            stats += remove_entry(&entry.path());
        }
    }

    // A directory that could not be emptied stays: the engine never reports a
    // folder it did not remove.
    if fs::remove_dir(path).is_ok() {
        stats.folders += 1;
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
