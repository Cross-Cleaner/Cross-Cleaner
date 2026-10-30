//! Command line surface: the flags, and the selection they describe.
//!
//! Selection is the interesting part and is modelled on what the terminal app
//! can express, because a command line has no popup to open: a category, a
//! subcategory of one, a whole program, or one program narrowed down to some of
//! its categories. The four levels mirror the terminal's two pages — the
//! category grid with its subcategory overlay, and the program page with its
//! per-program category overlay.
//!
//! Names are matched case-insensitively, because nobody types `Cache` reliably
//! on a keyboard with a caps lock on.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Cross Cleaner from the command line",
    long_about = None,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub globals: Globals,

    #[command(subcommand)]
    pub command: Command,
}

/// Options that mean the same thing to every subcommand.
///
/// Kept apart from [`Cli`] so a command can be moved out of the parsed value
/// and still be built against the databases the user asked for.
#[derive(Args, Debug, Clone, Default)]
pub struct Globals {
    /// Disable all built-in custom cleanings.
    #[arg(
        long,
        global = true,
        value_name = "bool",
        default_value_t = false,
        action = ArgAction::Set
    )]
    pub disable_custom: bool,

    /// Disable user scripts (the `Scripts` directory).
    ///
    /// Their rules, registry cleanings and custom cleaners are left out, and
    /// their `on_finished` hooks do not run: a scheduled run that must not send
    /// notifications passes this.
    #[arg(
        long,
        global = true,
        value_name = "bool",
        default_value_t = false,
        action = ArgAction::Set
    )]
    pub no_scripts: bool,

    /// Read the cleaner database from this file instead of the built-in one.
    #[arg(long, global = true, value_name = "path")]
    pub database_path: Option<PathBuf>,

    /// Read the registry database from this file instead of the built-in one.
    #[cfg(windows)]
    #[arg(long, global = true, value_name = "path")]
    pub registry_database_path: Option<PathBuf>,

    /// Never colorize the output. Also honours the NO_COLOR environment
    /// variable and turns off automatically when stdout is not a terminal.
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Clean the selected categories and programs.
    Clean(CleanArgs),
    /// Print what a selection would clean, without deleting anything.
    Plan(PlanArgs),
    /// List the categories and their subcategories.
    Categories(ReportArgs),
    /// List the programs of the selected categories.
    Programs(ProgramsArgs),
    /// Check for, download and install a newer release.
    #[cfg(feature = "self-update")]
    Update(UpdateArgs),
}

/// Output flags for a command that only reports.
#[derive(Args, Debug, Clone, Default)]
pub struct ReportArgs {
    #[command(flatten)]
    pub output: OutputArgs,
}

/// How much of a run to show, and how much of it to keep.
#[derive(Args, Debug, Clone, Default)]
pub struct OutputArgs {
    /// List the deleted path of every program, not just its total. Largest
    /// first.
    #[arg(short, long)]
    pub verbose: bool,

    /// Print nothing on success. Errors still go to stderr.
    #[arg(short, long)]
    pub quiet: bool,

    /// Print the result as a single JSON object on stdout. Implies --quiet for
    /// everything but the JSON itself.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct SelectionArgs {
    /// Clean every category, like the window app's "clean all".
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Category to clean. Repeatable. `Cache` takes the whole category,
    /// `Cache/Browser` only that subcategory, and `Cache/*` the category and
    /// all of its subcategories.
    #[arg(short = 'c', long = "category", value_name = "CATEGORY[/SUBCATEGORY]")]
    pub categories: Vec<String>,

    /// Skip a category even if it is selected. Repeatable.
    #[arg(short = 'x', long = "exclude-category", value_name = "CATEGORY")]
    pub exclude_categories: Vec<String>,

    /// Clean only these programs. Repeatable. `Chrome` takes every category the
    /// program is in, `Chrome=Logs` only that one, and `Chrome=*` the same as
    /// `Chrome`.
    #[arg(short = 'p', long = "program", value_name = "PROGRAM[=CATEGORY]")]
    pub programs: Vec<String>,

    /// Skip a program even if one of its categories is selected. Repeatable.
    #[arg(short = 'e', long = "exclude-program", value_name = "PROGRAM")]
    pub exclude_programs: Vec<String>,
}

impl SelectionArgs {
    /// True when the user named nothing at all, so `clean` has to decide
    /// whether to clean everything or to refuse.
    pub fn is_empty(&self) -> bool {
        !self.all
            && self.categories.is_empty()
            && self.programs.is_empty()
            && self.exclude_categories.is_empty()
            && self.exclude_programs.is_empty()
    }

    /// This selection, widened to every category when nothing was named.
    ///
    /// A listing with no selection answers "what can this clean?", which is
    /// everything; a `clean` with no selection is a mistake and says so instead
    /// (`clean` checks [`Self::is_empty`] itself).
    pub fn everything(&self) -> Self {
        if self.is_empty() {
            Self {
                all: true,
                ..Default::default()
            }
        } else {
            self.clone()
        }
    }
}

#[derive(Args, Debug)]
pub struct CleanArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Walk the selection and report what a real run would free, without deleting
    /// anything.
    ///
    /// The walk is the one the cleaner itself performs, so the size is measured
    /// rather than estimated, and files another program is holding open are
    /// reported separately instead of being promised as free.
    #[arg(long)]
    pub dry_run: bool,

    /// With `--dry-run`, also list the database patterns the selection covers.
    /// Answers "which paths would it look at", where `--dry-run` alone answers
    /// "how much is actually there".
    #[arg(long)]
    pub paths: bool,

    /// Do not ask for confirmation before deleting.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct PlanArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Also print every database path the selection covers, not only the
    /// programs. A single glob can match tens of thousands of paths, so this is
    /// opt-in.
    #[arg(long)]
    pub paths: bool,
}

#[derive(Args, Debug)]
pub struct ProgramsArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Only list programs whose name contains this text.
    #[arg(short = 's', long)]
    pub search: Option<String>,
}

#[cfg(feature = "self-update")]
#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Only report whether a newer release exists, do not install it.
    #[arg(short, long)]
    pub check: bool,

    /// Do not ask for confirmation before replacing the executable.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

impl Command {
    /// A copy of the output flags of whichever subcommand this is.
    ///
    /// Taken by value rather than by reference because the caller moves the
    /// command out of the parsed `Cli` right after reading this, and `update`
    /// carries no output flags of its own.
    pub fn output(&self) -> OutputArgs {
        match self {
            Command::Clean(args) => args.output.clone(),
            Command::Plan(args) => args.output.clone(),
            Command::Categories(args) => args.output.clone(),
            Command::Programs(args) => args.output.clone(),
            #[cfg(feature = "self-update")]
            Command::Update(_) => OutputArgs::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        // Every enum, flag and default is checked by clap itself; a conflict or
        // a duplicate short flag panics here rather than at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn a_selection_parses_into_its_parts() {
        let cli = Cli::try_parse_from([
            "cli",
            "clean",
            "-c",
            "Cache",
            "--category=Logs/Crash",
            "-p",
            "Chrome=Logs",
            "-e",
            "Discord",
        ])
        .expect("the command line parses");
        let Command::Clean(args) = cli.command else {
            panic!("expected a clean command");
        };
        assert_eq!(args.selection.categories, ["Cache", "Logs/Crash"]);
        assert_eq!(args.selection.programs, ["Chrome=Logs"]);
        assert_eq!(args.selection.exclude_programs, ["Discord"]);
        assert!(!args.yes);
    }

    #[test]
    fn a_clean_without_a_selection_is_empty() {
        let cli = Cli::try_parse_from(["cli", "clean"]).expect("parses");
        let Command::Clean(args) = cli.command else {
            panic!("expected a clean command");
        };
        assert!(args.selection.is_empty());
        assert!(
            !Cli::try_parse_from(["cli", "clean", "--all"])
                .map(|cli| match cli.command {
                    Command::Clean(args) => args.selection.is_empty(),
                    _ => unreachable!(),
                })
                .unwrap_or(false)
        );
    }

    #[test]
    fn database_flags_work_before_and_after_the_subcommand() {
        for argv in [
            ["cli", "--database-path", "db.json", "categories"],
            ["cli", "categories", "--database-path", "db.json"],
        ] {
            let cli = Cli::try_parse_from(argv).expect("parses");
            assert_eq!(
                cli.globals.database_path,
                Some(PathBuf::from("db.json")),
                "wrong for {argv:?}"
            );
        }
    }

    #[test]
    fn an_empty_selection_widens_to_everything_only_when_asked() {
        let empty = SelectionArgs::default();
        assert!(empty.is_empty());
        assert!(empty.everything().all);
        // An explicit selection is left alone: `everything` widens a listing, it
        // never widens a clean.
        let narrow = SelectionArgs {
            categories: vec!["Cache".to_string()],
            ..Default::default()
        };
        assert!(!narrow.everything().all);
        assert_eq!(narrow.everything().categories, ["Cache"]);
    }

    #[test]
    fn every_command_reports_through_the_same_flags() {
        let cli = Cli::try_parse_from(["cli", "categories", "--json"]).expect("parses");
        assert!(cli.command.output().json);
    }
}
