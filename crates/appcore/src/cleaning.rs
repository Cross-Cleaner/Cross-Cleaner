//! Background cleaning job: runs the selected cleaners concurrently and
//! reports progress through an mpsc channel.
//!
//! Two message shapes are sent:
//!
//! * `"Cleaning: <program>"` — the name of the entry that just started, used
//!   by the frontend as the "currently working on…" line.
//! * `"PROGRESS:<done>:<total>:<bytes>"` — the counters for the progress bar.

use cleaner::{clear_data, scan_data};
use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::{RegistryDatabase, clear_registry};
use database::structures::{CleanerResult, Cleared, ClearedPath, CustomCleaner};
use database::utils::get_file_size_string;
use futures::stream::{FuturesUnordered, StreamExt};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::sync::mpsc;

use crate::categories::effective_sub;

/// Prefix of the progress counter messages.
const PROGRESS_PREFIX: &str = "PROGRESS:";
/// Prefix of the "currently cleaning" messages.
const CLEANING_PREFIX: &str = "Cleaning: ";

/// How many deleted paths one program keeps for the results page.
///
/// A `…\Cache_Data\*` glob can match tens of thousands of files, and the detail
/// list is there to be scrolled through, not read to the end: past this point
/// the extra entries cost memory in every frame after the run without adding
/// anything a user can reach. The ones kept are the largest, and the page says
/// how many it left out rather than passing the list off as complete.
const MAX_PATH_DETAILS: usize = 1000;

/// Everything the cleaning job reports back.
///
/// The program list is shared rather than copied: it is read by every frame of
/// the results page and holds a detail entry per deleted path, so cloning it
/// once per repaint would cost more than drawing the page does.
pub type CleanResult = (u64, u64, u64, Arc<[Cleared]>);

/// Whether a run deletes what it finds or only measures it.
///
/// A scan is the same walk with the deletions left out, which is the only way its
/// numbers can be trusted: a dry run that re-derived the work would eventually
/// disagree with the run it is describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Remove what is matched. What every frontend does on its Cleaning page.
    #[default]
    Clean,
    /// Report what would be removed and leave it in place.
    Scan,
}

impl Mode {
    /// True when the run is allowed to delete.
    pub fn removes(self) -> bool {
        self == Mode::Clean
    }
}

/// What a scan found, split into what a run can free and what it cannot.
///
/// The split is the point of a scan. One total would promise space the run is not
/// going to deliver, because a file a running application holds open is present,
/// measurable, and still undeletable.
///
/// No `Debug`: it carries the same [`Cleared`] rows a run reports, and those
/// hold a detail entry per path. Debug-printing a scan would mean formatting a
/// second copy of a report the caller is about to print properly.
#[derive(Clone, Default)]
pub struct ScanReport {
    /// Bytes, files, directories and per-program rows a run could remove now.
    pub free: CleanResult,
    /// Files something is holding open, and the bytes they hold.
    pub locked_files: u64,
    /// Items a scan found the ACL refuses to delete. Reported apart from
    /// `locked_files` because the fix is different: elevation, not closing the
    /// program that is holding them.
    pub denied_files: u64,
    pub denied_bytes: u64,
    pub denied: Vec<ClearedPath>,
    pub locked_bytes: u64,
    /// Named locked files, largest first.
    pub locked: Arc<[ClearedPath]>,
    /// Cleaners a scan cannot measure, and why.
    ///
    /// Custom cleaners run arbitrary code, and registry entries only exist to be
    /// deleted; neither can be walked without doing the thing. A scan that
    /// quietly counted them as zero would understate the work, so they are named
    /// here instead.
    pub unmeasured: Vec<Unmeasured>,
}

/// A cleaner a scan could not measure, with the reason it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmeasured {
    /// The cleaner's own name: a custom cleaner's id, or `Program/Category`.
    pub id: String,
    pub reason: &'static str,
}

/// Runs the cleaning job, or — with [`Mode::Scan`] — the same walk without the
/// deletions.
///
/// The `mode` parameter exists so a dry run cannot drift from the run: every
/// filter, every flag check and every entry ordering below is shared, and the
/// only thing that changes is whether the walk is allowed to delete.
pub async fn work(
    selected_map: HashMap<Arc<str>, HashSet<Arc<str>>>,
    progress_sender: mpsc::Sender<String>,
    database: &CleanerDatabase,
    custom_database: &[CustomCleaner],
    #[cfg(windows)] registry_database: &RegistryDatabase,
    excluded_programs: HashSet<Arc<str>>,
    excluded_program_categories: HashSet<(Arc<str>, Arc<str>)>,
) -> CleanResult {
    work_with(
        selected_map,
        progress_sender,
        database,
        custom_database,
        #[cfg(windows)]
        registry_database,
        excluded_programs,
        excluded_program_categories,
        Mode::Clean,
    )
    .await
    .free
}

/// [`work`], with the mode and the scan's extra findings.
///
/// Returns [`ScanReport`] rather than a bare [`CleanResult`], because a scan has
/// two answers — what is freeable and what is locked — and collapsing them into
/// one number is the mistake this type exists to prevent.
pub async fn scan(
    selected_map: HashMap<Arc<str>, HashSet<Arc<str>>>,
    progress_sender: mpsc::Sender<String>,
    database: &CleanerDatabase,
    custom_database: &[CustomCleaner],
    #[cfg(windows)] registry_database: &RegistryDatabase,
    excluded_programs: HashSet<Arc<str>>,
    excluded_program_categories: HashSet<(Arc<str>, Arc<str>)>,
) -> ScanReport {
    work_with(
        selected_map,
        progress_sender,
        database,
        custom_database,
        #[cfg(windows)]
        registry_database,
        excluded_programs,
        excluded_program_categories,
        Mode::Scan,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn work_with(
    selected_map: HashMap<Arc<str>, HashSet<Arc<str>>>,
    progress_sender: mpsc::Sender<String>,
    database: &CleanerDatabase,
    custom_database: &[CustomCleaner],
    #[cfg(windows)] registry_database: &RegistryDatabase,
    excluded_programs: HashSet<Arc<str>>,
    excluded_program_categories: HashSet<(Arc<str>, Arc<str>)>,
    mode: Mode,
) -> ScanReport {
    let mut current_task = 0;

    // ASYNC without threads: pure FuturesUnordered
    let mut bytes_cleared: u64 = 0;
    let mut removed_files: u64 = 0;
    let mut removed_directories: u64 = 0;
    let mut cleared_programs = Vec::<Cleared>::new();

    // INFO: only ever non-empty in a scan; kept in the accumulator so both modes
    // share one return shape and the counting cannot be forgotten in one of them.
    let mut locked_files: u64 = 0;
    let mut locked_bytes: u64 = 0;
    let mut denied_files: u64 = 0;
    let mut denied_bytes: u64 = 0;
    let mut denied: Vec<ClearedPath> = Vec::new();
    let mut locked: Vec<ClearedPath> = Vec::new();
    let mut unmeasured: Vec<Unmeasured> = Vec::new();

    // C: limit to 8 concurrent cleaners
    let sem = Arc::new(tokio::sync::Semaphore::new(8));
    let mut futures: FuturesUnordered<Pin<Box<dyn Future<Output = CleanerResult> + Send>>> =
        FuturesUnordered::new();

    // INFO: Clear LastActivity from Registry
    // WARN: Windows only - show what is being cleaned right now
    // Streams directly into FuturesUnordered to avoid buffering all matches in RAM.
    #[cfg(windows)]
    {
        let scanning = mode == Mode::Scan;
        let _ = registry_database.for_each(|data| {
            let eff = effective_sub(&data.class, &data.sub_category);
            if let Some(subs) = selected_map.get(data.category.as_ref())
                && subs.contains(&eff)
                && !excluded_programs.contains(data.program.as_ref())
                && !excluded_program_categories
                    .contains(&(Arc::clone(&data.program), Arc::clone(&data.category)))
            {
                // INFO: A registry entry is measured by the same enumeration a run
                // performs, with the write left out, and each resolved key is
                // probed for the access the entry needs. A key that cannot be
                // deleted is reported as blocked rather than as a saving.
                if scanning {
                    let data = data.clone();
                    let program = data.program.clone();
                    let sender = progress_sender.clone();
                    let sem = sem.clone();
                    futures.push(Box::pin(async move {
                        let _p = sem.acquire_owned().await.unwrap();
                        let _ = sender.send(format!("{CLEANING_PREFIX}{program}")).await;
                        database::registry_database::measure_registry(&data)
                    }));
                    return;
                }
                let sender = progress_sender.clone();
                let name_msg = data.program.clone();
                let sem = sem.clone();
                futures.push(Box::pin(async move {
                    let _p = sem.acquire_owned().await.unwrap();
                    let _ = sender.send(format!("{CLEANING_PREFIX}{name_msg}")).await;
                    clear_registry(&data)
                }));
            }
        });
    }

    // INFO: Run built-in custom cleanings (functions defined in cleaner::custom_cleaners)
    let mut sequential_cleaners: Vec<CustomCleaner> = Vec::new();
    for data in custom_database.iter() {
        let eff = effective_sub("", &data.sub_category);
        if let Some(subs) = selected_map.get(data.category.as_ref())
            && subs.contains(&eff)
            && !excluded_programs.contains(data.program.as_ref())
            && !excluded_program_categories
                .contains(&(Arc::clone(&data.program), Arc::clone(&data.category)))
        {
            if data.sequential {
                sequential_cleaners.push(data.clone());
            } else if mode == Mode::Scan {
                // INFO: a custom cleaner is arbitrary code — image
                // optimization, an external tool call. There is nothing to walk
                // ahead of time, so a scan names it instead of guessing a size.
                unmeasured.push(Unmeasured {
                    id: data.id.clone(),
                    reason: "custom cleaners can only be measured by running them",
                });
            } else {
                let data = data.clone();
                let sender = progress_sender.clone();
                let name_msg = data.id.clone();
                let sem = sem.clone();
                futures.push(Box::pin(async move {
                    let _p = sem.acquire_owned().await.unwrap();
                    let _ = sender.send(format!("{CLEANING_PREFIX}{name_msg}")).await;
                    let progress_for_cleaner = sender.clone();
                    database::custom_cleaners::run_custom_cleaner(&data, Some(progress_for_cleaner))
                        .await
                }));
            }
        }
    }

    // INFO: Stream the database and keep only the selected entries directly into
    // FuturesUnordered to avoid buffering Vec<Arc<CleanerData>> in RAM.
    let _ = database.for_each(|data| {
        let eff = effective_sub(&data.class, &data.sub_category);
        if let Some(subs) = selected_map.get(data.category.as_ref())
            && subs.contains(&eff)
            && !excluded_programs.contains(data.program.as_ref())
            && !excluded_program_categories
                .contains(&(Arc::clone(&data.program), Arc::clone(&data.category)))
        {
            let data = Arc::new(data);
            let sender = progress_sender.clone();
            let path_msg = data.program.clone();
            let sem = sem.clone();
            // INFO: the same walk either way. `clear_data` and `scan_data` are
            // one function behind two names, so the two modes cannot report
            // different things about the same entry.
            let scanning = mode == Mode::Scan;
            futures.push(Box::pin(async move {
                let _p = sem.acquire_owned().await.unwrap();
                let _ = sender.send(format!("{CLEANING_PREFIX}{path_msg}")).await;
                if scanning {
                    scan_data(&data).await
                } else {
                    clear_data(&data).await
                }
            }));
        }
    });

    let total_tasks = futures.len() + sequential_cleaners.len();
    let _ = progress_sender
        .send(format!("{PROGRESS_PREFIX}0:{total_tasks}:0"))
        .await;

    while let Some(mut result) = futures.next().await {
        current_task += 1;

        // Read before the move: `fold` owns the result, and the run's own totals
        // are keyed on whether it had anything to count at all.
        let (bytes, files, folders) = (result.bytes, result.files, result.folders);
        // INFO: a result whose only content is locked files counts as nothing
        // removed — `fold` says so by returning false — but its locked totals
        // still have to be collected, or the scan would report a directory full
        // of locked files as empty. Read before the move: `fold` takes `result`.
        locked_files += result.locked_files;
        locked_bytes += result.locked_bytes;
        let taken = std::mem::take(&mut result.locked);
        locked.extend(taken);
        denied_files += result.denied_files;
        denied_bytes += result.denied_bytes;
        denied.extend(std::mem::take(&mut result.denied));
        if fold(&mut cleared_programs, result) {
            bytes_cleared += bytes;
            removed_files += files;
            removed_directories += folders;
        }

        // Send only progress and cleared bytes; the program name was already sent before cleaning
        let _ = progress_sender
            .send(format!(
                "{PROGRESS_PREFIX}{current_task}:{total_tasks}:{bytes_cleared}"
            ))
            .await;
    }

    // Run sequential cleaners one at a time (image optimizers, etc.)
    for data in sequential_cleaners {
        // INFO: the same reason as the concurrent custom cleaners above: they
        // run code, so a scan cannot measure them without running them.
        if mode == Mode::Scan {
            unmeasured.push(Unmeasured {
                id: data.id.clone(),
                reason: "custom cleaners can only be measured by running them",
            });
            continue;
        }
        current_task += 1;
        let _ = progress_sender
            .send(format!("{CLEANING_PREFIX}{}", data.id))
            .await;
        let result =
            database::custom_cleaners::run_custom_cleaner(&data, Some(progress_sender.clone()))
                .await;

        let (bytes, files, folders) = (result.bytes, result.files, result.folders);
        if fold(&mut cleared_programs, result) {
            bytes_cleared += bytes;
            removed_files += files;
            removed_directories += folders;
        }

        let _ = progress_sender
            .send(format!(
                "{PROGRESS_PREFIX}{current_task}:{total_tasks}:{bytes_cleared}"
            ))
            .await;
    }

    // Largest first: the detail overlay is a scrolling list, so the entries that
    // freed the most space have to be at the top of it.
    for cleared in &mut cleared_programs {
        cleared
            .paths
            .sort_by_key(|detail| Reverse(detail.removed_bytes));
    }

    // A scan has nothing to celebrate, so it must not pop a notification
    // claiming the disk was cleared when nothing was.
    if mode.removes() {
        notify_result(bytes_cleared, removed_files, removed_directories);

        // Scripts hear about the run after it is over, with the rows the results
        // page will show — not the selection, which is what was *asked* for. The
        // hooks run on their own thread, so a slow webhook cannot stall the UI.
        crate::scripts::notify_finished(
            bytes_cleared,
            removed_files,
            removed_directories,
            &cleared_programs,
        );
    }

    // Largest first for the same reason: a locked file is the one the user has
    // to act on, and the biggest is the one worth acting on first.
    locked.sort_by_key(|detail| Reverse(detail.removed_bytes));

    ScanReport {
        free: (
            bytes_cleared,
            removed_files,
            removed_directories,
            cleared_programs.into(),
        ),
        locked_files,
        locked_bytes,
        locked: locked.into(),
        denied_files,
        denied_bytes,
        denied: denied.into(),
        unmeasured,
    }
}

/// Folds one finished cleaner into the per-program table.
///
/// The concurrent and the sequential pass both come through here, so they cannot
/// aggregate differently: one program ends up as one row with one list of
/// deleted paths however it was cleaned.
///
/// Returns `true` when the cleaner removed anything — a cleaner that matched
/// nothing contributes no row and nothing to the run's totals.
fn fold(cleared_programs: &mut Vec<Cleared>, result: CleanerResult) -> bool {
    if !result.working {
        return false;
    }

    let index = match cleared_programs
        .iter()
        .position(|cleared| cleared.program == result.program.as_ref())
    {
        Some(index) => index,
        None => {
            cleared_programs.push(Cleared {
                program: result.program.to_string(),
                removed_bytes: 0,
                removed_files: 0,
                removed_directories: 0,
                affected_categories: Vec::new(),
                paths: Vec::new(),
                paths_omitted: 0,
            });
            cleared_programs.len() - 1
        }
    };

    let cleared = &mut cleared_programs[index];
    cleared.removed_bytes += result.bytes;
    cleared.removed_files += result.files;
    cleared.removed_directories += result.folders;
    let category = result.category.to_string();
    if !cleared.affected_categories.contains(&category) {
        cleared.affected_categories.push(category);
    }
    // Counted even before the cap: a run that deleted more than the cap holds has
    // to say so, and the count is only true if it includes what was dropped here.
    cleared.paths_omitted += result.paths_omitted;
    for detail in result.paths {
        keep_detail(cleared, detail);
    }
    true
}

/// Keeps one deleted path, evicting the smallest kept entry once the cap bites.
///
/// Evicting rather than refusing is what makes the cap keep the useful half: a
/// truncated list that drops the paths which freed the most space would be the
/// wrong list to show.
fn keep_detail(cleared: &mut Cleared, detail: ClearedPath) {
    if cleared.paths.len() < MAX_PATH_DETAILS {
        cleared.paths.push(detail);
        return;
    }
    cleared.paths_omitted += 1;
    let Some((index, smallest)) = cleared
        .paths
        .iter()
        .enumerate()
        .min_by_key(|(_, kept)| kept.removed_bytes)
    else {
        return;
    };
    if smallest.removed_bytes < detail.removed_bytes {
        cleared.paths[index] = detail;
    }
}

/// Shows the desktop notification with the final numbers. Kept out of line
/// because it owns a temporary icon file for its whole body.
fn notify_result(bytes_cleared: u64, removed_files: u64, removed_directories: u64) {
    let mut temp_file = NamedTempFile::new().unwrap();
    temp_file.write_all(database::ICON_BYTES).unwrap();
    let icon_path = temp_file.path().to_str().unwrap();

    let notification_body = format!(
        "Removed: {}\nFiles: {}\nDirs: {}",
        get_file_size_string(bytes_cleared),
        removed_files,
        removed_directories
    );

    let notification_result = notify_rust::Notification::new()
        .summary("Cross Cleaner")
        .body(&notification_body)
        .icon(icon_path)
        .show();

    temp_file.close().unwrap();
    if let Err(e) = notification_result {
        database::diag::warn(format!("failed to show notification: {e:?}"));
    }
}

/// Strips the `"Cleaning: "` prefix from a progress message.
pub fn program_name(message: &str) -> &str {
    message.strip_prefix(CLEANING_PREFIX).unwrap_or(message)
}

/// Parses a `"PROGRESS:<done>:<total>:<bytes>"` message. Returns `None` for
/// every other message so the caller can treat it as a plain status line.
pub fn parse_progress(message: &str) -> Option<(usize, usize, u64)> {
    let rest = message.strip_prefix(PROGRESS_PREFIX)?;
    let mut parts = rest.split(':');
    let done = parts.next()?.parse().ok()?;
    let total = parts.next()?.parse().ok()?;
    let bytes = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((done, total, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::cleaner_database::CleanerDatabase;
    use database::structures::CleanerData;

    /// A scan's job is to divide the tree into what a run frees and what it cannot.
    /// A single total would be the failure mode, so the two are separate fields and
    /// the free total must never absorb the locked one.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_scan_separates_freeable_bytes_from_locked_ones() {
        use std::fs;
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().expect("a temp dir");
        let free = dir.path().join("free.bin");
        let held = dir.path().join("held.bin");
        fs::write(&free, vec![0u8; 512]).unwrap();
        fs::write(&held, vec![0u8; 4096]).unwrap();

        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .expect("the file opens");

        let database = CleanerDatabase::from_vec(vec![CleanerData {
            path: format!("{}/*", dir.path().display()).into(),
            category: Arc::from("Cache"),
            program: Arc::from("Locked"),
            class: Arc::from("Application"),
            sub_category: Arc::from(""),
            files_to_remove: vec![],
            directories_to_remove: vec![],
            flags: database::structures::CleanerFlags::REMOVE_FILES,
        }]);

        let (tx, _rx) = mpsc::channel(64);
        let report = scan(
            // The entry has no `sub_category`, so it is selected by the empty
            // pseudo-subcategory, exactly as the category page ticks it.
            HashMap::from([(Arc::from("Cache"), HashSet::from([Arc::from("")]))]),
            tx,
            &database,
            &[],
            &RegistryDatabase::from_vec(Vec::new()),
            HashSet::new(),
            HashSet::new(),
        )
        .await;

        assert_eq!(report.free.0, 512, "only the removable file counts");
        assert_eq!(report.locked_files, 1);
        assert_eq!(report.locked_bytes, 4096);
        assert_eq!(report.locked.len(), 1, "and it is named");
        // Nothing was deleted: the whole point of asking.
        assert!(free.exists());
        assert!(held.exists());
        drop(handle);
    }

    #[tokio::test]
    async fn a_scan_names_the_cleaners_it_cannot_measure() {
        use database::structures::CustomCleaner;

        let database = CleanerDatabase::from_vec(Vec::new());
        let custom = vec![CustomCleaner {
            id: "Optimize pictures".to_string(),
            program: Arc::from("Pictures"),
            category: Arc::from("Images"),
            sub_category: Arc::from("Pictures"),
            path: "pictures".into(),
            args: Vec::new(),
            os: Vec::new(),
            function: |_, _| Box::pin(async { panic!("a scan must not run a cleaner") }),
            sequential: false,
        }];

        let (tx, _rx) = mpsc::channel(64);
        #[cfg(windows)]
        let registry = RegistryDatabase::from_vec(Vec::new());
        let report = scan(
            HashMap::from([(Arc::from("Images"), HashSet::from([Arc::from("Pictures")]))]),
            tx,
            &database,
            &custom,
            #[cfg(windows)]
            &registry,
            HashSet::new(),
            HashSet::new(),
        )
        .await;

        assert_eq!(report.unmeasured.len(), 1);
        assert_eq!(report.unmeasured[0].id, "Optimize pictures");
        assert!(!report.unmeasured[0].reason.is_empty());
    }

    #[test]
    fn parses_progress_messages() {
        assert_eq!(parse_progress("PROGRESS:3:10:2048"), Some((3, 10, 2048u64)));
        assert_eq!(parse_progress("PROGRESS:0:0:0"), Some((0, 0, 0)));
    }

    #[test]
    fn rejects_non_progress_messages() {
        assert_eq!(parse_progress("Cleaning: Chrome"), None);
        assert_eq!(parse_progress("PROGRESS:3:10"), None);
        assert_eq!(parse_progress("PROGRESS:a:b:c"), None);
        assert_eq!(parse_progress("PROGRESS:1:2:3:4"), None);
        assert_eq!(parse_progress(""), None);
    }

    #[test]
    fn strips_program_prefix() {
        assert_eq!(program_name("Cleaning: Chrome"), "Chrome");
        assert_eq!(program_name("Chrome"), "Chrome");
    }

    /// A cleaner result as the job gets it back.
    fn result(program: &str, category: &str, bytes: u64, paths: &[(&str, u64)]) -> CleanerResult {
        CleanerResult {
            files: 0,
            folders: 0,
            bytes,
            working: true,
            path: "C:\\cache\\*".into(),
            paths_omitted: 0,
            locked_files: 0,
            locked_bytes: 0,
            locked: Vec::new(),
            denied_files: 0,
            denied_bytes: 0,
            denied: Vec::new(),
            paths: paths
                .iter()
                .map(|(path, bytes)| ClearedPath {
                    path: (*path).into(),
                    removed_bytes: *bytes,
                    removed_files: 1,
                    removed_directories: 0,
                })
                .collect(),
            program: Arc::from(program),
            category: Arc::from(category),
            sub_category: Arc::from(""),
        }
    }

    /// What the results page opens the detail overlay with: the deleted paths of
    /// one program, and how much each of them freed.
    #[test]
    fn deleted_paths_are_kept_per_program() {
        let mut cleared: Vec<Cleared> = Vec::new();
        assert!(fold(
            &mut cleared,
            result(
                "Chrome",
                "Cache",
                300,
                &[("C:\\cache\\a.tmp", 200), ("C:\\cache\\b.tmp", 100)],
            )
        ));
        // A second entry of the same program joins the row instead of adding one.
        assert!(fold(
            &mut cleared,
            result("Chrome", "Logs", 50, &[("C:\\logs\\c.log", 50)]),
        ));
        // And a different program gets a row of its own.
        fold(
            &mut cleared,
            result("Firefox", "Cache", 10, &[("C:\\ff\\x", 10)]),
        );

        assert_eq!(cleared.len(), 2);
        let chrome = &cleared[0];
        assert_eq!(chrome.removed_bytes, 350);
        assert_eq!(
            chrome.affected_categories,
            vec!["Cache".to_string(), "Logs".to_string()],
            "both categories belong to the one row",
        );
        let paths: Vec<(String, u64)> = chrome
            .paths
            .iter()
            .map(|detail| (detail.path.as_string(), detail.removed_bytes))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("C:\\cache\\a.tmp".to_string(), 200),
                ("C:\\cache\\b.tmp".to_string(), 100),
                ("C:\\logs\\c.log".to_string(), 50),
            ],
        );
    }

    /// A cleaner that matched nothing must not leave a row behind, and must not
    /// be counted by the caller either.
    #[test]
    fn a_cleaner_that_removed_nothing_is_not_folded_in() {
        let mut idle = result("Chrome", "Cache", 0, &[]);
        idle.working = false;
        let mut cleared: Vec<Cleared> = Vec::new();
        assert!(!fold(&mut cleared, idle));
        assert!(cleared.is_empty());
    }

    /// A cleaner's own cap and this one both say what they left out, and the two
    /// counts add up: a row that silently lost items in two places is a row whose
    /// list cannot be trusted to be the whole story.
    #[test]
    fn omitted_items_from_the_cleaner_reach_the_row() {
        let mut cleared: Vec<Cleared> = Vec::new();
        let mut capped = result("Chrome", "Cache", 10, &[("C:\\cache\\a.tmp", 10)]);
        capped.paths_omitted = 900;
        fold(&mut cleared, capped);

        assert_eq!(cleared[0].paths.len(), 1, "the one item it did list");
        assert_eq!(cleared[0].paths_omitted, 900);
    }

    /// Past the cap the list keeps the largest paths rather than the first ones:
    /// a truncated list that dropped the biggest deletions would be the wrong
    /// list to show.
    #[test]
    fn the_detail_list_keeps_the_largest_paths_when_it_is_capped() {
        let mut cleared: Vec<Cleared> = Vec::new();
        let paths: Vec<(String, u64)> = (0..=MAX_PATH_DETAILS as u64)
            .map(|bytes| (format!("C:\\cache\\{bytes}"), bytes))
            .collect();
        let paths: Vec<(&str, u64)> = paths
            .iter()
            .map(|(path, bytes)| (path.as_str(), *bytes))
            .collect();
        fold(&mut cleared, result("Chrome", "Cache", 0, &paths));

        assert_eq!(cleared[0].paths.len(), MAX_PATH_DETAILS, "the cap holds");
        assert_eq!(
            cleared[0].paths_omitted, 1,
            "and the one it left out is counted, not hidden",
        );
        let smallest = cleared[0].paths.iter().map(|d| d.removed_bytes).min();
        assert_eq!(
            smallest,
            Some(1),
            "the smallest entry was evicted for the larger one",
        );
        let largest = cleared[0].paths.iter().map(|d| d.removed_bytes).max();
        assert_eq!(
            largest,
            Some(MAX_PATH_DETAILS as u64),
            "the biggest path is never the one dropped",
        );
    }
}
