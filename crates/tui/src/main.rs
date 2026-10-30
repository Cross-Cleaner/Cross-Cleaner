//! Cross Cleaner, terminal edition.
//!
//! Mirrors the window frontend: the same categories, the same program list, the
//! same cleaning run — rendered with ratatui instead of egui.
//!
//! ```text
//! cargo run -p tui
//! cargo run -p tui -- --database-path=custom_database.json
//! ```

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use appcore::app::AppState;
use clap::{ArgAction, Parser};
use crossterm::cursor;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::CustomCleaner;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui::TuiApp;

/// How long the loop waits for a key before redrawing anyway. Cleaning pushes
/// progress through a channel, so the app must repaint on its own while a run
/// is in flight.
const TICK: Duration = Duration::from_millis(100);

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Disable all built-in custom cleanings.
    /// Example: --disable-custom=true
    #[arg(long, value_name = "bool", default_value_t = false, action = ArgAction::Set)]
    disable_custom: bool,

    /// Specify a custom database file path.
    /// Example: --database-path=custom_database.json
    #[arg(long, value_name = "path")]
    database_path: Option<String>,

    /// Specify a custom registry database file path.
    /// Example: --registry-database-path=custom_database.json
    #[cfg(windows)]
    #[arg(long, value_name = "registry_path")]
    registry_database_path: Option<String>,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    // INFO: the custom cleaners are registered in a global registry, so this
    // has to happen before the database is assembled.
    cleaner::custom_cleaners::register_all();
    appcore::scripts::register_custom_cleaners();

    let custom_database: Arc<[CustomCleaner]> = if args.disable_custom {
        Arc::from(Vec::new())
    } else {
        Arc::from(database::custom_cleaners::get_custom_cleaners())
    };

    // INFO: validate a user-supplied database early; the entries themselves are
    // streamed on demand (see CleanerDatabase::for_each).
    let database = match &args.database_path {
        Some(path) => {
            let database = CleanerDatabase::from_file(path);
            if let Err(e) = database.for_each(|_| {}) {
                eprintln!("Failed to load database from file: {e}");
                std::process::exit(1);
            }
            database
        }
        None => CleanerDatabase::default_source(),
    };
    let database = appcore::scripts::with_rules(database);

    #[cfg(windows)]
    let state = match &args.registry_database_path {
        Some(path) => {
            let registry = RegistryDatabase::from_file(path);
            if let Err(e) = registry.for_each(|_| {}) {
                eprintln!("Failed to load database from file: {e}");
                std::process::exit(1);
            }
            AppState::from_database(
                database,
                appcore::scripts::with_registry_rules(registry),
                custom_database,
            )
        }
        None => AppState::from_database(
            database,
            appcore::scripts::with_registry_rules(RegistryDatabase::default_source()),
            custom_database,
        ),
    };

    #[cfg(not(windows))]
    let state = AppState::from_database(database, custom_database);

    // INFO: `AppState` spawns cleaning jobs with `tokio::spawn`, so the runtime
    // has to outlive the UI loop. `block_on` keeps it alive for the whole run.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut app = TuiApp::new(state);
    // Registers the worker that can replace this executable with the release
    // binary; without it the dialog only offers the release page.
    app.start_self_update();
    // Opens the audio device once. A machine without one (a container, CI) just
    // stays silent — `init` swallows the failure, same as in `desktop`.
    appcore::sounds::init();
    // Before the terminal is taken: from here on nothing may write to stderr.
    tui::app::install_diagnostic_sink();
    runtime.block_on(run(app))
}

/// Owns the terminal setup, so an early error restores it.
///
/// Restoring is deliberately a free function rather than something only `Drop`
/// does, because two paths bypass destructors entirely:
///
/// * a panic — with `panic = "abort"` there is no unwinding at all;
/// * `std::process::exit` — which the self-update worker calls to hand the
///   executable back to `self_replace` and start the new version.
///
/// In both cases the user's shell would otherwise be left in raw mode on the
/// alternate screen, which looks like a broken terminal and needs `reset` to
/// fix. Both paths call [`restore_terminal`] first.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Mouse capture alongside the alternate screen: without it the terminal
        // keeps interpreting clicks itself — selecting text, pasting — and the
        // app never hears about them. It is undone in `restore_terminal`, which
        // runs on the paths that skip this guard's `Drop` too.
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Puts the terminal back the way it was found. Idempotent.
fn restore_terminal() {
    // Best effort: the process is usually about to exit, so a failure here only
    // affects how the shell prompt looks afterwards. Mouse capture first: left
    // on, the terminal would keep sending mouse events to a shell that is no
    // longer reading them, so its own selection stops working.
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
}

/// Installs a panic hook that restores the terminal before the message is
/// printed, so an abort mid-cleaning cannot strand the user.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

async fn run(mut app: TuiApp) -> io::Result<()> {
    let mut guard = TerminalGuard::enter()?;
    // After `enter`, so the hook is only needed while we own the screen.
    install_panic_hook();
    // The self-update worker exits the process directly; it must not skip the
    // restore. Without the `self-update` feature there is no worker, so nothing
    // can bypass `TerminalGuard`.
    #[cfg(feature = "self-update")]
    selfupdate::set_before_exit(restore_terminal);
    let result = event_loop(&mut guard.terminal, &mut app);
    drop(guard);
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut TuiApp,
) -> io::Result<()> {
    loop {
        // Drain the cleaning channels and the version check before drawing, so
        // the frame already shows the newest state.
        app.tick();
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(TICK)? {
            match event::read()? {
                // Key repeats are already filtered by crossterm on Windows;
                // filtering the kind keeps a kitty-protocol release from
                // firing a binding twice.
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    app.on_event(Event::Key(key))
                }
                other => app.on_event(other),
            }
            if app.should_quit {
                return Ok(());
            }
        }
    }
}
