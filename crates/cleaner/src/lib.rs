use database::diag;
use database::structures::{
    CleanerData, CleanerFilter, CleanerFlags, CleanerResult, ClearedPath, SharedPath,
};
use futures::stream::{self, StreamExt};
use glob::glob;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::io;
use tokio::sync::Semaphore;

pub mod custom_cleaners;
pub mod deletable;
pub mod image_optimizer;
pub mod processes;
pub mod trash;

// INFO: Re-export so macro_rules! ($crate::database::...) resolves in any consumer crate
pub use database;

// Rejects "..", ".", absolute paths, Windows prefixes (C:\, \\?\) and empty names.
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    let rel = Path::new(name);
    let mut out = PathBuf::new();
    let mut any = false;
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                out.push(s);
                any = true;
            }
            _ => return None,
        }
    }
    if any { Some(out) } else { None }
}

// PERF: Global cap on concurrently-running blocking filesystem operations across
// all cleaners. Bounds the tokio blocking pool (threads + per-thread allocator
// state) instead of letting every cleaner spawn its own pool of tasks.
static BLOCKING: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(32)));

// macOS uses /var as a system link. Start at the first path component so those
// paths still work, then reject links below it while walking directory handles.
///
/// Public because the script API removes through the same rule: it keeps the
/// returned handle and does every removal relative to it, so a link swapped in
/// after the path was checked cannot redirect the operation.
pub fn open_dir_without_links(path: &Path) -> io::Result<cap_std::fs::Dir> {
    let mut components = path.components().peekable();
    let mut anchor = PathBuf::new();
    if path.is_absolute() {
        while matches!(
            components.peek(),
            Some(Component::Prefix(_) | Component::RootDir)
        ) {
            anchor.push(components.next().unwrap().as_os_str());
        }
        if let Some(Component::Normal(top_level)) = components.peek() {
            anchor.push(top_level);
            components.next();
        }
    } else {
        anchor.push(".");
    }

    let mut dir = cap_std::fs::Dir::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
    for component in components {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let meta = dir.symlink_metadata(name)?;
                if meta.is_symlink() || !meta.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "linked or non-directory path component",
                    ));
                }
                dir = dir.open_dir(name)?;
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe path")),
        }
    }
    Ok(dir)
}

/// Whether `path` is a symlink or a junction, i.e. a reparse point that
/// resolves somewhere other than where it sits.
///
/// Uses `symlink_metadata`, so the link itself is inspected and not whatever it
/// points at — the same reason the walk opens directories through a handle
/// rather than by path.
fn is_linked(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
}

/// Whether a walk deletes what it finds or only measures it.
///
/// Threaded through the whole removal path rather than checked once at the top,
/// because a dry run that re-derives what a clean would do is only trustworthy
/// if it is the *same* walk: the glob expansion, the flag checks, the
/// `files_to_remove` / `directories_to_remove` lists and the order they run in
/// are all things a second implementation would eventually get wrong, and a
/// dry run that disagrees with the run is worse than no dry run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Remove what is matched. The normal run.
    Remove,
    /// Measure what is matched and leave it in place.
    Measure,
}

impl Mode {
    /// True when the walk is allowed to delete.
    fn removes(self) -> bool {
        self == Mode::Remove
    }
}

/// A [`CleanerFilter`] compiled for the walk: the exclude globs parsed once, and
/// "now" fixed for the whole entry so two files a second apart are judged
/// against the same clock.
///
/// The filter is built per database entry, not per matched path: a glob that
/// matches ten thousand files would otherwise compile the same pattern ten
/// thousand times.
struct Filter {
    rules: CleanerFilter,
    exclude: Vec<glob::Pattern>,
    now: u64,
}

impl Filter {
    fn new(rules: &CleanerFilter) -> Self {
        // An invalid pattern is dropped rather than failing the entry: a typo in
        // one exclusion must not disable the cleaning it belongs to, and
        // dropping a pattern only ever makes the entry remove *more* than asked,
        // which is why it is reported.
        let exclude = rules
            .exclude
            .iter()
            .filter_map(|pattern| match glob::Pattern::new(pattern) {
                Ok(compiled) => Some(compiled),
                Err(e) => {
                    diag::warn(format!(
                        "cleaner: ignoring exclude pattern {pattern:?}: {e}"
                    ));
                    None
                }
            })
            .collect();

        Self {
            rules: rules.clone(),
            exclude,
            now: trash::unix_now(),
        }
    }

    /// Whether the entry removes everything it matches, i.e. whether the walk can
    /// take the plain delete-everything path.
    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `path` is named by an exclusion.
    ///
    /// Both the full path and the bare name are tried, so `cache` protects every
    /// directory of that name while `*/cache/*` protects a position.
    fn excluded(&self, path: &Path) -> bool {
        if self.exclude.is_empty() {
            return false;
        }

        let options = glob::MatchOptions {
            // Windows names are case-insensitive, so a pattern written `Temp`
            // has to protect `temp` too; elsewhere the case is meaningful.
            case_sensitive: !cfg!(windows),
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let full = path.to_string_lossy();
        let name = path.file_name().map(|n| n.to_string_lossy());

        self.exclude.iter().any(|pattern| {
            pattern.matches_with(&full, options)
                || name
                    .as_deref()
                    .is_some_and(|name| pattern.matches_with(name, options))
        })
    }

    /// Whether a file of `size` bytes, last modified at `modified`, may go.
    fn accepts(&self, size: u64, modified: Option<u64>) -> bool {
        self.rules.accepts(size, modified, self.now)
    }
}

/// A file's modification time, in seconds since the Unix epoch, when the
/// platform reports one.
fn modified_secs(meta: &cap_std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Removes (or measures) one named file inside an open directory handle.
///
/// Returns the file's size together with whether it can actually be reached by
/// a real run: in [`Mode::Measure`] the answer is the probe's verdict, which is
/// the difference between bytes a run frees and bytes it does not. The reason
/// travels with the size rather than being flattened to a bool, because "another
/// program holds it" and "you lack the rights" call for different advice.
fn remove_file_in_dir(
    dir: &cap_std::fs::Dir,
    name: &Path,
    path: &Path,
    mode: Mode,
    filter: &Filter,
) -> io::Result<(u64, deletable::Deletable)> {
    let meta = dir.symlink_metadata(name)?;
    if meta.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symlink not supported",
        ));
    }
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let len = meta.len();
    if mode.removes() {
        if filter.rules.to_trash {
            // The item leaves the way it came in: by path, through the shell or
            // the trash directory. The handle inside `dir` is not needed and is
            // deliberately not used — a bin takes names, not descriptors.
            trash::send(path)?;
        } else {
            dir.remove_file(name)?;
        }
        return Ok((len, deletable::Deletable::Yes));
    }
    // INFO: probed after the stat and before the caller adds it to the totals.
    // A file can be readable and still undeletable — that is the whole point of
    // the probe, and a scan that reported it as free would promise bytes the
    // run is going to fail on.
    Ok((len, deletable::is_deletable(path)))
}

fn remove_file_sync(
    path: &Path,
    mode: Mode,
    filter: &Filter,
) -> io::Result<(u64, deletable::Deletable)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name in path"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = open_dir_without_links(parent)?;
    remove_file_in_dir(&dir, Path::new(name), path, mode, filter)
}

/// What one removal took: the totals the run reports, plus one entry per item
/// that actually went.
///
/// The two are kept apart because they answer different questions. "3 files, 3
/// directories" says how much was deleted; only `entries` says *which*, and a
/// report that showed the totals without the entries is what made a three-file
/// deletion look like a single mysterious path.
#[derive(Default)]
struct Removed {
    files: u64,
    folders: u64,
    bytes: u64,
    /// One entry per removed item, in walk order. Capped by
    /// [`MAX_REMOVED_ENTRIES`]: a cache directory can hold six figures of files,
    /// and the results page shows a scrolling list.
    entries: Vec<ClearedPath>,
    /// Items that went but are not in `entries`, because the cap was reached.
    omitted: usize,
    /// Files a scan found that something holds open, with their sizes. Empty in
    /// every run that removes, and never folded into the counters above: these
    /// are bytes the run will *not* free.
    locked: Vec<ClearedPath>,
    /// How many files `locked` accounts for, including the ones past the cap.
    locked_count: u64,
    /// The bytes those files hold, counted in full even past the cap.
    locked_bytes: u64,
    /// Files a scan found that the ACL refuses to delete. Kept apart from
    /// `locked` because the fix is different — closing an application against a
    /// permission problem wastes the user's time and sends them down the wrong
    /// path entirely.
    denied: Vec<ClearedPath>,
    denied_count: u64,
    denied_bytes: u64,
}

/// Deleted paths kept for one cleaner. Past this the numbers still count
/// everything, but the list stops growing — the appcore that assembles the
/// results page caps it again per program and says how many it left out, so
/// nothing is silently lost, only summarised twice.
const MAX_REMOVED_ENTRIES: usize = 1000;

impl Removed {
    fn file(path: &Path, bytes: u64) -> Self {
        Self {
            files: 1,
            bytes,
            entries: vec![ClearedPath {
                path: SharedPath::new(&path.to_string_lossy()),
                removed_bytes: bytes,
                removed_files: 1,
                removed_directories: 0,
            }],
            ..Self::default()
        }
    }

    fn folder(path: &Path) -> Self {
        Self {
            folders: 1,
            entries: vec![ClearedPath {
                path: SharedPath::new(&path.to_string_lossy()),
                removed_bytes: 0,
                removed_files: 0,
                removed_directories: 1,
            }],
            ..Self::default()
        }
    }

    /// Folds a child removal into this one, listing the child's own items.
    fn merge(&mut self, child: Removed) {
        self.files += child.files;
        self.folders += child.folders;
        self.bytes += child.bytes;
        self.extend(child.entries);
        self.omitted += child.omitted;
        self.locked_count += child.locked_count;
        self.locked_bytes += child.locked_bytes;
        self.locked.extend(child.locked);
        self.denied_count += child.denied_count;
        self.denied_bytes += child.denied_bytes;
        self.denied.extend(child.denied);
    }

    /// Folds in an item a scan found cannot be removed, routing it by reason.
    ///
    /// Separate from [`Self::merge`] on purpose: the counters must not move,
    /// because the bytes are not freeable. Counting them in `bytes` and then
    /// subtracting later is the same arithmetic with two chances to forget the
    /// subtraction.
    fn merge_blocked(&mut self, blocked: Removed, reason: deletable::Deletable) {
        match reason {
            deletable::Deletable::Denied => {
                self.denied_count += blocked.files + blocked.folders;
                self.denied_bytes += blocked.bytes;
                self.denied.extend(blocked.entries);
            }
            // `Locked` and `Unknown` are folded together deliberately: an
            // unanswered probe is not something the user can act on, and it is
            // far closer to "something else holds it" than to "your rights are
            // wrong", which would send them off to run elevated for nothing.
            _ => {
                self.locked_count += blocked.files + blocked.folders;
                self.locked_bytes += blocked.bytes;
                self.locked.extend(blocked.entries);
            }
        }
    }

    /// Adds entries, counting whatever does not fit instead of dropping it.
    fn extend(&mut self, entries: Vec<ClearedPath>) {
        for entry in entries {
            if self.entries.len() < MAX_REMOVED_ENTRIES {
                self.entries.push(entry);
            } else {
                self.omitted += 1;
            }
        }
    }
}

fn remove_dir_in_dir(
    parent_dir: &cap_std::fs::Dir,
    name: &Path,
    path: &Path,
    mode: Mode,
    filter: &Filter,
) -> io::Result<Removed> {
    let meta = parent_dir.symlink_metadata(name)?;
    if meta.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "symlink not supported",
        ));
    }

    // A whole directory going to the bin is one move, so the walk is only there
    // to say what that move is worth — and it has to run in Measure even during
    // a real run, because the items are not being deleted one by one.
    if filter.rules.to_trash && filter.is_empty() {
        let dir = parent_dir.open_dir(name)?;
        let mut removed = remove_dir_recursive(&dir, path, Mode::Measure, filter)?;
        drop(dir);

        if mode.removes() {
            trash::send(path)?;
        }

        removed.merge(Removed::folder(path));
        return Ok(removed);
    }

    let dir = parent_dir.open_dir(name)?;
    // Keep the handle open only while walking; on Windows a directory cannot
    // be removed while any handle to it is open (cap-std omits FILE_SHARE_DELETE).
    let walk = if filter.is_empty() {
        FilteredWalk {
            removed: remove_dir_recursive(&dir, path, mode, filter)?,
            kept: false,
        }
    } else {
        remove_dir_filtered(&dir, path, mode, filter, 0)?
    };
    drop(dir);

    let mut removed = walk.removed;

    // A walk that kept something cannot remove the directory around it, and must
    // not count it: the entry named a directory to remove and the filter said
    // "not all of it", which leaves the directory where it is.
    if !walk.kept {
        if mode.removes() {
            parent_dir.remove_dir(name)?; // root is now empty
        }

        // The directory counts itself in either mode: it is the one item the
        // caller named, and a measured run has to report the same shape as a
        // real one.
        removed.merge(Removed::folder(path));
    }

    Ok(removed)
}

fn remove_dir_sync(root: PathBuf, mode: Mode, filter: &Filter) -> io::Result<Removed> {
    let name = root.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "cannot remove filesystem root")
    })?;
    let parent = root.parent().unwrap_or_else(|| Path::new("."));
    let parent_dir = open_dir_without_links(parent)?;
    remove_dir_in_dir(&parent_dir, Path::new(name), &root, mode, filter)
}

// INFO: Depth-first deletion relative to open handles. Entry types come from
// the handle (lstat semantics, never follows links). Symlinks and Windows
// junctions are removed as links; their targets are never touched.
//
// INFO: `path` is the directory being walked, so every item removed below it can
// be named by its full path rather than by a bare entry name.
fn remove_dir_recursive(
    dir: &cap_std::fs::Dir,
    path: &Path,
    mode: Mode,
    filter: &Filter,
) -> io::Result<Removed> {
    let mut removed = Removed::default();

    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let entry_path = path.join(&name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            // Never follow links. The removal syscall differs per platform:
            // - Windows: directory reparse points (junctions, dir symlinks) have
            //   FILE_ATTRIBUTE_DIRECTORY but FileType::is_dir() is false for them,
            //   so they must go through RemoveDirectory (removes the link itself).
            // - Unix: unlink removes any symlink, including symlink-to-dir.
            //
            // In Measure mode the link is counted the same way and left alone:
            // a symlink still frees the bytes of the link itself, and never the
            // bytes of whatever it points at.
            if mode.removes() {
                #[cfg(windows)]
                {
                    // Junctions and dir symlinks are reparse points; DeleteFile
                    // rejects them, RemoveDirectory removes the link itself.
                    // Regular file symlinks go through DeleteFile.
                    if dir.remove_file(&name).is_ok() {
                        removed.merge(Removed::file(&entry_path, 0));
                    } else {
                        dir.remove_dir(&name)?;
                        removed.merge(Removed::folder(&entry_path));
                    }
                }
                #[cfg(not(windows))]
                {
                    dir.remove_file(&name)?;
                    removed.merge(Removed::file(&entry_path, 0));
                }
            } else {
                removed.merge(Removed::file(&entry_path, 0));
            }
        } else if ft.is_dir() {
            let sub = dir.open_dir(&name)?;
            let child = remove_dir_recursive(&sub, &entry_path, mode, filter)?;
            drop(sub); // release handle before removing (Windows FILE_SHARE_DELETE)
            removed.merge(child);
            if mode.removes() {
                dir.remove_dir(&name)?; // sub is now empty
                removed.merge(Removed::folder(&entry_path));
            } else {
                // The contents above are gone or counted, so what is left to
                // ask about is whether this user may delete the directory
                // itself. Counting it without asking promised a directory the
                // run would leave behind.
                let reason = deletable::is_deletable(&entry_path);
                if reason.is_yes() {
                    removed.merge(Removed::folder(&entry_path));
                } else {
                    removed.merge_blocked(Removed::folder(&entry_path), reason);
                }
            }
        } else {
            let bytes = entry.metadata()?.len();
            if mode.removes() {
                if filter.rules.to_trash {
                    trash::send(&entry_path)?;
                } else {
                    // A locked file fails here, and the whole walk of this
                    // directory stops with it: the entry that wanted the directory
                    // removed cannot remove a non-empty one. That is the real
                    // behaviour, so the scan has to predict it rather than report a
                    // size the run will not deliver.
                    dir.remove_file(&name)?;
                }
                removed.merge(Removed::file(&entry_path, bytes));
            } else {
                let reason = deletable::is_deletable(&entry_path);
                if reason.is_yes() {
                    removed.merge(Removed::file(&entry_path, bytes));
                } else {
                    // Counted as reachable minus nothing: the bytes are kept out
                    // of the run's totals below, and named here so the user can
                    // see what is holding them.
                    removed.merge_blocked(Removed::file(&entry_path, bytes), reason);
                }
            }
        }
    }
    Ok(removed)
}

/// What a filtered walk of one directory found.
struct FilteredWalk {
    removed: Removed,
    /// Whether something under it was deliberately left in place. A directory
    /// with anything kept cannot be removed, so this is what stops the caller
    /// from trying and from counting it.
    kept: bool,
}

/// Depth-first removal of what `filter` accepts, leaving the rest.
///
/// This is the counterpart of [`remove_dir_recursive`] for entries that do not
/// remove everything they match. The differences are all deliberate: excluded
/// names are skipped rather than deleted, `max_depth` stops the descent instead
/// of the walk, and a directory is only removed once its contents are gone — so
/// a filter that keeps a single file also keeps the whole tree around it.
fn remove_dir_filtered(
    dir: &cap_std::fs::Dir,
    path: &Path,
    mode: Mode,
    filter: &Filter,
    depth: u32,
) -> io::Result<FilteredWalk> {
    let mut walk = FilteredWalk {
        removed: Removed::default(),
        kept: false,
    };

    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let entry_path = path.join(&name);

        if filter.excluded(&entry_path) {
            walk.kept = true;
            continue;
        }

        let ft = entry.file_type()?;
        let child_depth = depth + 1;
        let too_deep = filter.rules.max_depth.is_some_and(|max| child_depth > max);

        if too_deep {
            // Past the limit nothing is touched, not even a link: the point of
            // the limit is to leave the subtree exactly as it is.
            walk.kept = true;
            continue;
        }

        if ft.is_symlink() {
            // A link is not data, only a pointer: it goes whenever it is in
            // reach, so the tree does not fill up with dead links. It is never
            // followed — see the comment in [`remove_dir_recursive`].
            if mode.removes() {
                #[cfg(windows)]
                {
                    if dir.remove_file(&name).is_err() {
                        dir.remove_dir(&name)?;
                        walk.removed.merge(Removed::folder(&entry_path));
                        continue;
                    }
                }
                #[cfg(not(windows))]
                {
                    dir.remove_file(&name)?;
                }

                walk.removed.merge(Removed::file(&entry_path, 0));
            } else {
                walk.removed.merge(Removed::file(&entry_path, 0));
            }
        } else if ft.is_dir() {
            let sub = dir.open_dir(&name)?;
            let child = remove_dir_filtered(&sub, &entry_path, mode, filter, child_depth)?;
            drop(sub);

            walk.kept |= child.kept;
            walk.removed.merge(child.removed);

            if child.kept {
                continue;
            }

            if mode.removes() {
                // Best effort: the directory can still refuse (a file appeared
                // in it between the walk and the removal), and one stubborn
                // empty directory is not a reason to abandon the rest.
                if dir.remove_dir(&name).is_ok() {
                    walk.removed.merge(Removed::folder(&entry_path));
                } else {
                    walk.kept = true;
                }
            } else {
                let reason = deletable::is_deletable(&entry_path);
                if reason.is_yes() {
                    walk.removed.merge(Removed::folder(&entry_path));
                } else {
                    walk.removed
                        .merge_blocked(Removed::folder(&entry_path), reason);
                }
            }
        } else {
            let meta = entry.metadata()?;

            if !filter.accepts(meta.len(), modified_secs(&meta)) {
                walk.kept = true;
                continue;
            }

            if mode.removes() {
                if filter.rules.to_trash {
                    trash::send(&entry_path)?;
                } else {
                    dir.remove_file(&name)?;
                }
                walk.removed.merge(Removed::file(&entry_path, meta.len()));
            } else {
                let reason = deletable::is_deletable(&entry_path);
                if reason.is_yes() {
                    walk.removed.merge(Removed::file(&entry_path, meta.len()));
                } else {
                    let item = Removed::file(&entry_path, meta.len());
                    walk.removed.merge_blocked(item, reason);
                }
            }
        }
    }

    Ok(walk)
}

#[derive(Default)]
struct PathStats {
    files: u64,
    folders: u64,
    bytes: u64,
    working: bool,
    /// Every item removed for this matched path, named.
    ///
    /// The counters above are the run's totals; this is what the results page
    /// lists. Keeping them apart is the point: a database entry that names
    /// `files_to_remove: ["a.tmp"]` has deleted `…\a.tmp`, not the directory it
    /// was found in, and a report that says otherwise cannot be checked.
    removed: Vec<ClearedPath>,
    /// Items removed but not listed, because [`MAX_REMOVED_ENTRIES`] was reached.
    omitted: usize,
    /// Files a scan found held open by another process. Counted apart from
    /// `files` / `bytes` so a scan never promises bytes the run cannot free.
    locked: Vec<ClearedPath>,
    locked_count: u64,
    locked_bytes: u64,
    /// Items a scan found the ACL refuses to delete. Kept apart from `locked`
    /// because the remedy is different: elevation, not closing an application.
    denied: Vec<ClearedPath>,
    denied_count: u64,
    denied_bytes: u64,
}

impl PathStats {
    /// Folds in an item a scan found cannot be removed, routed by reason.
    ///
    /// Separate from [`Self::add`] because the counters must not move: the bytes
    /// are visible and real, but the run will not free them, and a dry run that
    /// counted them would promise space the user does not get back.
    fn merge_blocked(&mut self, blocked: Removed, reason: deletable::Deletable) {
        let (count, bytes, list) = match reason {
            deletable::Deletable::Denied => (
                &mut self.denied_count,
                &mut self.denied_bytes,
                &mut self.denied,
            ),
            // `Locked` and `Unknown` share a bucket: an unanswered probe is not
            // actionable, and it is much closer to "something else holds it"
            // than to "your rights are wrong".
            _ => (
                &mut self.locked_count,
                &mut self.locked_bytes,
                &mut self.locked,
            ),
        };
        *count += blocked.files + blocked.folders;
        *bytes += blocked.bytes;
        for entry in blocked.entries {
            if list.len() < MAX_REMOVED_ENTRIES {
                list.push(entry);
            }
        }
        // A blocked item still proves the path exists, so the entry counts as
        // touched. Without this a directory holding nothing but locked files
        // would read as "nothing matched", which is the opposite of the truth.
        self.working = true;
    }

    /// Folds one removal in: the totals, and every item it removed by name.
    fn add(&mut self, removed: Removed) {
        self.files += removed.files;
        self.folders += removed.folders;
        self.bytes += removed.bytes;
        self.working = true;
        self.omitted += removed.omitted;
        // INFO: the locked files are counted even when the path counted nothing
        // else. A scan that found only locked files still found something, and
        // `working` is what stops the caller from reporting that as "nothing
        // matched" — which would be the exact opposite of the truth.
        self.locked_count += removed.locked_count;
        self.locked_bytes += removed.locked_bytes;
        for entry in removed.locked {
            if self.locked.len() < MAX_REMOVED_ENTRIES {
                self.locked.push(entry);
            }
        }
        self.denied_count += removed.denied_count;
        self.denied_bytes += removed.denied_bytes;
        for entry in removed.denied {
            if self.denied.len() < MAX_REMOVED_ENTRIES {
                self.denied.push(entry);
            }
        }
        for entry in removed.entries {
            if self.removed.len() < MAX_REMOVED_ENTRIES {
                self.removed.push(entry);
            } else {
                self.omitted += 1;
            }
        }
    }
}

// INFO: All filesystem work for one matched path, executed inside a single
// blocking task. Uses the same cap-std operations as before (no TOCTOU window).
fn clean_path_sync(
    path: &Path,
    shared: &SharedPath,
    data: &CleanerData,
    filter: &Filter,
    mode: Mode,
) -> PathStats {
    let mut stats = PathStats::default();

    // Closed before the flags run, and that is not tidiness: cap-std opens
    // directories without `FILE_SHARE_DELETE` on Windows, so a live handle on
    // `path` makes every removal of `path` itself fail. An entry that both names
    // files to drop and asks for the directory to go afterwards silently kept
    // the directory — and reported nothing about it, because the removal that
    // failed was the one nobody was watching.
    let named_dir = if data.files_to_remove.is_empty() && data.directories_to_remove.is_empty() {
        None
    } else {
        match open_dir_without_links(path) {
            Ok(dir) => Some(dir),
            Err(e) => {
                // InvalidInput here is the link check refusing to walk out of
                // the directory it was given — a permanent answer about this
                // entry, not a runtime fault, so it is not worth reporting on
                // every run. Anything else (permissions, I/O) still is.
                if e.kind() != io::ErrorKind::InvalidInput {
                    diag::warn(format!("cleaner: open_dir {}: {e}", path.display()));
                }
                None
            }
        }
    };

    for fname in &data.files_to_remove {
        let Some(relative) = safe_relative_path(fname) else {
            diag::warn(format!(
                "cleaner: skipping unsafe file name {fname:?} in {}",
                data.path
            ));
            continue;
        };
        let Some(dir) = &named_dir else { break };
        let fpath = path.join(&relative);
        // A named file is subject to the same age and size rule as everything
        // else: `files_to_remove: ["*.log"]` plus `older_than` is how a rule says
        // "the old logs, not the live one".
        if !filter_allows_file(filter, dir, &Path::new(fname.as_ref()), &fpath) {
            continue;
        }
        match remove_file_in_dir(dir, &relative, &fpath, mode, filter) {
            // The file's own path, not its parent's: that is what was deleted.
            Ok((b, reason)) => {
                let removed = Removed::file(&fpath, b);
                if reason.is_yes() {
                    stats.add(removed)
                } else {
                    stats.merge_blocked(removed, reason)
                }
            }
            // A file the database lists but that is not on disk any more has
            // nothing to clean: the goal is already met. Temp and cache folders
            // churn constantly, so a stale entry is the normal case, not a
            // fault — warning about it would bury the warnings that do matter
            // (access denied, a file in use).
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => diag::warn(format!("cleaner: remove_file {}: {e}", fpath.display())),
        }
    }

    for dname in &data.directories_to_remove {
        let Some(relative) = safe_relative_path(dname) else {
            diag::warn(format!(
                "cleaner: skipping unsafe dir name {dname:?} in {}",
                data.path
            ));
            continue;
        };
        let Some(dir) = &named_dir else { break };
        let dpath = path.join(&relative);

        if filter.excluded(&dpath) {
            continue;
        }

        match remove_dir_in_dir(dir, &relative, &dpath, mode, filter) {
            // Every item inside it is listed by name, not folded into one line:
            // "3 files, 3 dirs" beside a single path tells the reader how much
            // went and nothing about what.
            Ok(removed) => stats.add(removed),
            // Same as for a missing file above: an absent directory is already clean.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => diag::warn(format!("cleaner: remove_dir {}: {e}", dpath.display())),
        }
    }

    // The handle has to be gone before anything removes `path` itself — see where
    // `named_dir` is opened.
    drop(named_dir);

    if data.flags.contains(CleanerFlags::REMOVE_ALL_IN_DIR) {
        // try fast; skip is_dir check for speed (A)
        if let Ok(removed) = remove_dir_sync(path.to_path_buf(), mode, filter) {
            stats.add(removed);
        }
    }

    if data.flags.contains(CleanerFlags::REMOVE_FILES)
        && !filter.excluded(path)
        && let Some(meta) = std::fs::symlink_metadata(path).ok()
        && filter.accepts(
            meta.len(),
            meta.modified().ok().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs())
            }),
        )
        && let Ok((b, reason)) = remove_file_sync(path, mode, filter)
    {
        // The matched path itself, interned: listing it costs a pointer, not a
        // copy of the segments.
        let removed = Removed {
            files: 1,
            bytes: b,
            entries: vec![ClearedPath {
                path: shared.clone(),
                removed_bytes: b,
                removed_files: 1,
                removed_directories: 0,
            }],
            ..Removed::default()
        };
        if reason.is_yes() {
            stats.add(removed)
        } else {
            stats.merge_blocked(removed, reason)
        }
    }

    if data.flags.contains(CleanerFlags::REMOVE_DIRECTORIES)
        && let Ok(removed) = remove_dir_sync(path.to_path_buf(), mode, filter)
    {
        stats.add(removed);
    }

    if data
        .flags
        .contains(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN)
        && let Ok(removed) = remove_dir_sync(path.to_path_buf(), mode, filter)
    {
        stats.add(removed);
    }

    stats
}

/// Whether a file the entry names by hand passes the filter.
///
/// The size and age come from the open directory handle when there is one, so
/// the check describes the same file the removal is about to visit rather than
/// whatever the path resolves to a moment later.
fn filter_allows_file(filter: &Filter, dir: &cap_std::fs::Dir, name: &Path, path: &Path) -> bool {
    if filter.is_empty() {
        return true;
    }

    if filter.excluded(path) {
        return false;
    }

    match dir.symlink_metadata(name) {
        Ok(meta) => filter.accepts(meta.len(), modified_secs(&meta)),
        // A named file that is not there has nothing to clean either way; saying
        // no keeps this from turning into a "file vanished" warning per run.
        Err(_) => false,
    }
}

// PERF: Cap on simultaneously-live per-path futures inside a single cleaner.
// Prevents a huge glob result (e.g. `**`) from allocating one future per path.
const MAX_PATHS_IN_FLIGHT: usize = 64;

/// Clean one glob-matched path for a single database entry. `data` is shared
/// across every path of the entry (Arc); all filesystem work for the path runs
/// in a single blocking task, bounded by the global `BLOCKING` semaphore (B2).
///
/// Returns the path itself next to its counters: `clear_data` folds the counters
/// into one result per database entry, but the results page has to say *which*
/// path freed *how much*, and that is lost the moment they are summed.
async fn clean_one_path(
    path: PathBuf,
    data: Arc<CleanerData>,
    filter: Arc<Filter>,
    mode: Mode,
) -> (SharedPath, PathStats) {
    // Interned before `path` moves into the blocking task: the segments are
    // shared with every other path under the same directories, so a run deletes
    // N paths without storing N copies of the prefix.
    let shared = SharedPath::new(&path.to_string_lossy());

    // B2: bound global blocking concurrency. The permit is held until the
    // blocking task finishes, so at most `BLOCKING` permits run at once.
    let _permit = BLOCKING.clone().acquire_owned().await.ok();

    // Cloned because the same interned path is needed both inside the task and
    // on the way out; the clone is a pointer, not another copy of the segments.
    let for_task = shared.clone();

    // Every failure mode (join error, etc.) falls back to a non-working result,
    // matching the previous per-operation error handling.
    let stats = tokio::task::spawn_blocking(move || {
        clean_path_sync(&path, &for_task, &data, &filter, mode)
    })
    .await
    .unwrap_or_default();

    (shared, stats)
}

/// Walk one database entry: expand its glob, then either remove or measure
/// every matched path.
///
/// The two public entry points below are this one with a different [`Mode`].
/// Keeping them together is the point — a dry run is only worth reading if it
/// counts what the real run would actually delete, and the only way to guarantee
/// that is to run the same walk.
async fn walk_data(data: &CleanerData, mode: Mode) -> CleanerResult {
    let mut out = CleanerResult {
        files: 0,
        folders: 0,
        bytes: 0,
        working: false,
        program: data.program.clone(),
        path: data.path.clone(),
        paths: Vec::new(),
        paths_omitted: 0,
        category: data.category.clone(),
        sub_category: data.sub_category.clone(),
        locked_files: 0,
        locked_bytes: 0,
        locked: Vec::new(),
        denied_files: 0,
        denied_bytes: 0,
        denied: Vec::new(),
    };

    // The rule asks not to run while its owner is open: a browser writes its
    // cache straight back, so cleaning it under a running browser is work that
    // is undone before the user sees the result. Checked once per entry, before
    // anything is walked — the answer cannot change inside a run.
    if processes::running(&data.filter.skip_if_running) {
        return out;
    }

    // INFO: Reject parent-dir traversal in the DB-supplied glob pattern
    let path_str = data.path.to_string();
    if Path::new(&path_str)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return out;
    }

    let glob_iter = match glob(&path_str) {
        Ok(g) => g,
        Err(_) => return out,
    };

    // PERF: share one DB entry across all matched paths (Arc) instead of a full
    // clone per path, and keep at most MAX_PATHS_IN_FLIGHT futures alive.
    let data = Arc::new(data.clone());
    // Compiled once for the whole entry: the exclude globs and the clock are
    // shared by every matched path.
    let filter = Arc::new(Filter::new(&data.filter));

    let mut path_stream = stream::iter(
        glob_iter
            .filter_map(Result::ok)
            // A wildcard such as `{drive}Users\*` also matches the entries
            // Windows keeps in that folder for compatibility: `All Users` and
            // `Все пользователи` are junctions into `C:\ProgramData`. They are
            // never a clean target — walking one would escape the directory it
            // was matched in — and the walk below refuses them anyway, so drop
            // them here instead of paying a warning on every run.
            .filter(|path| !is_linked(path)),
    )
    .map(|path| clean_one_path(path, Arc::clone(&data), Arc::clone(&filter), mode))
    .buffer_unordered(MAX_PATHS_IN_FLIGHT);

    while let Some((_, stats)) = path_stream.next().await {
        if stats.working {
            out.working = true;
            out.files += stats.files;
            out.folders += stats.folders;
            out.bytes += stats.bytes;
            // One entry per removed item, taken from the walk itself rather than
            // re-derived here: only the task knows which of them actually went.
            out.paths.extend(stats.removed);
            out.paths_omitted += stats.omitted;
            out.locked_files += stats.locked_count;
            out.locked_bytes += stats.locked_bytes;
            for path in stats.locked {
                if out.locked.len() < MAX_REMOVED_ENTRIES {
                    out.locked.push(path);
                }
            }
            out.denied_files += stats.denied_count;
            out.denied_bytes += stats.denied_bytes;
            for path in stats.denied {
                if out.denied.len() < MAX_REMOVED_ENTRIES {
                    out.denied.push(path);
                }
            }
        }
    }

    out
}

// NOTE: The main function for data cleansing.
// PERF: one blocking task per matched path, bounded globally by `BLOCKING`.
pub async fn clear_data(data: &CleanerData) -> CleanerResult {
    walk_data(data, Mode::Remove).await
}

/// Walk one entry and report what a real [`clear_data`] would remove, deleting
/// nothing.
///
/// The glob is expanded, every matched path is walked, and the files, folders
/// and bytes it *would* free are counted and named exactly as the real run
/// counts them — so the answer to "how much is this worth" is measured rather
/// than guessed from the pattern in the database.
///
/// Two things it deliberately does not do:
///
/// * **It is not free.** It walks every matched path and reads every file's
///   size, so a full `--all` scan costs about as much I/O as the run does. That
///   is the price of an honest number; a scan that only looked at the patterns
///   would be fast and wrong.
/// * **It is not a promise.** Files can be locked, moved or written to between
///   the scan and the run, and a locked file is removed at the run and not at the
///   scan. Treat the result as the size of the opportunity, not of the outcome.
pub async fn scan_data(data: &CleanerData) -> CleanerResult {
    walk_data(data, Mode::Measure).await
}

/// Creates a directory symlink at `link` pointing at `target`, reporting whether
/// the platform allowed it.
///
/// Windows only grants that privilege to Developer Mode or an elevated token, so
/// a machine with neither cannot set up the tree a test needs. Reporting it —
/// rather than unwrapping — lets those tests skip instead of failing for a
/// reason that has nothing to do with the cleaner.
#[cfg(test)]
pub(crate) fn dir_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link);
    created.is_ok()
}

/// Whether a test that could not create its directory symlink has to stop.
///
/// The reason is printed rather than swallowed: a silently shortened test looks
/// exactly like one that passed on its own merits, and these tests exist
/// precisely to prove the cleaner refuses to walk through a link.
#[cfg(test)]
pub(crate) fn skip_without_symlink(created: bool) -> bool {
    if !created {
        eprintln!(
            "skipped: this platform would not create a directory symlink \
             (Windows needs Developer Mode or an elevated token)"
        );
    }
    !created
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::structures::CleanerData;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_data(path: String) -> CleanerData {
        CleanerData {
            path: path.into(),
            category: std::sync::Arc::from("TestCategory"),
            program: std::sync::Arc::from("TestProgram"),
            class: std::sync::Arc::from("TestClass"),
            sub_category: std::sync::Arc::from("TestSub"),
            files_to_remove: vec![],
            directories_to_remove: vec![],
            flags: CleanerFlags::empty(),
            filter: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_clear_data_nonexistent_path() {
        let data = create_test_data(String::from("/nonexistent/path/*"));
        let result = clear_data(&data).await;

        assert_eq!(result.files, 0);
        assert_eq!(result.folders, 0);
        assert_eq!(result.bytes, 0);
        assert!(!result.working);
    }

    #[tokio::test]
    async fn test_clear_data_remove_files() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_file.txt");
        fs::write(&file_path, b"test content").unwrap();

        let mut data = create_test_data(file_path.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert_eq!(result.files, 1);
        assert!(result.bytes > 0);
        assert!(!file_path.exists());
    }

    /// A scan has to report what the run would free *and* leave everything
    /// where it was. Both halves matter: the counts are the reason to use a
    /// scan, and the untouched filesystem is the reason it is safe to.
    #[tokio::test]
    async fn a_scan_counts_the_files_without_removing_them() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test_file.txt");
        fs::write(&file_path, b"test content").unwrap();

        let mut data = create_test_data(file_path.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let scanned = scan_data(&data).await;

        assert!(scanned.working, "a scan of a real file has to report it");
        assert_eq!(scanned.files, 1);
        assert_eq!(scanned.bytes, b"test content".len() as u64);
        assert!(
            file_path.exists(),
            "a scan must not remove what it measured"
        );
    }

    /// The scan and the run are the same walk, so they have to agree exactly.
    /// If they ever diverge, the dry run is describing a different run.
    #[tokio::test]
    async fn a_scan_and_the_run_agree_on_the_numbers() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("a.txt"), b"aaaa").unwrap();
        fs::write(target.join("b.txt"), b"bb").unwrap();
        fs::create_dir(target.join("nested")).unwrap();
        fs::write(target.join("nested").join("c.txt"), b"cccccc").unwrap();

        let mut data = create_test_data(target.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let scanned = scan_data(&data).await;

        // The scan left the tree in place, which is what made the run below
        // find exactly the same thing the scan measured.
        assert!(target.exists(), "the scan must not remove what it measured");

        let cleaned = clear_data(&data).await;

        assert_eq!(scanned.files, cleaned.files);
        assert_eq!(scanned.folders, cleaned.folders);
        assert_eq!(scanned.bytes, cleaned.bytes);
        assert!(!target.exists(), "and the run did remove it");
    }

    /// A scan must not promise the bytes of a file something is holding open, and
    /// must say which ones they are. This is the case that separates an honest
    /// dry run from a hopeful one: the file is there, the size is real, and the
    /// run will still fail to remove it.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_scan_leaves_locked_files_out_of_the_freable_size() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp_dir = TempDir::new().unwrap();
        let free = temp_dir.path().join("free.bin");
        let held = temp_dir.path().join("held.bin");
        fs::write(&free, vec![0u8; 100]).unwrap();
        fs::write(&held, vec![0u8; 200]).unwrap();

        // No sharing at all: the delete the run would attempt is refused.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .expect("the file opens");

        let mut data = create_test_data(format!("{}/*", temp_dir.path().display()));
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let scanned = scan_data(&data).await;

        // The free file is counted; the locked one is not.
        assert_eq!(scanned.files, 1, "only the removable file is counted");
        assert_eq!(scanned.bytes, 100);
        assert_eq!(scanned.locked_files, 1);
        assert_eq!(
            scanned.locked_bytes, 200,
            "and the unreachable bytes are named"
        );
        assert_eq!(scanned.locked.len(), 1);
        assert!(
            scanned.locked[0].path.to_string().contains("held.bin"),
            "the locked path is reported by name, got {:?}",
            scanned.locked[0].path
        );

        // Nothing was deleted, and the lock is still what a run would meet.
        assert!(free.exists());
        assert!(held.exists());
        drop(handle);
    }

    #[tokio::test]
    async fn test_clear_data_remove_directory() {
        let temp_dir = TempDir::new().unwrap();
        let sub_dir = temp_dir.path().join("sub_dir");
        fs::create_dir(&sub_dir).unwrap();
        fs::write(sub_dir.join("file.txt"), b"content").unwrap();

        let mut data = create_test_data(sub_dir.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_DIRECTORIES);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(result.folders > 0);
        assert!(!sub_dir.exists());
    }

    #[tokio::test]
    async fn test_clear_data_remove_all_in_dir() {
        let temp_dir = TempDir::new().unwrap();
        let target_dir = temp_dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("file1.txt"), b"content1").unwrap();
        fs::write(target_dir.join("file2.txt"), b"content2").unwrap();

        let mut data = create_test_data(target_dir.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(result.files >= 2);
        assert!(!target_dir.exists());
    }

    #[tokio::test]
    async fn test_clear_data_specific_files() {
        let temp_dir = TempDir::new().unwrap();
        let target_dir = temp_dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();
        fs::write(target_dir.join("remove_me.tmp"), b"temp").unwrap();
        fs::write(target_dir.join("keep_me.txt"), b"keep").unwrap();

        let mut data = create_test_data(target_dir.to_str().unwrap().to_string());
        data.files_to_remove = vec![std::sync::Arc::from("remove_me.tmp")];

        let result = clear_data(&data).await;

        assert!(result.working);
        assert_eq!(result.files, 1);
        assert!(!target_dir.join("remove_me.tmp").exists());
        assert!(target_dir.join("keep_me.txt").exists());
    }

    #[tokio::test]
    async fn test_clear_data_specific_directories() {
        let temp_dir = TempDir::new().unwrap();
        let target_dir = temp_dir.path().join("target");
        fs::create_dir(&target_dir).unwrap();

        let remove_dir = target_dir.join("cache");
        fs::create_dir(&remove_dir).unwrap();
        fs::write(remove_dir.join("cache_file.txt"), b"cache").unwrap();

        let keep_dir = target_dir.join("data");
        fs::create_dir(&keep_dir).unwrap();

        let mut data = create_test_data(target_dir.to_str().unwrap().to_string());
        data.directories_to_remove = vec![std::sync::Arc::from("cache")];

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(result.folders >= 1);
        assert!(!remove_dir.exists());
        assert!(keep_dir.exists());
    }

    #[tokio::test]
    async fn test_clear_data_specific_nested_names() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        let nested = base.join("nested");
        fs::create_dir_all(nested.join("cache")).unwrap();
        fs::write(nested.join("old.log"), b"log").unwrap();
        fs::write(nested.join("cache").join("cache.dat"), b"cache").unwrap();
        fs::write(nested.join("keep.txt"), b"keep").unwrap();

        let mut data = create_test_data(base.to_string_lossy().into_owned());
        data.files_to_remove = vec!["nested/old.log".into()];
        data.directories_to_remove = vec!["nested/cache".into()];

        let result = clear_data(&data).await;
        assert_eq!((result.files, result.folders, result.bytes), (2, 1, 8));
        assert!(nested.join("keep.txt").exists());
    }

    #[tokio::test]
    async fn test_clear_data_glob_pattern() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.tmp"), b"temp1").unwrap();
        fs::write(temp_dir.path().join("file2.tmp"), b"temp2").unwrap();
        fs::write(temp_dir.path().join("file3.txt"), b"text").unwrap();

        let pattern = format!("{}/*.tmp", temp_dir.path().to_str().unwrap());
        let mut data = create_test_data(pattern);
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert_eq!(result.files, 2);
        assert!(!temp_dir.path().join("file1.tmp").exists());
        assert!(!temp_dir.path().join("file2.tmp").exists());
        assert!(temp_dir.path().join("file3.txt").exists());
    }

    #[tokio::test]
    async fn test_clear_data_nested_directories() {
        let temp_dir = TempDir::new().unwrap();
        let nested = temp_dir.path().join("level1").join("level2").join("level3");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("deep_file.txt"), b"deep content").unwrap();

        let mut data =
            create_test_data(temp_dir.path().join("level1").to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_DIRECTORIES);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(result.folders >= 3);
        assert!(result.files >= 1);
    }

    #[tokio::test]
    async fn test_clear_data_result_fields() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, b"test").unwrap();

        let mut data = create_test_data(file_path.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;

        assert_eq!(result.program.as_ref(), "TestProgram");
        assert_eq!(result.category.as_ref(), "TestCategory");
        assert_eq!(result.path, file_path.to_str().unwrap());
        assert!(result.working);
    }

    /// A file the database named is deleted by name, so it is listed by name.
    /// Reporting its parent directory instead would credit the clean with
    /// removing a directory that is still there, and hide the file that is not.
    #[tokio::test]
    async fn named_files_are_listed_as_their_own_paths() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();
        fs::write(base.join("drop.tmp"), b"0123456789").unwrap();
        fs::write(base.join("keep.txt"), b"keep").unwrap();

        let mut data = create_test_data(base.to_string_lossy().into_owned());
        data.files_to_remove = vec![std::sync::Arc::from("drop.tmp")];

        let result = clear_data(&data).await;
        assert_eq!(result.files, 1);
        assert_eq!(result.bytes, 10);
        assert_eq!(result.paths.len(), 1, "{:?}", result.paths);
        let entry = &result.paths[0];
        assert_eq!(
            entry.path.as_string(),
            base.join("drop.tmp").to_string_lossy()
        );
        assert_eq!(entry.removed_bytes, 10);
        assert_eq!(entry.removed_files, 1);
        // The directory it was found in is still on disk, so it is not listed.
        assert!(base.exists());
        assert!(base.join("keep.txt").exists());
    }

    /// Same for a named folder: the folder and everything in it are listed by name,
    /// and the parent it was found in is untouched and unlisted.
    #[tokio::test]
    async fn named_directories_are_listed_as_their_own_paths() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        let cache = base.join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("a.dat"), b"content").unwrap();

        let mut data = create_test_data(base.to_string_lossy().into_owned());
        data.directories_to_remove = vec![std::sync::Arc::from("cache")];

        let result = clear_data(&data).await;
        assert_eq!(result.files, 1);
        let mut listed: Vec<String> = result
            .paths
            .iter()
            .map(|entry| entry.path.as_string())
            .collect();
        listed.sort();
        let cache = cache.to_string_lossy().into_owned();
        assert_eq!(
            listed,
            vec![cache.clone(), format!("{}\\{}", cache, "a.dat"),],
            "the folder and the file inside it, each by name:\n{:?}",
            result.paths,
        );
        // Each entry describes only what that item was: the directory is not
        // credited with the file's size, or the list double-counts.
        let file = result
            .paths
            .iter()
            .find(|entry| entry.path.as_string().ends_with("a.dat"))
            .expect("the file");
        assert_eq!(file.removed_bytes, 7);
        assert_eq!(file.removed_files, 1);
        assert_eq!(file.removed_directories, 0);
        let dir = result
            .paths
            .iter()
            .find(|entry| entry.path.as_string() == cache)
            .expect("the directory");
        assert_eq!(dir.removed_bytes, 0, "a directory has no size of its own");
        assert_eq!(dir.removed_directories, 1);
        let listed_bytes: u64 = result.paths.iter().map(|entry| entry.removed_bytes).sum();
        assert_eq!(listed_bytes, result.bytes);
        assert!(!base.join("cache").exists());
        assert!(base.exists());
    }

    /// A directory removed by a flag is walked, and everything in it is listed
    /// by name. One line reading "3 files, 3 dirs" beside the directory says how
    /// much went and nothing about *what* — which is the whole point of the list.
    #[tokio::test]
    async fn a_removed_directory_lists_everything_inside_it() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        let nested = target.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(target.join("one.txt"), b"a").unwrap();
        fs::write(nested.join("two.txt"), b"bb").unwrap();
        fs::write(nested.join("three.txt"), b"ccc").unwrap();

        let mut data = create_test_data(target.to_string_lossy().into_owned());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let result = clear_data(&data).await;
        assert_eq!(result.files, 3);
        assert_eq!(result.folders, 2, "the directory and the one inside it");

        let mut listed: Vec<String> = result
            .paths
            .iter()
            .map(|entry| entry.path.as_string())
            .collect();
        listed.sort();
        let target = target.to_string_lossy().into_owned();
        // Sorted, and the directory itself is the shortest of its own subtree, so
        // it sorts last among the `target\…` entries.
        assert_eq!(
            listed,
            vec![
                target.clone(),
                format!("{}\\{}", target, "nested"),
                format!("{}\\{}\\{}", target, "nested", "three.txt"),
                format!("{}\\{}\\{}", target, "nested", "two.txt"),
                format!("{}\\{}", target, "one.txt"),
            ],
            "every removed item is named, the directory included",
        );
        assert_eq!(result.paths_omitted, 0);

        // Each file carries its own size, and the listed bytes still add up to
        // the run's total: two totals that can disagree is the bug this whole
        // path was fixed to avoid.
        let bytes_of = |suffix: &str| {
            result
                .paths
                .iter()
                .find(|entry| entry.path.as_string().ends_with(suffix))
                .unwrap_or_else(|| panic!("{suffix} not listed: {listed:?}"))
                .removed_bytes
        };
        assert_eq!(bytes_of("one.txt"), 1);
        assert_eq!(bytes_of("two.txt"), 2);
        assert_eq!(bytes_of("three.txt"), 3);
        let listed_bytes: u64 = result.paths.iter().map(|entry| entry.removed_bytes).sum();
        assert_eq!(listed_bytes, result.bytes);
        assert_eq!(result.bytes, 6);
    }

    /// Past the cap the walk keeps counting, and says how many items it did not
    /// list. Dropping them silently is how a report comes to claim to be
    /// complete when it is not.
    #[tokio::test]
    async fn a_huge_directory_is_capped_without_hiding_it() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        for n in 0..(MAX_REMOVED_ENTRIES + 50) {
            fs::write(target.join(format!("f{n:06}")), b"x").unwrap();
        }

        let mut data = create_test_data(target.to_string_lossy().into_owned());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let result = clear_data(&data).await;
        assert_eq!(
            result.paths.len(),
            MAX_REMOVED_ENTRIES,
            "the list stops at the cap",
        );
        // The directory itself is one of the removed items, so it is the one the
        // cap costs: 50 files plus the directory is 51 slots against 1000.
        assert_eq!(
            result.paths_omitted, 51,
            "and the items past the cap are counted, not dropped",
        );
        // The totals still describe everything.
        assert_eq!(result.files, (MAX_REMOVED_ENTRIES + 50) as u64);
        assert!(result.working);
    }

    /// Both named files and a wholesale flag in one entry: the report has to
    /// show the named file *and* the directory that went with it, not merge them
    /// into one line that names neither.
    #[tokio::test]
    async fn a_named_file_and_the_directory_around_it_are_both_listed() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("drop.tmp"), b"1234").unwrap();

        let mut data = create_test_data(target.to_string_lossy().into_owned());
        data.files_to_remove = vec![std::sync::Arc::from("drop.tmp")];
        data.flags
            .insert(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN);

        let result = clear_data(&data).await;
        let listed: Vec<String> = result
            .paths
            .iter()
            .map(|entry| entry.path.as_string())
            .collect();
        assert!(
            listed.contains(&target.join("drop.tmp").to_string_lossy().into_owned()),
            "{listed:?}",
        );
        assert!(
            listed.contains(&target.to_string_lossy().into_owned()),
            "{listed:?}"
        );
        // The counters still add up to what actually went: the file, then the
        // now-empty directory around it.
        assert_eq!(result.files, 1);
        assert_eq!(result.folders, 1);
        assert_eq!(result.bytes, 4);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn test_clear_data_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let empty_dir = temp_dir.path().join("empty");
        fs::create_dir(&empty_dir).unwrap();

        let mut data = create_test_data(empty_dir.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_DIRECTORIES);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert_eq!(result.folders, 1);
        assert_eq!(result.files, 0);
    }

    #[tokio::test]
    async fn test_clear_data_byte_counting() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("sized_file.txt");
        let content = b"0123456789"; // 10 bytes
        fs::write(&file_path, content).unwrap();

        let mut data = create_test_data(file_path.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;

        assert_eq!(result.bytes, 10);
    }

    #[tokio::test]
    async fn test_clear_data_multiple_operations() {
        let temp_dir = TempDir::new().unwrap();
        let target_dir = temp_dir.path().join("multi_test");
        fs::create_dir(&target_dir).unwrap();

        // Create files to remove by name
        fs::write(target_dir.join("temp.tmp"), b"temp").unwrap();

        // Create directory to remove by name
        let cache_dir = target_dir.join("cache");
        fs::create_dir(&cache_dir).unwrap();
        fs::write(cache_dir.join("cache.dat"), b"cache").unwrap();

        let mut data = create_test_data(target_dir.to_str().unwrap().to_string());
        data.files_to_remove = vec![std::sync::Arc::from("temp.tmp")];
        data.directories_to_remove = vec![std::sync::Arc::from("cache")];

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(result.files >= 2); // temp.tmp + cache.dat
        assert!(result.folders >= 1); // cache dir
        assert!(!target_dir.join("temp.tmp").exists());
        assert!(!cache_dir.exists());
    }

    #[tokio::test]
    async fn test_clear_data_rejects_parent_traversal() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();
        fs::write(base.join("ok.txt"), b"x").unwrap();

        let outside = temp_dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"secret").unwrap();

        let mut data = create_test_data(base.to_str().unwrap().to_string());
        data.files_to_remove = vec![std::sync::Arc::from("../outside/secret.txt")];
        data.directories_to_remove = vec![std::sync::Arc::from("../outside")];

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(secret.exists());
        assert!(outside.exists());
        assert!(base.join("ok.txt").exists());
    }

    #[tokio::test]
    async fn test_clear_data_rejects_absolute_names() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();

        let outside = temp_dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"secret").unwrap();

        let mut data = create_test_data(base.to_str().unwrap().to_string());
        data.files_to_remove = vec![std::sync::Arc::from(secret.to_string_lossy().to_string())];
        data.directories_to_remove =
            vec![std::sync::Arc::from(outside.to_string_lossy().to_string())];

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(secret.exists());
        assert!(outside.exists());
    }

    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn test_clear_data_named_paths_do_not_follow_directory_links() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        let outside = temp_dir.path().join("outside");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        fs::create_dir(outside.join("keep_dir")).unwrap();
        if skip_without_symlink(dir_symlink(&outside, &base.join("link"))) {
            return;
        }

        let mut data = create_test_data(base.to_string_lossy().into_owned());
        data.files_to_remove = vec!["link/keep.txt".into()];
        data.directories_to_remove = vec!["link/keep_dir".into()];

        let result = clear_data(&data).await;
        assert!(!result.working);
        assert!(outside.join("keep.txt").exists());
        assert!(outside.join("keep_dir").exists());
    }

    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn test_clear_data_does_not_follow_linked_match_parent() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        let outside = temp_dir.path().join("outside");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("keep.tmp");
        fs::write(&keep, b"keep").unwrap();
        if skip_without_symlink(dir_symlink(&outside, &base.join("link"))) {
            return;
        }

        let mut data = create_test_data(format!("{}/link/*.tmp", base.display()));
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;
        assert!(!result.working);
        assert!(keep.exists());
    }

    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn test_clear_data_named_paths_refuse_linked_root() {
        let temp_dir = TempDir::new().unwrap();
        let outside = temp_dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("keep.txt");
        fs::write(&keep, b"keep").unwrap();
        let link = temp_dir.path().join("link");
        if skip_without_symlink(dir_symlink(&outside, &link)) {
            return;
        }

        let mut data = create_test_data(link.to_string_lossy().into_owned());
        data.files_to_remove = vec!["keep.txt".into()];

        let result = clear_data(&data).await;
        assert!(!result.working);
        assert!(keep.exists());
    }

    #[tokio::test]
    async fn test_remove_directory_after_clean_counts_deleted_contents() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file.txt"), b"content").unwrap();

        let mut data = create_test_data(target.to_string_lossy().into_owned());
        data.flags
            .insert(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN);

        let result = clear_data(&data).await;
        assert_eq!((result.files, result.folders, result.bytes), (1, 1, 7));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn test_clear_data_rejects_parent_in_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();
        let f = base.join("f.txt");
        fs::write(&f, b"x").unwrap();

        let pattern = format!(
            "{}/../base/*.txt",
            temp_dir.path().join("base").to_str().unwrap()
        );
        let mut data = create_test_data(pattern);
        data.flags.insert(CleanerFlags::REMOVE_FILES);

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(f.exists());
    }

    #[tokio::test]
    async fn test_clear_data_skips_dot_and_empty_names() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();
        fs::write(base.join("ok.txt"), b"x").unwrap();

        let mut data = create_test_data(base.to_str().unwrap().to_string());
        data.files_to_remove = vec![std::sync::Arc::from("."), std::sync::Arc::from("")];

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(base.join("ok.txt").exists());
    }

    // INFO: junction on Windows, symlink on Unix. The cleaner must remove the
    // link itself, never descend into or delete the target's contents.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_clear_data_junction_inside_tree_not_followed() {
        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();

        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("valuable.txt"), b"keep").unwrap();

        let link = base.join("link");
        if skip_without_symlink(dir_symlink(&target, &link)) {
            return;
        }

        let mut data = create_test_data(base.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(target.join("valuable.txt").exists());
        assert!(!link.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_clear_data_symlink_inside_tree_not_followed() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().join("base");
        fs::create_dir(&base).unwrap();

        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("valuable.txt"), b"keep").unwrap();

        let link = base.join("link");
        symlink(&target, &link).unwrap();

        let mut data = create_test_data(base.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);

        let result = clear_data(&data).await;

        assert!(result.working);
        assert!(target.join("valuable.txt").exists());
        assert!(!link.exists());
    }

    // INFO: root itself is a link -> refuse instead of following it.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_clear_data_refuses_junction_root() {
        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("valuable.txt"), b"keep").unwrap();

        let link = temp_dir.path().join("link");
        if skip_without_symlink(dir_symlink(&target, &link)) {
            return;
        }

        let mut data = create_test_data(link.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.flags
            .insert(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN);

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(target.join("valuable.txt").exists());
        assert!(link.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_clear_data_refuses_symlink_root() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let target = temp_dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("valuable.txt"), b"keep").unwrap();

        let link = temp_dir.path().join("link");
        symlink(&target, &link).unwrap();

        let mut data = create_test_data(link.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.flags
            .insert(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN);

        let result = clear_data(&data).await;

        assert!(!result.working);
        assert!(target.join("valuable.txt").exists());
        assert!(link.exists());
    }

    /// A filter's exclusions are the difference between "delete this cache" and
    /// "delete this cache but not the user's saved logins". The named entry and
    /// everything under it has to survive, and it must not be counted.
    #[tokio::test]
    async fn an_exclusion_keeps_the_named_entry_and_its_contents() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("cache");
        fs::create_dir_all(root.join("user-data")).unwrap();
        fs::write(root.join("junk.tmp"), b"12345").unwrap();
        fs::write(root.join("user-data").join("Logins"), b"secret").unwrap();

        let mut data = create_test_data(root.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.filter.exclude.push("user-data".into());

        let result = clear_data(&data).await;

        assert!(!root.join("junk.tmp").exists());
        assert!(
            root.join("user-data").join("Logins").exists(),
            "an excluded directory must keep everything under it"
        );
        assert_eq!(result.files, 1);
        assert_eq!(result.bytes, 5);
    }

    /// `older_than` is measured from the file's mtime, and a file that is not
    /// old enough is neither removed nor counted.
    #[tokio::test]
    async fn files_younger_than_the_age_limit_are_left_alone() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("logs");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("fresh.log"), b"new").unwrap();

        let mut data = create_test_data(root.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.filter.older_than_seconds = Some(86_400);

        let result = clear_data(&data).await;

        assert!(root.join("fresh.log").exists());
        assert_eq!(result.files, 0);
        assert_eq!(result.bytes, 0);
    }

    /// Size bounds are a property of the file, checked on the same walk.
    #[tokio::test]
    async fn size_bounds_are_applied_per_file() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("small.bin"), vec![0u8; 10]).unwrap();
        fs::write(root.join("big.bin"), vec![0u8; 100]).unwrap();

        let mut data = create_test_data(root.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.filter.min_size = Some(50);

        let result = clear_data(&data).await;

        assert!(root.join("small.bin").exists(), "too small to remove");
        assert!(!root.join("big.bin").exists());
        assert_eq!(result.files, 1);
        assert_eq!(result.bytes, 100);
    }

    /// Depth is counted from the matched path: an entry directly inside it is
    /// depth 1, so `max_depth(1)` is one level and nothing below it.
    #[tokio::test]
    async fn max_depth_stops_the_walk_where_it_says() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("cache");
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::write(root.join("one.tmp"), b"1").unwrap();
        fs::write(root.join("a").join("two.tmp"), b"22").unwrap();
        fs::write(root.join("a").join("b").join("three.tmp"), b"333").unwrap();

        let mut data = create_test_data(root.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.filter.max_depth = Some(2);

        let result = clear_data(&data).await;

        assert!(!root.join("one.tmp").exists(), "depth 1 is inside");
        assert!(!root.join("a").join("two.tmp").exists(), "depth 2 too");
        assert!(
            root.join("a").join("b").join("three.tmp").exists(),
            "depth 3 is past the limit"
        );
        assert_eq!(result.files, 2);
    }

    /// A filter that only excludes cannot let the walk take the
    /// delete-everything shortcut, or the exclusion would be ignored.
    #[test]
    fn a_filter_with_only_an_exclusion_is_not_empty() {
        let mut filter = CleanerFilter::default();
        assert!(filter.is_empty());

        filter.exclude.push("keep".into());
        assert!(!filter.is_empty());
    }

    /// The same walk runs for a scan, so a filtered scan promises exactly what
    /// the filtered run would free.
    #[tokio::test]
    async fn a_filtered_scan_agrees_with_the_filtered_run() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.lock"), b"keep").unwrap();
        fs::write(root.join("junk.tmp"), b"junk").unwrap();

        let mut data = create_test_data(root.to_str().unwrap().to_string());
        data.flags.insert(CleanerFlags::REMOVE_ALL_IN_DIR);
        data.filter.exclude.push("*.lock".into());

        let scanned = scan_data(&data).await;
        assert_eq!(scanned.files, 1);
        assert_eq!(scanned.bytes, 4);

        let run = clear_data(&data).await;
        assert_eq!(run.files, scanned.files);
        assert_eq!(run.bytes, scanned.bytes);
        assert!(root.join("keep.lock").exists());
    }
}

// Property-based tests with proptest
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;
    use tempfile::TempDir;

    // helper to run async clear_data inside sync proptest
    fn run_async<F, T>(f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    proptest! {
        /// Property: byte counting should always match actual file sizes
        #[test]
        fn prop_byte_counting_accurate(content in prop::collection::vec(any::<u8>(), 0..1000)) {
            let temp_dir = TempDir::new().unwrap();
            let file_path = temp_dir.path().join("test_file.bin");
            fs::write(&file_path, &content).unwrap();

            let data = CleanerData {
                path: file_path.to_str().unwrap().to_string().into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_FILES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.bytes, content.len() as u64);
        }

        /// Property: file counter should match number of files deleted
        #[test]
        fn prop_file_counter_accurate(num_files in 1usize..50) {
            let temp_dir = TempDir::new().unwrap();
            let target_dir = temp_dir.path().join("files");
            fs::create_dir(&target_dir).unwrap();

            for i in 0..num_files {
                fs::write(target_dir.join(format!("file_{}.txt", i)), b"content").unwrap();
            }

            let pattern = format!("{}/*.txt", target_dir.to_str().unwrap());
            let data = CleanerData {
                path: pattern.into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_FILES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.files, num_files as u64);
        }

        /// Property: clearing non-existent path should always be safe
        #[test]
        fn prop_nonexistent_path_safe(path in "[a-z]{1,20}/[a-z]{1,20}") {
            let non_existent = format!("/tmp/nonexistent_{}/file.txt", path);
            let data = CleanerData {
                path: non_existent.into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_FILES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert!(!result.working);
            prop_assert_eq!(result.files, 0);
            prop_assert_eq!(result.folders, 0);
            prop_assert_eq!(result.bytes, 0);
        }

        /// Property: removing empty directories should work
        #[test]
        fn prop_empty_directory_removal(num_dirs in 1usize..20) {
            let temp_dir = TempDir::new().unwrap();

            for i in 0..num_dirs {
                let dir = temp_dir.path().join(format!("empty_dir_{}", i));
                fs::create_dir(&dir).unwrap();
            }

            let pattern = format!("{}/*", temp_dir.path().to_str().unwrap());
            let data = CleanerData {
                path: pattern.into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_DIRECTORIES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.folders, num_dirs as u64);
            prop_assert_eq!(result.files, 0);
        }

        /// Property: result should always have correct program/category
        #[test]
        fn prop_result_metadata(program in "[A-Za-z]{3,20}", category in "[A-Za-z]{3,20}") {
            let temp_dir = TempDir::new().unwrap();
            let file_path = temp_dir.path().join("test.txt");
            fs::write(&file_path, b"test").unwrap();

            let data = CleanerData {
                path: file_path.to_str().unwrap().to_string().into(),
                category: std::sync::Arc::from(category.clone()),
                program: std::sync::Arc::from(program.clone()),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_FILES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.program.as_ref(), program.as_str());
            prop_assert_eq!(result.category.as_ref(), category.as_str());
        }

        /// Property: nested directory deletion should count all subdirectories
        #[test]
        fn prop_nested_directory_counting(depth in 1usize..5) {
            let temp_dir = TempDir::new().unwrap();
            let mut current = temp_dir.path().join("level_0");
            fs::create_dir(&current).unwrap();

            for i in 1..depth {
                current = current.join(format!("level_{}", i));
                fs::create_dir(&current).unwrap();
            }

            let start_dir = temp_dir.path().join("level_0");
            let data = CleanerData {
                path: start_dir.to_str().unwrap().to_string().into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_DIRECTORIES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.folders, depth as u64);
        }

        /// Property: specific file removal should only remove specified files
        #[test]
        fn prop_specific_file_removal(filename in "[a-z]{3,10}\\.(txt|tmp|log)") {
            let temp_dir = TempDir::new().unwrap();
            let target_dir = temp_dir.path().join("target");
            fs::create_dir(&target_dir).unwrap();

            // Create the target file
            fs::write(target_dir.join(&filename), b"remove").unwrap();
            // Create other files
            fs::write(target_dir.join("keep1.txt"), b"keep").unwrap();
            fs::write(target_dir.join("keep2.txt"), b"keep").unwrap();

            let data = CleanerData {
                path: target_dir.to_str().unwrap().to_string().into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![std::sync::Arc::from(filename.clone())],
                directories_to_remove: vec![],
                flags: CleanerFlags::empty(),
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.files, 1);
            prop_assert!(!target_dir.join(&filename).exists());
            prop_assert!(target_dir.join("keep1.txt").exists());
            prop_assert!(target_dir.join("keep2.txt").exists());
        }

        /// Property: total bytes should equal sum of all file sizes
        #[test]
        fn prop_total_bytes_sum(file_sizes in prop::collection::vec(0u64..10000, 1..10)) {
            let temp_dir = TempDir::new().unwrap();
            let target_dir = temp_dir.path().join("bytes_test");
            fs::create_dir(&target_dir).unwrap();

            let mut expected_bytes = 0u64;
            for (i, size) in file_sizes.iter().enumerate() {
                let content = vec![0u8; *size as usize];
                fs::write(target_dir.join(format!("file_{}.dat", i)), &content).unwrap();
                expected_bytes += size;
            }

            let pattern = format!("{}/*.dat", target_dir.to_str().unwrap());
            let data = CleanerData {
                path: pattern.into(),
                category: std::sync::Arc::from("Test"),
                program: std::sync::Arc::from("Test"),
                class: std::sync::Arc::from("Test"),
                sub_category: std::sync::Arc::from("Test"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::REMOVE_FILES,
                filter: Default::default(),
            };

            let result = run_async(clear_data(&data));
            prop_assert_eq!(result.bytes, expected_bytes);
            prop_assert_eq!(result.files, file_sizes.len() as u64);
        }
    }
}
