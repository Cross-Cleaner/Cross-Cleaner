//! Cross Cleaner, command line edition.
//!
//! The third frontend, next to the window app and the terminal UI. Same
//! database, same cleaning engine, same results — reached by typing instead of
//! clicking.
//!
//! ```text
//! cli categories                          # what can be cleaned
//! cli programs -c Cache                   # which programs a category covers
//! cli plan -c Cache -c Logs               # what a selection would touch
//! cli clean -c Cache -c Logs              # clean it
//! cli clean -p 'Chrome=Logs' -y           # one program, one of its categories
//! cli clean -a --json                     # everything, as JSON for a script
//! ```
//!
//! Exit codes: `0` success, `1` a failure, `130` interrupted with Ctrl-C.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use appcore::AppState;
use clap::Parser;
use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::CustomCleaner;

use cli::args::{CleanArgs, Cli, Command, Globals};
use cli::term::Ui;
use cli::{report, run, select};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// How long `cli clean` waits for a script's finished hook before exiting.
///
/// Generous enough for a webhook on a slow connection, short enough that a hook
/// hanging on an unreachable host does not turn a finished run into a hang.
const HOOK_GRACE: Duration = Duration::from_secs(10);

/// Runs one subcommand against a freshly built state.
///
/// The state is built per command rather than once up front: `cli update` and
/// `cli --version` must not pay for streaming the databases, and a listing
/// command never starts a run.
fn execute(cli: Cli) -> Result<ExitCode, String> {
    let globals = cli.globals.clone();
    let output = cli.command.output().clone();
    let ui = Ui::new(globals.no_color, output.quiet, output.json);
    // Before any command runs a cleaner, so diagnostics from the libraries never
    // land on top of the progress line.
    ui.install_diagnostics();

    match cli.command {
        Command::Categories(args) => {
            let state = build_state(&globals)?;
            report::categories(&state, &ui, &args.output);
            Ok(ExitCode::SUCCESS)
        }

        Command::Programs(args) => {
            let mut state = build_state(&globals)?;
            select::apply(&mut state, &args.selection.everything())
                .map_err(|error| error.to_string())?;
            report::programs(&mut state, &ui, &args.output, args.search.as_deref());
            Ok(ExitCode::SUCCESS)
        }

        Command::Plan(args) => {
            let mut state = build_state(&globals)?;
            select::apply(&mut state, &args.selection).map_err(|error| error.to_string())?;
            report::dry_run(&state, &ui, &args.output, args.paths);
            Ok(ExitCode::SUCCESS)
        }

        Command::Clean(args) => {
            let result = clean(&ui, &globals, args);

            // Scripts are told about the run on a background thread. A terminal
            // run exits as soon as it has printed its report, so the hooks get a
            // short grace period — a scheduled `cli clean` that posts a
            // notification must not lose it to process exit.
            if !globals.no_scripts && !appcore::scripts::wait_for_hooks(HOOK_GRACE) {
                ui.line("A script's finished hook is still running; leaving it behind.");
            }

            result
        }

        #[cfg(feature = "self-update")]
        Command::Update(args) => {
            cli::update::run(&ui, &args)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `cli clean`, dry run included.
fn clean(ui: &Ui, globals: &Globals, args: CleanArgs) -> Result<ExitCode, String> {
    if args.selection.is_empty() {
        return Err(
            "nothing selected: name a category (-c Cache), a subcategory (-c Cache/Browser), \
             a program (-p Chrome) or pass --all. `cli categories` lists what is available."
                .to_string(),
        );
    }

    let mut state = build_state(globals)?;
    let selection =
        select::apply(&mut state, &args.selection).map_err(|error| error.to_string())?;
    if selection.programs.is_empty() {
        ui.line("Nothing selected after filtering; nothing to clean.");
        return Ok(ExitCode::SUCCESS);
    }

    // INFO: `--dry-run` is answered by the same walk a run performs with the
    // deletions switched off, so it goes through `run::run` rather than a
    // separate branch here: two paths would be two answers.
    //
    // `--paths` asks a different question — which patterns the selection
    // covers — so that one still reads the database plan.
    if args.dry_run && args.paths {
        report::dry_run(&state, ui, &args.output, true);
        return Ok(ExitCode::SUCCESS);
    }

    // INFO: `AppState::start_cleaning` spawns onto the tokio runtime, so one has
    // to be alive for the whole run. Built by hand rather than with
    // `#[tokio::main]`, like `desktop`: the runtime must not be entered before
    // the arguments have been read.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;

    let report = runtime.block_on(run::run(&mut state, ui, &selection, &args));
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        // A run cut short by Ctrl-C is not a success: a script that cleans on a
        // schedule has to be able to tell the two apart.
        ExitCode::from(130)
    })
}

/// The database set every command runs against.
///
/// Built the same way `tui` and `desktop` build theirs, including the order:
/// the custom cleaners have to be registered before they can be listed, and a
/// user-supplied file is validated here rather than at the first entry that
/// fails to parse.
fn build_state(globals: &Globals) -> Result<AppState, String> {
    cleaner::custom_cleaners::register_all();

    // Scripts go in before the custom-cleaner registry is read, or the ones they
    // declare would be missing from the list this build is about to snapshot.
    if !globals.no_scripts {
        appcore::scripts::register_custom_cleaners();
    }

    let custom_database: Arc<[CustomCleaner]> = if globals.disable_custom {
        Arc::from(Vec::new())
    } else {
        Arc::from(database::custom_cleaners::get_custom_cleaners())
    };

    let database = match &globals.database_path {
        Some(path) => {
            let database = CleanerDatabase::from_file(path);
            database
                .for_each(|_| {})
                .map_err(|error| format!("failed to load the cleaner database: {error}"))?;
            database
        }
        None => CleanerDatabase::default_source(),
    };

    // The rules a script declares are entries like any other: layered here, they
    // are counted by the scan, shown in `plan` and cleaned by the job.
    let database = if globals.no_scripts {
        database
    } else {
        appcore::scripts::with_rules(database)
    };

    #[cfg(windows)]
    {
        let registry_database = match &globals.registry_database_path {
            Some(path) => {
                let registry = RegistryDatabase::from_file(path);
                registry
                    .for_each(|_| {})
                    .map_err(|error| format!("failed to load the registry database: {error}"))?;
                registry
            }
            None => RegistryDatabase::default_source(),
        };
        let registry_database = if globals.no_scripts {
            registry_database
        } else {
            appcore::scripts::with_registry_rules(registry_database)
        };
        Ok(AppState::from_database(
            database,
            registry_database,
            custom_database,
        ))
    }

    #[cfg(not(windows))]
    {
        Ok(AppState::from_database(database, custom_database))
    }
}

/// The compile-time reminder that [`Globals`] is a clap `Args` struct: if it
/// ever loses that, the flatten in [`Cli`] stops compiling here first.
const _: fn(&Cli) -> &Globals = |cli| &cli.globals;
