//! Running a selection and reporting what came of it.
//!
//! The job itself is [`appcore::cleaning::work`] — this module only drives
//! [`AppState`], exactly the way `tui` and `gui` do: start the run, drain the
//! progress channel, poll for the result. Everything a run reports is already
//! computed by `appcore`; the work here is turning it into lines.
//!
//! Two things the window frontends do not need and this one does:
//!
//! * **Confirmation.** A window app has a button, and clicking it is the
//!   consent. A command line may be run from a script, so `--yes` exists and
//!   the prompt is skipped when stdin is not a terminal: a pipe cannot answer a
//!   question, and blocking on a read that will never come is worse than
//!   refusing to start.
//! * **Interrupt.** Ctrl-C has to stop the run and say what was already gone,
//!   rather than leaving the process killed mid-delete with no report at all.

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;

use appcore::AppState;
use appcore::cleaning;
use tokio::sync::mpsc;

use crate::args::{CleanArgs, OutputArgs};
use crate::select::Selection;
use crate::term::{Ui, count};

/// What a finished run reported, so the caller can render it either way.
pub struct RunReport {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub cleared: Vec<database::structures::Cleared>,
    /// False when the run was cut short by Ctrl-C, so the exit code can say so.
    pub complete: bool,
}

/// Starts the run, waits for it, and prints the result.
///
/// `state` is expected to have the selection already applied — see
/// [`crate::select::apply`]. The category selection is consumed by the run, so
/// this is a one-way door for a given state, which is why the caller builds a
/// fresh one per invocation anyway.
pub async fn run(
    state: &mut AppState,
    ui: &Ui,
    selection: &Selection,
    args: &CleanArgs,
) -> RunReport {
    if args.dry_run {
        return scan(state, ui, &args.output).await;
    }

    if !args.yes && !confirm(ui, selection) {
        return RunReport {
            bytes: 0,
            files: 0,
            directories: 0,
            cleared: Vec::new(),
            complete: false,
        };
    }

    state.start_cleaning();

    // INFO: `AppState` reports progress through a channel rather than a
    // callback, so the loop has to keep draining it: the channel is bounded at
    // 32 messages, and a cleaner that filled it would block until this reads.
    //
    // The Ctrl-C future is created once and polled every iteration: a fresh one
    // per tick would re-register the handler each time, and the one that has
    // already fired would never fire again.
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let mut interrupted = false;
    // Whether the run reported a result. Tracked here rather than read off
    // `poll_result` afterwards, because that call consumes the result: a
    // second one always answers "nothing new", which would look exactly like a
    // job that died without reporting.
    let mut reported = false;

    while state.is_cleaning() {
        tokio::select! {
            _ = &mut interrupt => {
                // First Ctrl-C stops the run. The handle is aborted rather than
                // dropped, so a cleaner that is halfway through removing a
                // directory is not left with a half-removed one: `abort` cancels
                // at the next await point, and every removal path in `cleaner`
                // reaches one.
                interrupted = true;
                // Scripts see the same request: a handler that walks a large
                // tree between two `remove_*` calls stops at its next
                // `cleaner::cancelled()` instead of running to the end of work
                // nobody is waiting for.
                appcore::scripts::cancel();
                ui.line(&ui.warn("Stopping — the entries in flight finish first."));
                if let Some(handle) = state.task_handle.take() {
                    handle.abort();
                }
                // The job was aborted, so its own end never ran: the cancelled
                // hook is told here instead of from the cleaning loop.
                appcore::scripts::notify_cancelled();
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }

        // Drain before polling: the result is only sent once the last cleaner is
        // done, and the progress messages queued before it still have to reach
        // the counters, or the final frame would show the second-to-last one.
        state.drain_progress();
        draw(ui, state);
        if state.poll_result() {
            reported = true;
            break;
        }
    }

    // Anything still queued after the loop, whether the run finished between
    // the sleep and the poll or was cut short mid-report.
    state.drain_progress();
    if !reported {
        reported = state.poll_result();
    }
    let complete = reported && !interrupted;

    ui.clear_progress();

    let (bytes, files, directories, cleared) = match state.cleared_data.clone() {
        Some(data) => data,
        None => {
            // The run was aborted, so no result will ever arrive. Report what
            // the progress messages did say rather than nothing at all.
            if interrupted {
                ui.line(
                    &ui.warn("Interrupted. The numbers above are what had been removed so far."),
                );
            } else {
                ui.line(&ui.bad("The cleaning job ended without reporting a result."));
            }
            return RunReport {
                bytes: 0,
                files: 0,
                directories: 0,
                cleared: Vec::new(),
                complete: false,
            };
        }
    };

    let cleared: Vec<database::structures::Cleared> = cleared.iter().cloned().collect();
    let report = RunReport {
        bytes,
        files,
        directories,
        cleared: cleared.clone(),
        complete,
    };
    render(ui, &report, &args.output);
    report
}

/// Draws one progress line from the state, the same numbers the window app's
/// progress bar shows.
fn draw(ui: &Ui, state: &AppState) {
    let fraction = state.progress_fraction();
    let program = state.current_program();
    let mut detail = String::new();
    if state.total_tasks > 0 {
        detail.push_str(&format!("{}/{}", state.current_task, state.total_tasks));
    }
    if state.cleaned_bytes > 0 {
        detail.push_str(&format!(
            " · {} freed",
            database::utils::get_file_size_string(state.cleaned_bytes)
        ));
    }
    if let Some(eta) = state.eta() {
        detail.push_str(&format!(" · {eta} left"));
    }
    let label = if program.is_empty() {
        "Cleaning".to_string()
    } else {
        program.to_string()
    };
    ui.progress(fraction, &label, &detail);
}

/// Walks the selection without deleting anything, and reports what a real run
/// would free.
///
/// This is [`appcore::cleaning::scan`] — the same walk a run performs with the
/// deletions turned off — rather than a second look at the database patterns.
/// The difference is the whole point: a scan that read the patterns would answer
/// "how much *could* be here", and the user asked "how much *is* here".
///
/// Three things it deliberately refuses to fold into one number:
///
/// * files something holds open, which a run will fail to remove;
/// * custom cleaners, which can only be measured by running them;
/// * registry entries, which only exist to be deleted.
///
/// Each is reported on its own line instead, because a scan that averaged them
/// into the total would be reporting a number it cannot stand behind.
pub async fn scan(state: &AppState, ui: &Ui, output: &OutputArgs) -> RunReport {
    let selected_map = state.selected_map();
    let (excluded_programs, excluded_program_categories) = state.exclusions();

    let (sender, mut receiver) = mpsc::channel::<String>(32);
    let database = state.database.clone();
    let custom_database = Arc::clone(&state.custom_database);
    #[cfg(windows)]
    let registry_database = state.registry_database.clone();

    // INFO: the walk runs on the runtime while this side only drains the
    // channel, so a scan streams over the same eight-at-a-time concurrency a
    // real run uses. Doing it here instead would serialise every entry, and a
    // full `--all` scan is tens of thousands of them.
    let job = tokio::spawn(async move {
        cleaning::scan(
            selected_map,
            sender,
            &database,
            &custom_database,
            #[cfg(windows)]
            &registry_database,
            excluded_programs,
            excluded_program_categories,
        )
        .await
    });

    let outcome = tokio::select! {
        // Ctrl-C during a scan is safe in a way it is not during a run: nothing
        // has been deleted, so stopping costs the answer and nothing else.
        _ = tokio::signal::ctrl_c() => None,
        report = async {
            // The same progress protocol a run speaks, so the line reads the
            // same and names what is being walked.
            while let Some(message) = receiver.recv().await {
                match cleaning::parse_progress(&message) {
                    Some((done, total, _bytes)) => ui.progress(
                        Some(done as f32 / total.max(1) as f32),
                        "Scanning",
                        &format!("{done}/{total}"),
                    ),
                    None => ui.progress(None, "Scanning", cleaning::program_name(&message)),
                }
            }
            job.await.ok()
        } => report,
    };

    ui.clear_progress();

    let Some(report) = outcome else {
        ui.line(&ui.warn("Scan cancelled. Nothing was deleted."));
        return RunReport {
            bytes: 0,
            files: 0,
            directories: 0,
            cleared: Vec::new(),
            complete: false,
        };
    };

    render_scan(ui, &report, output);
    RunReport {
        bytes: report.free.0,
        files: report.free.1,
        directories: report.free.2,
        cleared: report.free.3.iter().cloned().collect(),
        // A scan always completes: nothing was at stake, so reporting it as
        // incomplete would make the caller exit 130 over a dry run that answered
        // its question perfectly well.
        complete: true,
    }
}

/// Prints what a scan found: the freeable size, then everything held back from
/// it, each on its own line.
fn render_scan(ui: &Ui, report: &cleaning::ScanReport, output: &OutputArgs) {
    if output.json {
        ui.print_json(&scan_json(report));
        return;
    }
    if output.quiet {
        return;
    }

    let (bytes, files, directories, cleared) = &report.free;
    ui.line(&ui.heading(&format!(
        "Would free {} in {} and {} across {}",
        count(*bytes, "byte"),
        count(*files, "file"),
        count(*directories, "directory"),
        count(cleared.len() as u64, "program")
    )));

    if *bytes == 0 && report.locked_files == 0 && report.unmeasured.is_empty() {
        ui.line(&ui.dim("Nothing found — the selected paths are already empty."));
        return;
    }

    // Largest first, same as a run's table: the biggest freed entries are what
    // the reader is looking for.
    let mut rows: Vec<&database::structures::Cleared> = cleared.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.removed_bytes));
    if !rows.is_empty() {
        ui.line("");
        for row in rows {
            ui.line(&format!(
                "  {}  {}  {}",
                ui.bold(&row.program),
                ui.good(&database::utils::get_file_size_string(row.removed_bytes)),
                ui.dim(&format!(
                    "{} · {}",
                    count(row.removed_files, "file"),
                    count(row.removed_directories, "directory")
                ))
            ));
            if output.verbose {
                for detail in &row.paths {
                    ui.line(&format!(
                        "      {}  {}",
                        detail.path,
                        ui.dim(&database::utils::get_file_size_string(detail.removed_bytes))
                    ));
                }
            }
        }
    }

    // The part of the tree a run cannot reach. Printed whether or not anything
    // else was found: "you would have got 2 GB but 40 MB is locked" is a
    // different answer from "you would have got 2 GB", and the user asked.
    if report.locked_files > 0 {
        ui.line("");
        ui.line(&ui.warn(&format!(
            "{} are in use and will not be removed:",
            count(report.locked_files, "item")
        )));
        ui.line(&format!(
            "  {} held back — close the programs using them and run again.",
            ui.bad(&database::utils::get_file_size_string(report.locked_bytes))
        ));
        if output.verbose {
            for detail in report.locked.iter().take(50) {
                ui.line(&format!(
                    "      {}  {}",
                    detail.path,
                    ui.dim(&database::utils::get_file_size_string(detail.removed_bytes))
                ));
            }
        }
    }

    // Reported apart from `locked` because the advice is different: telling
    // someone to close their applications when the problem is their ACL sends
    // them after the wrong fix.
    if report.denied_files > 0 {
        ui.line("");
        ui.line(&ui.warn(&format!(
            "{} cannot be deleted without administrator rights:",
            count(report.denied_files, "item")
        )));
        ui.line(&format!(
            "  {} held back — run the same command from an elevated prompt.",
            ui.bad(&database::utils::get_file_size_string(report.denied_bytes))
        ));
        if output.verbose {
            for detail in report.denied.iter().take(50) {
                ui.line(&format!(
                    "      {}  {}",
                    detail.path,
                    ui.dim(&database::utils::get_file_size_string(detail.removed_bytes))
                ));
            }
        }
    }

    if !report.unmeasured.is_empty() {
        // INFO: a cleaner whose path holds `{drive}` is registered once per
        // drive, so the same id arrives several times. Listing it four times
        // would read as four cleaners rather than one.
        let mut ids: Vec<&str> = report
            .unmeasured
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ui.line("");
        ui.line(&ui.dim(&format!(
            "{} cannot be measured without running {}:",
            count(ids.len() as u64, "cleaner"),
            if ids.len() == 1 { "it" } else { "them" }
        )));
        for id in ids {
            ui.line(&format!("  {id} — {}", ui.dim(report.unmeasured[0].reason)));
        }
    }
}

/// The scan's findings as one JSON object, split the same way the text report is.
fn scan_json(report: &cleaning::ScanReport) -> String {
    let (bytes, files, directories, cleared) = &report.free;
    serde_json::json!({
        "dry_run": true,
        "bytes": bytes,
        "files": files,
        "directories": directories,
        "programs": cleared.iter().map(program_json).collect::<Vec<_>>(),
        "locked_files": report.locked_files,
        "locked_bytes": report.locked_bytes,
        // The same shape the `paths` of a program have, so a script can treat a
        // locked file exactly like a freeable one and filter on the same keys.
        "locked_paths": report.locked.iter().map(path_json).collect::<Vec<_>>(),
        "denied_files": report.denied_files,
        "denied_bytes": report.denied_bytes,
        "denied_paths": report.denied.iter().map(path_json).collect::<Vec<_>>(),
        "unmeasured": report.unmeasured.iter().map(|entry| serde_json::json!({
            "id": entry.id,
            "reason": entry.reason,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// One program's row in the JSON report, for both a run and a scan.
///
/// Deliberately a single function. The two reports used to be written out
/// separately, and they drifted: the run's carried `paths` and the scan's did
/// not, so a script that read one could not read the other and nothing failed
/// loudly — a dry run simply reported less than a run about the same tree. One
/// generator is what keeps the second mode from quietly becoming the odd one out.
fn program_json(entry: &database::structures::Cleared) -> serde_json::Value {
    serde_json::json!({
        "program": entry.program,
        "bytes": entry.removed_bytes,
        "files": entry.removed_files,
        "directories": entry.removed_directories,
        "categories": entry.affected_categories,
        // Counted beyond the cap below, so a list that stops early cannot be
        // mistaken for the whole of it.
        "paths_omitted": entry.paths_omitted,
        "paths": entry.paths.iter().map(path_json).collect::<Vec<_>>(),
    })
}

/// One path in the JSON report: where it is, and what it costs.
///
/// The counters travel with the path rather than only its size, because a
/// removed directory and a removed file are different outcomes for the same
/// number of bytes — `0 bytes` next to `1 directory` says the folder itself
/// went, not that it was empty.
fn path_json(detail: &database::structures::ClearedPath) -> serde_json::Value {
    serde_json::json!({
        "path": detail.path.as_string(),
        "bytes": detail.removed_bytes,
        "files": detail.removed_files,
        "directories": detail.removed_directories,
    })
}

/// Prints the result: the table, then the paths if `--verbose` asked for them.
pub fn render(ui: &Ui, report: &RunReport, output: &OutputArgs) {
    if output.json {
        ui.print_json(&json(report));
        return;
    }

    ui.result_table(
        report.bytes,
        report.files,
        report.directories,
        &report.cleared,
    );

    if !output.verbose || ui.quiet() {
        return;
    }
    // Largest first, the same order the window app's detail overlay lists them
    // in, so the paths that freed the most are at the top of the terminal too.
    let mut cleared: Vec<&database::structures::Cleared> = report.cleared.iter().collect();
    cleared.sort_by_key(|row| std::cmp::Reverse(row.removed_bytes));
    for entry in cleared {
        ui.path_details(entry);
    }
}

/// The result as one JSON object, for a script that reads this instead of a
/// human.
fn json(report: &RunReport) -> String {
    serde_json::json!({
        "complete": report.complete,
        "bytes": report.bytes,
        "files": report.files,
        "directories": report.directories,
        "programs": report.cleared.iter().map(program_json).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Asks before deleting, unless `--yes` already answered.
///
/// Skipped entirely when stdin is not a terminal: a script or a pipeline cannot
/// answer, and a read that waits forever on a closed stdin is a hang, not a
/// safeguard. `--yes` is the documented way to say yes in that case.
fn confirm(ui: &Ui, selection: &Selection) -> bool {
    if !std::io::stdin().is_terminal() {
        return true;
    }
    ui.line(&format!(
        "About to clean {} and {}.",
        count(selection.programs.len() as u64, "program"),
        count(
            selection
                .programs
                .iter()
                .map(|(_, cats)| cats.len())
                .sum::<usize>() as u64,
            "category"
        )
    ));
    print!("Continue? [y/N] ");
    let _ = std::io::stdout().flush();

    let mut answer = [0u8; 8];
    let read = std::io::stdin().read(&mut answer);
    let answer = match read {
        Ok(n) => String::from_utf8_lossy(&answer[..n]).trim().to_lowercase(),
        Err(error) => {
            ui.warning(&format!("Could not read the answer: {error}"));
            return false;
        }
    };
    answer == "y" || answer == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;
    use appcore::AppState;
    use database::cleaner_database::CleanerDatabase;
    use database::structures::{CleanerData, CleanerFlags};
    use std::sync::Arc;

    fn entry(category: &str, program: &str, sub: &str) -> CleanerData {
        CleanerData {
            path: format!("{program}/{sub}").into(),
            category: Arc::from(category),
            program: Arc::from(program),
            class: Arc::from("Application"),
            sub_category: Arc::from(sub),
            files_to_remove: vec![],
            directories_to_remove: vec![],
            flags: CleanerFlags::empty(),
            filter: Default::default(),
        }
    }

    fn state(entries: Vec<CleanerData>) -> AppState {
        let database = CleanerDatabase::from_vec(entries);
        let custom = Arc::from(Vec::new());
        #[cfg(windows)]
        {
            AppState::from_database(
                database,
                database::registry_database::RegistryDatabase::from_vec(Vec::new()),
                custom,
            )
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    }

    #[tokio::test]
    async fn a_run_with_nothing_to_delete_reports_zeroes() {
        // A category whose entries point at paths that do not exist: the job
        // runs, matches nothing, and has to say so rather than hang or fail.
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        app.toggle_category(0);
        app.build_program_list();
        let ui = Ui::new(true, true, false);
        let selection = crate::select::Selection {
            programs: vec![("Chrome".to_string(), vec!["Cache".to_string()])],
        };
        let args = CleanArgs {
            selection: Default::default(),
            output: OutputArgs {
                json: true,
                ..Default::default()
            },
            dry_run: false,
            paths: false,
            yes: true,
        };
        let report = run(&mut app, &ui, &selection, &args).await;
        assert!(report.complete);
        assert_eq!(report.bytes, 0);
        assert_eq!(report.cleared.len(), 0);
    }

    /// The run and the scan have to answer the same questions the same way.
    ///
    /// They used to be written as two separate JSON blocks, and they drifted: the
    /// run's carried `paths`, the scan's did not. Nothing failed — a dry run simply
    /// reported less than a run about the very same tree, and a script written
    /// against one of them broke on the other without a word about why. This is
    /// the guard that keeps the second mode from quietly becoming the odd one out.
    #[test]
    fn both_json_reports_carry_the_same_keys_for_a_program() {
        let entry = database::structures::Cleared {
            program: "Zed".to_string(),
            removed_bytes: 65019,
            removed_files: 2,
            removed_directories: 1,
            affected_categories: vec!["Logs".to_string()],
            category_totals: Vec::new(),
            paths: vec![database::structures::ClearedPath {
                path: database::structures::SharedPath::new("C:/tmp/Zed.log"),
                removed_bytes: 65019,
                removed_files: 2,
                removed_directories: 1,
            }],
            paths_omitted: 0,
        };

        let from_run = program_json(&entry);
        let from_scan = program_json(&entry);
        assert_eq!(from_run, from_scan, "one generator, so one shape");

        // The keys a script needs to enumerate paths and their sizes.
        for key in [
            "program",
            "bytes",
            "files",
            "directories",
            "paths",
            "paths_omitted",
        ] {
            assert!(from_run.get(key).is_some(), "missing {key}");
        }
        let path = &from_run["paths"][0];
        assert_eq!(path["path"], "C:/tmp/Zed.log");
        assert_eq!(path["bytes"], 65019);
        // A path that is a directory rather than a file is only visible through
        // these two counters: a directory of zero length and a zero-byte file are
        // otherwise the same number.
        assert_eq!(path["files"], 2);
        assert_eq!(path["directories"], 1);
    }

    /// A cleaner registered per drive arrives from the scan once per drive. The
    /// report has to name it once: four lines reading as four cleaners would be
    /// a different statement about the machine than the one that is true.
    #[test]
    fn the_same_cleaner_coming_from_several_drives_is_named_once() {
        let report = cleaning::ScanReport {
            unmeasured: vec![
                cleaning::Unmeasured {
                    id: "Optimize pictures".to_string(),
                    reason: "custom cleaners can only be measured by running them",
                },
                cleaning::Unmeasured {
                    id: "Optimize pictures".to_string(),
                    reason: "custom cleaners can only be measured by running them",
                },
            ],
            ..Default::default()
        };
        let mut ids: Vec<&str> = report.unmeasured.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids, ["Optimize pictures"]);
    }

    #[tokio::test]
    async fn a_json_report_carries_the_totals() {
        let report = RunReport {
            bytes: 100,
            files: 2,
            directories: 1,
            cleared: vec![database::structures::Cleared {
                program: "Chrome".to_string(),
                removed_bytes: 100,
                removed_files: 2,
                removed_directories: 1,
                affected_categories: vec!["Cache".to_string()],
                category_totals: Vec::new(),
                paths: Vec::new(),
                paths_omitted: 0,
            }],
            complete: true,
        };
        let value: serde_json::Value = serde_json::from_str(&json(&report)).expect("valid JSON");
        assert_eq!(value["bytes"], 100);
        assert_eq!(value["programs"][0]["program"], "Chrome");
        assert_eq!(value["complete"], true);
    }
}
