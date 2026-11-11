//! Tests for the terminal app, grouped by what they exercise.
//!
//! The helpers here — an offscreen [`ratatui`] backend, a way to press a key or
//! click a glyph, and the fixtures each group starts from — are shared by all of
//! them, so a test can be moved between files without rewriting its setup.

use std::sync::Arc;
use std::time::Duration;

use appcore::AppConfig;
use appcore::app::Page;
use appcore::updater::UpdateStage;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::{CleanerData, CleanerFlags};
use database::version::ChangelogGroup;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::diagnostics::DIAGNOSTIC_HISTORY;
use super::settings::SETTINGS;
use super::sound::take_played;
use super::*;

mod categories;
mod deleted_paths;
mod diagnostics;
mod layouts;
mod mouse;
mod overlays;
mod pages;
mod update;

fn entry(category: &str, program: &str, sub: &str) -> CleanerData {
    CleanerData {
        path: format!("{category}/{program}").into(),
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

/// Two categories, three programs, one of them in two categories.
fn sample_app() -> TuiApp {
    let entries = vec![
        entry("Cache", "Chrome", "Browser"),
        entry("Cache", "Chrome", "Code"),
        entry("Cache", "Firefox", "Browser"),
        entry("Logs", "Firefox", "App"),
    ];
    let database = CleanerDatabase::from_vec(entries);
    let custom = Arc::from(Vec::new());
    let state = {
        #[cfg(windows)]
        {
            AppState::from_database(database, RegistryDatabase::from_vec(Vec::new()), custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    };
    // The version check hits the network; tests must not wait for it.
    let mut app = TuiApp::new(state);
    app.update_receiver = None;
    app
}

/// Renders one frame into an offscreen backend and returns it as text.
fn draw(app: &mut TuiApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(app: &mut TuiApp, code: KeyCode) {
    app.on_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

/// Clicks at a screen position with `button` held down.
fn click_with(app: &mut TuiApp, button: MouseButton, column: u16, row: u16) {
    app.on_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(button),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }));
}

/// Left-clicks at a screen position.
fn click(app: &mut TuiApp, column: u16, row: u16) {
    click_with(app, MouseButton::Left, column, row);
}

/// Right-clicks at a screen position: the terminal's context menu.
fn right_click(app: &mut TuiApp, column: u16, row: u16) {
    click_with(app, MouseButton::Right, column, row);
}

/// Right-clicks the first `needle` found on the current frame.
fn right_click_on(app: &mut TuiApp, width: u16, height: u16, needle: &str) {
    let screen = draw(app, width, height);
    let (column, row) = position_of(&screen, needle);
    right_click(app, column as u16, row as u16);
}

/// Turns the wheel at a screen position, positive for a downward notch.
fn wheel(app: &mut TuiApp, notches: isize) {
    wheel_at(app, 0, 0, notches)
}

/// [`wheel`] at a screen position, for the targets that are not the whole screen.
fn wheel_at(app: &mut TuiApp, column: u16, row: u16, notches: isize) {
    let kind = if notches > 0 {
        MouseEventKind::ScrollDown
    } else {
        MouseEventKind::ScrollUp
    };
    for _ in 0..notches.abs() {
        app.on_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }
}

/// Draws a frame and clicks the first `needle` found on it.
///
/// Goes through the rendered text rather than through the layout constants,
/// so the test fails if a target and the glyph it belongs to drift apart —
/// which is the failure mode hit testing has, and the reason the targets are
/// registered where the widgets are drawn.
fn click_on(app: &mut TuiApp, width: u16, height: u16, needle: &str) {
    let screen = draw(app, width, height);
    let (column, row) = position_of(&screen, needle);
    click(app, column as u16, row as u16);
}

/// Screen position of the first `needle`, as `(column, row)`.
fn position_of(screen: &str, needle: &str) -> (usize, usize) {
    screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| column_of(line, needle).map(|column| (column, row)))
        .unwrap_or_else(|| panic!("no {needle:?} on screen:\n{screen}"))
}

/// Every checkbox glyph a category cell can show.
const CHECKS: [&str; 3] = ["[x]", "[ ]", "[-]"];

/// True when the text between the box borders is a category grid row.
fn is_grid_row(inner: &str) -> bool {
    !checkbox_columns(inner).is_empty()
}

/// Columns at which a grid row shows checkboxes, left to right.
///
/// Byte offsets are converted to character columns: the focus marker is a
/// three-byte glyph, so raw `match_indices` would report it as a shift.
fn checkbox_columns(inner: &str) -> Vec<usize> {
    CHECKS
        .iter()
        .copied()
        .flat_map(|mark| inner.match_indices(mark))
        .map(|(at, _)| inner[..at].chars().count())
        .collect()
}

/// The category grid rows of a rendered frame, borders stripped.
fn grid_rows(screen: &str) -> Vec<String> {
    screen
        .lines()
        .filter_map(|line| {
            let inner = line.strip_prefix('│')?.strip_suffix('│')?;
            is_grid_row(inner).then(|| inner.to_string())
        })
        .collect()
}

// --- fixtures -------------------------------------------------------

/// Rows whose cells differ in shape: one has subcategories and a marker, the
/// next has none, and the labels have different lengths. That is exactly what
/// used to push the second column out of alignment.
fn uneven_grid_app() -> TuiApp {
    let entries = vec![
        // Row 0, left: label plus a `→ N` marker.
        entry("Cache", "App0", "Browser"),
        // Row 0, right: no subcategories at all, so no marker.
        entry("Logs", "App1", ""),
        // Row 1, left: no marker, long label.
        entry("Documentation and manuals", "App2", ""),
        // Row 1, right: marker plus a two-digit count.
        entry("Backups", "App3", "Alpha"),
        entry("Backups", "App3", "Beta"),
        entry("Backups", "App3", "Gamma"),
        entry("Backups", "App3", "Delta"),
        entry("Backups", "App3", "Epsilon"),
        entry("Backups", "App3", "Zeta"),
        entry("Backups", "App3", "Eta"),
        entry("Backups", "App3", "Theta"),
        entry("Backups", "App3", "Iota"),
        entry("Backups", "App3", "Kappa"),
    ];
    let database = CleanerDatabase::from_vec(entries);
    let custom = Arc::from(Vec::new());
    let state = {
        #[cfg(windows)]
        {
            AppState::from_database(database, RegistryDatabase::from_vec(Vec::new()), custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    };
    let mut app = TuiApp::new(state);
    app.update_receiver = None;
    // Tick two subcategories so the `→ N` marker needs two digits.
    app.state.categories[3].selected.insert(Arc::from("Alpha"));
    app.state.categories[3].selected.insert(Arc::from("Beta"));
    app
}

/// One category holding a single program, so the program list has a row whose
/// program is in exactly one category — the case the marker names instead of
/// counting. `sample_app` cannot do this: Chrome is in two subcategories and
/// Firefox spans two categories.
fn single_category_program() -> TuiApp {
    let entries = vec![
        entry("Cache", "Solo", "Browser"),
        entry("Logs", "Chrome", "App"),
        entry("Logs", "Chrome", "Core"),
    ];
    let database = CleanerDatabase::from_vec(entries);
    let custom = Arc::from(Vec::new());
    let state = {
        #[cfg(windows)]
        {
            AppState::from_database(database, RegistryDatabase::from_vec(Vec::new()), custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    };
    let mut app = TuiApp::new(state);
    app.update_receiver = None;
    app.state.toggle_category(0);
    assert!(
        app.state.build_program_list(),
        "the program list must have been built",
    );
    app.state.current_page = Page::ProgramSelection;
    app
}

/// Two categories that both have subcategories, so both cells show an arrow.
fn mirrored_grid_app() -> TuiApp {
    let entries = vec![
        entry("Cache", "App0", "Browser"),
        entry("Logs", "App1", "App"),
    ];
    let database = CleanerDatabase::from_vec(entries);
    let custom = Arc::from(Vec::new());
    let state = {
        #[cfg(windows)]
        {
            AppState::from_database(database, RegistryDatabase::from_vec(Vec::new()), custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    };
    let mut app = TuiApp::new(state);
    app.update_receiver = None;
    app
}

/// `count` categories, all with two subcategories, so the grid is a real
/// multi-row rectangle.
fn many_categories(count: usize) -> TuiApp {
    let entries: Vec<_> = (0..count)
        .flat_map(|i| {
            [
                entry(&format!("Cat{i}"), &format!("App{i}"), "Alpha"),
                entry(&format!("Cat{i}"), &format!("App{i}"), "Beta"),
            ]
        })
        .collect();
    let database = CleanerDatabase::from_vec(entries);
    let custom = Arc::from(Vec::new());
    let state = {
        #[cfg(windows)]
        {
            AppState::from_database(database, RegistryDatabase::from_vec(Vec::new()), custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    };
    let mut app = TuiApp::new(state);
    app.update_receiver = None;
    app
}

/// One row of the results table, with the paths behind its numbers.
fn cleared(program: &str, bytes: u64, paths: &[(&str, u64)]) -> database::structures::Cleared {
    use database::structures::{Cleared, ClearedPath};
    Cleared {
        program: program.to_string(),
        removed_bytes: bytes,
        removed_files: paths.len() as u64,
        removed_directories: 0,
        affected_categories: vec!["Cache".to_string()],
        category_totals: Vec::new(),
        paths: paths
            .iter()
            .map(|(path, bytes)| ClearedPath {
                path: (*path).into(),
                removed_bytes: *bytes,
                removed_files: 1,
                removed_directories: 0,
            })
            .collect(),
        paths_omitted: 0,
    }
}

/// A finished run: Chrome deleted three paths, Firefox one, and a third
/// program reported a total without saying which path it came from.
fn results_app() -> TuiApp {
    let mut app = sample_app();
    app.state.cleared_data = Some((
        6144,
        4,
        0,
        Arc::from(vec![
            cleared(
                "Chrome",
                4096,
                &[
                    ("C:/cache/a.tmp", 2048),
                    ("C:/cache/b.tmp", 1024),
                    ("C:/cache/c.tmp", 1024),
                ],
            ),
            cleared("Firefox", 2048, &[("C:/ff/x.dat", 2048)]),
            cleared("ShareX", 0, &[]),
        ]),
    ));
    app.state.current_page = Page::Results;
    app
}

/// One results row with its numbers set by hand.
///
/// [`cleared`] derives the counts from the paths it is given, which is fine for
/// a test about the path list and useless for one about sorting: there every
/// column has to be able to disagree with the others.
fn report_row(
    program: &str,
    bytes: u64,
    files: u64,
    dirs: u64,
    categories: &[&str],
    path: &str,
) -> database::structures::Cleared {
    use database::structures::{Cleared, ClearedPath};
    Cleared {
        program: program.to_string(),
        removed_bytes: bytes,
        removed_files: files,
        removed_directories: dirs,
        affected_categories: categories.iter().map(|name| (*name).to_string()).collect(),
        category_totals: Vec::new(),
        paths: vec![ClearedPath {
            path: (*path).into(),
            removed_bytes: bytes,
            removed_files: files,
            removed_directories: 0,
        }],
        paths_omitted: 0,
    }
}

/// A finished run in an order that is neither name nor size, with columns that
/// disagree with one another — so a sort has something to change, and the column
/// it changed cannot be guessed from another one.
///
/// * report order — ShareX, Visual Studio Code, Adobe
/// * size — VS Code first, then the two 1 KB rows, largest first on a tie
/// * files — ShareX has most, VS Code fewest: the reverse of the size order
/// * dirs — VS Code has most, ShareX none at all
/// * categories — `Backups, Cache` / `Cache` / `Logs` reads in a third order
///   again, and none of the three orders is the same as either of the others.
///
/// Each row still carries one path of its own, so the overlay behind `Enter` can
/// be checked against the order the table is drawn in.
fn sortable_results_app() -> TuiApp {
    let mut app = sample_app();
    app.state.cleared_data = Some((
        6144,
        9,
        3,
        Arc::from(vec![
            report_row("ShareX", 1024, 4, 0, &["Logs"], "C:/sharex/a"),
            report_row("Visual Studio Code", 4096, 2, 2, &["Cache"], "C:/vscode/a"),
            report_row("Adobe", 1024, 3, 1, &["Backups", "Cache"], "C:/adobe/a"),
        ]),
    ));
    app.state.current_page = Page::Results;
    app
}

/// A report long enough for the wheel to have somewhere to go, whose columns run
/// opposite ways.
///
/// A notch moves the cursor three rows, so a three-row report sends it straight
/// back to the first of them — and a test that the wheel moves it would then pass
/// or fail for a reason that has nothing to do with the wheel. The counts run
/// against the sizes for the other half of the reason: a fixture whose columns all
/// rank the same way cannot tell one sort from another.
fn scrolling_results_app() -> TuiApp {
    let mut app = sample_app();
    app.state.cleared_data = Some((
        21_504,
        21,
        15,
        Arc::from(
            (0..6)
                .map(|n| {
                    let size = (6 - n) as u64;
                    let count = (n + 1) as u64;
                    report_row(
                        &format!("App{n}"),
                        size * 1024,
                        count,
                        size - 1,
                        &["Cache"],
                        &format!("C:/app{n}"),
                    )
                })
                .collect::<Vec<_>>(),
        ),
    ));
    app.state.current_page = Page::Results;
    app
}

/// A changelog long enough to overflow any overlay.
fn long_changelog() -> Changelog {
    Changelog {
        groups: (0..12)
            .map(|group| ChangelogGroup {
                title: format!("Group {group}"),
                items: (0..4)
                    .map(|item| format!("Change {group}.{item} with some detail"))
                    .collect(),
            })
            .collect(),
        contributors: (0..20).map(|c| format!("Contributor {c}")).collect(),
    }
}

fn release() -> NewRelease {
    NewRelease {
        version: "9.9.9".to_string(),
        url: "https://github.com/Cross-Optimizations/Cross-Cleaner/releases/tag/v9.9.9".to_string(),
        asset_url: Some("https://example.invalid/tui".to_string()),
        asset_size: Some(4096),
    }
}

/// A pending update plus a *live* updater channel.
///
/// The receiver is returned so the caller can keep it alive: a dropped receiver
/// would make every `send` fail, which is a different code path than the real
/// one and would hide bugs in the dialog.
fn app_with_update() -> (TuiApp, std::sync::mpsc::Receiver<UpdaterCommand>) {
    let mut app = sample_app();
    let (tx, rx) = std::sync::mpsc::channel();
    app.updater_tx = Some(tx);
    app.update_release = Some(release());
    (app, rx)
}

/// A version check that has already found `found`, so the first `tick` picks
/// it up. The check sender is dropped on return, which is exactly what the
/// real background thread does after reporting.
///
/// The updater receiver comes back too: an automatic download has to land
/// somewhere, and it has to be a live one or the send fails instead.
fn app_with_check(
    found: Option<NewRelease>,
) -> (TuiApp, std::sync::mpsc::Receiver<UpdaterCommand>) {
    let (mut app, rx) = app_with_update();
    let (tx, check) = std::sync::mpsc::channel();
    tx.send(Ok(found)).expect("the receiver is alive");
    app.update_receiver = Some(check);
    (app, rx)
}

// --- shared assertions -----------------------------------------------

/// Character column at which `needle` starts in `haystack`.
fn column_of(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .find(needle)
        .map(|at| haystack[..at].chars().count())
}

/// The column header: it sits inside the table's top border, so the border line
/// itself carries the labels.
fn header_line<'a>(screen: &'a str, lines: &[&'a str]) -> &'a str {
    lines
        .iter()
        .find(|line| line.starts_with('╭') && line.contains("Program"))
        .copied()
        .unwrap_or_else(|| panic!("no header row:\n{screen}"))
}

/// The top border of the report, which is where the column labels live.
fn report_header(app: &mut TuiApp, width: u16, height: u16) -> (String, usize) {
    let screen = draw(app, width, height);
    let lines: Vec<&str> = screen.lines().collect();
    let header = header_line(&screen, &lines);
    let row = screen
        .lines()
        .position(|line| line == header)
        .expect("the header is a line of the frame");
    (header.to_string(), row)
}

/// The bottom border of the report, which carries the sort chain and the frame's
/// own hints.
fn frame_foot(screen: &str) -> &str {
    screen
        .lines()
        .find(|line| line.starts_with('╰'))
        .unwrap_or_else(|| panic!("the frame has no foot:\n{screen}"))
}

/// Clicks a column label in the report's top border.
///
/// Goes through the drawn label rather than through the layout constants, for
/// the same reason [`click_on`] does: a target and the glyph it belongs to are
/// drawn from the same geometry, and a test that hardcoded the geometry would
/// pass even once they had drifted apart.
fn click_header(app: &mut TuiApp, width: u16, height: u16, label: &str) {
    let (header, row) = report_header(app, width, height);
    let column = column_of(&header, label).unwrap_or_else(|| panic!("no {label:?} in {header:?}"));
    click(app, column as u16, row as u16);
}

/// Screen position of the report's column labels: where the wheel clears the sort.
fn header_at(app: &mut TuiApp, width: u16, height: u16) -> (u16, usize) {
    let (header, row) = report_header(app, width, height);
    let column = column_of(&header, "Program").expect("the header carries the labels");
    (column as u16, row)
}

/// Asserts the drawn report lists `names` top to bottom, in that order.
///
/// An order is not a set of values, so it has to be read off the screen: the row
/// each name was drawn on, compared in sequence. That is the whole claim the sort
/// makes — which line comes first — and it is what a reader of the report sees.
#[track_caller]
fn assert_row_order(screen: &str, names: &[&str]) {
    let rows: Vec<usize> = names
        .iter()
        .map(|name| position_of(screen, name).1)
        .collect();
    assert!(
        rows.windows(2).all(|pair| pair[0] < pair[1]),
        "expected {names:?} top to bottom, found them on rows {rows:?}:\n{screen}",
    );
}

// --- locks that keep process-global state out of each other's way ---

/// Serialises the tests that read or write the sound levels.
///
/// The volumes live in one process-global config, and the harness runs tests
/// in parallel: without this a test that clamps `sound_volume` to zero would
/// silently break another's "full volume" assertion, and the failure would
/// land on whichever test happened to overlap rather than on the cause.
static CONFIG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`CONFIG`] and restores the default volumes on the way out.
///
/// The restore is the point of returning a guard rather than a lock: the
/// next test in line must not inherit the level this one left behind, and a
/// panicking test has to restore it too.
fn config_lock() -> ConfigGuard {
    // The field is never read: the lock exists for its `Drop`, and holding it
    // is what keeps the tests apart.
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    appcore::config::update(|c| *c = AppConfig::default());
    ConfigGuard(guard)
}

struct ConfigGuard(#[expect(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Drop for ConfigGuard {
    fn drop(&mut self) {
        appcore::config::update(|c| *c = AppConfig::default());
    }
}

/// Serialises the tests that share the process-global state a cleaning run
/// touches.
///
/// The harness runs tests in parallel, and two pieces of that state outlive any
/// one test: the queue behind [`take_diagnostics`], which
/// [`TuiApp::tick`](super::TuiApp::tick) drains into a toast — and a toast takes
/// the footer line over whatever the footer was reporting — and the captured
/// sound clips. So the test that drives a real cleaning run, the test that owns
/// the diagnostics sink, and every test that ticks and then reads the footer all
/// take turns here.
static SEQUENTIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`SEQUENTIAL`] for the duration of a test.
///
/// A poisoned lock only means some earlier test panicked; the queue is drained
/// on the way out either way, so there is nothing here worth failing over.
pub(super) fn sequential() -> std::sync::MutexGuard<'static, ()> {
    SEQUENTIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Installs the capture sink for one test and removes it again on drop.
///
/// The sink and the queue behind it are process-global, so this also keeps a
/// panicking test from leaking the sink into the rest of the suite — which is
/// why the diagnostics behaviour is covered by a single test rather than
/// several that would race for the same global.
struct DiagnosticsGuard;

impl DiagnosticsGuard {
    fn install() -> Self {
        install_diagnostic_sink();
        Self
    }
}

impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        remove_diagnostic_sink();
        let _ = take_diagnostics();
    }
}
