//! Turns the names typed on the command line into ticks on an [`AppState`].
//!
//! Everything here is a lookup and a set membership test: it resolves
//! `Cache/Browser` and `Chrome=Logs` against the lists `AppState` already
//! built, and reports what it could not resolve. It never invents a name, so a
//! typo is an error with the real names next to it rather than a run that
//! quietly cleans something else.
//!
//! The rules it writes into are `AppState`'s own — a category tick selects the
//! same subcategories the terminal app's checkbox selects, and a program tick
//! clears the same per-category exclusions its popup clears.

use std::sync::Arc;

use appcore::AppState;
use appcore::categories::CategoryState;

use crate::args::SelectionArgs;

/// A name that matched nothing, with the candidates that would have.
#[derive(Debug, PartialEq, Eq)]
pub struct Unresolved {
    pub what: &'static str,
    pub name: String,
    /// Real names close to `name`, so the message can offer one.
    pub suggestions: Vec<String>,
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown {} `{}`", self.what, self.name)?;
        match self.suggestions.len() {
            0 => Ok(()),
            1 => write!(f, ", did you mean `{}`?", self.suggestions[0]),
            _ => write!(f, ", did you mean one of: {}?", self.suggestions.join(", ")),
        }
    }
}

/// One program of the program page, reduced to what a selection cares about.
#[derive(Clone)]
struct ProgramMatch {
    index: usize,
    name: Arc<str>,
}

/// A resolved selection, ready to be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Programs that will be cleaned, with the categories left enabled for
    /// each. Only used for reporting — the job reads the tick marks on the
    /// state itself.
    pub programs: Vec<(String, Vec<String>)>,
}

/// Applies `selection` to `state`, so `state` is ready for
/// [`AppState::start_cleaning`].
///
/// Categories first, then the program page: the list of programs only exists
/// once the categories decide which programs are in scope at all, so a program
/// is resolved against the programs its category actually offers.
pub fn apply(state: &mut AppState, selection: &SelectionArgs) -> Result<Selection, Unresolved> {
    select_categories(state, selection)?;
    state.build_program_list();
    let programs = select_programs(state, selection)?;

    Ok(Selection { programs })
}

/// Ticks the requested categories and subcategories.
fn select_categories(state: &mut AppState, selection: &SelectionArgs) -> Result<(), Unresolved> {
    // `--all` first, so an explicit `--category` narrows it rather than being
    // wiped by it: `--all -c Cache` reads as "everything except everything but
    // Cache", which is what the flags say.
    if selection.all {
        for category in &mut state.categories {
            select_all_subcategories(category);
        }
    }

    for name in &selection.categories {
        select_category(state, name)?;
    }

    // Exclusions run last so they win over both `--all` and `--category`.
    for name in &selection.exclude_categories {
        let (category, sub) = split_category(name);
        let index = find_category(state, category)?;
        match sub {
            None => state.categories[index].selected.clear(),
            Some(sub) => {
                // Only an exclusion: a subcategory that is not there is not an
                // error here, because excluding something absent is a no-op the
                // user can afford to be wrong about.
                if let Some(sub) = resolve_sub(&state.categories[index], sub, false)? {
                    state.categories[index].selected.remove(&sub);
                }
            }
        }
    }

    Ok(())
}

/// Ticks one category, or one subcategory of it.
fn select_category(state: &mut AppState, name: &str) -> Result<(), Unresolved> {
    let (category, sub) = split_category(name);
    let index = find_category(state, category)?;
    match sub {
        // `Cache` and `Cache/*` mean the same thing: the category is a unit, so
        // ticking it takes its subcategories with it, exactly as the terminal
        // app's category checkbox does.
        None => select_all_subcategories(&mut state.categories[index]),
        Some(sub) => {
            let sub = resolve_sub(&state.categories[index], sub, true)?.expect(
                "resolve_sub only returns None for a subcategory that does not \
                 exist, and that is an error when it was asked for explicitly",
            );
            state.categories[index].selected.insert(sub);
        }
    }
    Ok(())
}

/// Ticks every subcategory of a category, plus the empty pseudo-subcategory
/// when it has one.
fn select_all_subcategories(category: &mut CategoryState) {
    category.selected = category.subs.iter().cloned().collect();
    if category.has_empty {
        category.selected.insert(Arc::from(""));
    }
}

/// Resolves the program page: which programs stay ticked, and which of their
/// categories stay enabled.
fn select_programs(
    state: &mut AppState,
    selection: &SelectionArgs,
) -> Result<Vec<(String, Vec<String>)>, Unresolved> {
    let all: Vec<ProgramMatch> = state
        .program_checkboxes
        .iter()
        .enumerate()
        .map(|(index, (_, name))| ProgramMatch {
            index,
            name: Arc::clone(name),
        })
        .collect();

    // `--program` names the programs that stay. Anything not named goes off,
    // and an unknown name is an error: silently cleaning a different program
    // than the one asked for is worse than refusing to start.
    let wanted: Vec<ProgramMatch> = if selection.programs.is_empty() {
        all.clone()
    } else {
        let mut wanted: Vec<ProgramMatch> = Vec::new();
        for name in &selection.programs {
            let (program, category) = split_program(name);
            let Some(found) = find_program(&all, program) else {
                return Err(unknown(
                    "program",
                    program,
                    all.iter().map(|p| p.name.as_ref()).collect(),
                ));
            };
            // `Chrome=Logs` narrows the program to one of its categories, which
            // is what the terminal app's per-program overlay does.
            if let Some(category) = category
                // `Chrome=*` is `Chrome`: naming every category is what the
                // program already is, so the narrowing is a no-op rather than a
                // lookup for a category literally called `*`.
                && category != "*"
            {
                let categories = state
                    .program_categories
                    .get(found.index)
                    .cloned()
                    .unwrap_or_default();
                let Some(index) = find_category_name(&categories, category) else {
                    return Err(unknown(
                        "category of this program",
                        &format!("{program}={category}"),
                        categories.iter().map(|c| c.as_ref()).collect(),
                    ));
                };
                let keep = Arc::clone(&categories[index]);
                if let Some(disabled) = state.program_disabled.get_mut(found.index) {
                    // Cleared first: `--program Chrome=Logs --program Chrome=Cache`
                    // means the last one wins, rather than leaving Logs disabled
                    // by the first and Cache by the second.
                    disabled.clear();
                    for cat in &categories {
                        if cat != &keep {
                            disabled.insert(Arc::clone(cat));
                        }
                    }
                }
            }
            if !wanted.iter().any(|p| p.index == found.index) {
                wanted.push(found.clone());
            }
        }
        wanted
    };

    for program in &all {
        let keep = wanted.iter().any(|p| p.index == program.index);
        if !keep && *state.program_checkboxes[program.index].0.borrow() {
            state.toggle_program(program.index);
        }
    }

    // `--exclude-program` runs last, so it wins over `--program` too.
    for name in &selection.exclude_programs {
        let Some(found) = find_program(&all, name) else {
            return Err(unknown(
                "program",
                name,
                all.iter().map(|p| p.name.as_ref()).collect(),
            ));
        };
        if *state.program_checkboxes[found.index].0.borrow() {
            state.toggle_program(found.index);
        }
    }

    // What is left, for the report. A program whose every category ended up
    // excluded is dropped: nothing would be cleaned through it, so listing it
    // would claim work that never happens.
    Ok((0..state.program_checkboxes.len())
        .filter(|&index| {
            *state.program_checkboxes[index].0.borrow() && is_program_enabled(state, index)
        })
        .map(|index| {
            let name = state.program_checkboxes[index].1.to_string();
            let categories = enabled_categories(state, index);
            (name, categories)
        })
        .collect())
}

/// The categories a program still has enabled, in the order the program page
/// lists them.
fn enabled_categories(state: &AppState, index: usize) -> Vec<String> {
    let Some(categories) = state.program_categories.get(index) else {
        return Vec::new();
    };
    let disabled = state.program_disabled.get(index);
    categories
        .iter()
        .filter(|category| !disabled.is_some_and(|set| set.contains(*category)))
        .map(|category| category.to_string())
        .collect()
}

/// True while at least one of the program's categories is still enabled.
///
/// A program the state does not know any category for counts as enabled: the
/// job filters on categories rather than on this, so reporting it as off would
/// hide work that is about to happen.
fn is_program_enabled(state: &AppState, index: usize) -> bool {
    let Some(categories) = state.program_categories.get(index) else {
        return true;
    };
    let Some(disabled) = state.program_disabled.get(index) else {
        return true;
    };
    categories
        .iter()
        .any(|category| !disabled.contains(category))
}

/// Splits `Cache/Browser` into `("Cache", Some("Browser"))`, and `Cache` into
/// `("Cache", None)`.
fn split_category(name: &str) -> (&str, Option<&str>) {
    match name.split_once('/') {
        Some((category, sub)) => (category, Some(sub.trim())),
        None => (name.trim(), None),
    }
}

/// Splits `Chrome=Logs` into `("Chrome", Some("Logs"))`, and `Chrome` into
/// `("Chrome", None)`.
fn split_program(name: &str) -> (&str, Option<&str>) {
    match name.split_once('=') {
        Some((program, category)) => (program.trim(), Some(category.trim())),
        None => (name.trim(), None),
    }
}

/// Index of the category named `name`.
fn find_category(state: &AppState, name: &str) -> Result<usize, Unresolved> {
    state
        .categories
        .iter()
        .position(|category| category.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            unknown(
                "category",
                name,
                state.categories.iter().map(|c| c.name.as_ref()).collect(),
            )
        })
}

/// Resolves a subcategory of `category` to the exact `Arc<str>` the selection
/// set is keyed by.
///
/// `""` and `Uncategorized` both address the pseudo-subcategory that holds
/// entries with no `sub_category`. A name that is not there is an error when it
/// was asked for and `None` when it only appeared in an exclusion.
fn resolve_sub(
    category: &CategoryState,
    name: &str,
    strict: bool,
) -> Result<Option<Arc<str>>, Unresolved> {
    let uncategorized = name.is_empty() || name.eq_ignore_ascii_case("uncategorized");
    if uncategorized {
        return Ok(category.has_empty.then(|| Arc::from("")));
    }
    // `*` is the whole category, which the caller already does when no
    // subcategory was named; spelling it out is a no-op rather than an error.
    if name == "*" {
        return Ok(None);
    }
    match category
        .subs
        .iter()
        .find(|sub| sub.eq_ignore_ascii_case(name))
        .cloned()
    {
        Some(sub) => Ok(Some(sub)),
        None if strict => {
            // Every subcategory is listed, not just the close ones: a category
            // has a handful of them, so the whole list is short enough to read
            // and the prefix ranking would leave a mistyped name with none.
            let mut names = uncategorized_names(category);
            names.sort();
            Err(Unresolved {
                what: "subcategory",
                name: name.to_string(),
                suggestions: names,
            })
        }
        None => Ok(None),
    }
}

/// The names a subcategory of `category` can be addressed by, including the
/// pseudo-subcategory, so an error lists everything that was on offer.
fn uncategorized_names(category: &CategoryState) -> Vec<String> {
    let mut names: Vec<String> = category.subs.iter().map(|sub| sub.to_string()).collect();
    if category.has_empty {
        names.push("Uncategorized".to_string());
    }
    names
}

/// Index of the program named `name`.
fn find_program<'a>(programs: &'a [ProgramMatch], name: &str) -> Option<&'a ProgramMatch> {
    programs
        .iter()
        .find(|program| program.name.eq_ignore_ascii_case(name))
}

/// Index of the category named `name` in a list of category names.
fn find_category_name(categories: &[Arc<str>], name: &str) -> Option<usize> {
    categories
        .iter()
        .position(|category| category.eq_ignore_ascii_case(name))
}

/// An [`Unresolved`] carrying the closest real names, so a mistyped flag says
/// what it could have been.
///
/// The candidates arrive as anything that derefs to a `&str`, so a caller can
/// pass an owned list or borrow straight out of the state.
fn unknown<S: AsRef<str>>(what: &'static str, name: &str, candidates: Vec<S>) -> Unresolved {
    let needle = name.to_lowercase();
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|candidate| {
            let candidate = candidate.as_ref().to_string();
            let lowered = candidate.to_lowercase();
            // Rank by the length of the longest common prefix: it is what makes
            // "cach" find "Cache" and "chrom" find "Chrome", and it needs no
            // edit-distance machinery for names this short.
            let prefix = lowered
                .chars()
                .zip(needle.chars())
                .take_while(|(a, b)| a == b)
                .count();
            (usize::MAX - prefix, candidate)
        })
        // A name sharing no first character with the query is not a suggestion.
        .filter(|(score, _)| *score != usize::MAX)
        .collect();
    scored.sort();
    Unresolved {
        what,
        name: name.to_string(),
        suggestions: scored.into_iter().take(3).map(|(_, n)| n).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database::cleaner_database::CleanerDatabase;
    use database::structures::{CleanerData, CleanerFlags};

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

    fn sample() -> AppState {
        state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
            entry("Cache", "Firefox", "Browser"),
            entry("Logs", "Chrome", "App"),
            entry("Logs", "Firefox", ""),
        ])
    }

    fn names(selection: &Selection) -> Vec<&str> {
        selection.programs.iter().map(|(p, _)| p.as_str()).collect()
    }

    #[test]
    fn a_category_selects_all_of_its_subcategories() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["Cache".to_string()],
                ..Default::default()
            },
        )
        .expect("Cache exists");
        // Chrome is in Cache/Browser and Cache/Code, so a whole-category tick
        // has to reach both — the same two subcategories the terminal app's
        // category checkbox reaches.
        assert_eq!(
            app.selected_map().get(&Arc::from("Cache")).map(|s| s.len()),
            Some(2)
        );
        assert_eq!(names(&selection), ["Chrome", "Firefox"]);
        assert_eq!(selection.programs[0].1, ["Cache"]);
    }

    #[test]
    fn a_subcategory_selects_only_that_one() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["Cache/Code".to_string()],
                ..Default::default()
            },
        )
        .expect("Cache/Code exists");
        // Chrome has Code, Firefox does not.
        assert_eq!(names(&selection), ["Chrome"]);
    }

    #[test]
    fn uncategorized_addresses_the_empty_subcategory() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["Logs/Uncategorized".to_string()],
                ..Default::default()
            },
        )
        .expect("Logs has an uncategorized subcategory");
        assert_eq!(names(&selection), ["Firefox"]);
    }

    #[test]
    fn names_are_matched_without_case() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["cAcHe".to_string()],
                ..Default::default()
            },
        )
        .expect("Cache matches case-insensitively");
        assert_eq!(names(&selection), ["Chrome", "Firefox"]);
    }

    #[test]
    fn all_selects_every_category() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                ..Default::default()
            },
        )
        .expect("no names to resolve");
        assert_eq!(names(&selection), ["Chrome", "Firefox"]);
    }

    #[test]
    fn exclude_category_wins_over_all() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                exclude_categories: vec!["Logs".to_string()],
                ..Default::default()
            },
        )
        .expect("Logs exists");
        assert!(!app.selected_map().contains_key(&Arc::from("Logs")));
        assert_eq!(names(&selection), ["Chrome", "Firefox"]);
    }

    #[test]
    fn a_program_can_be_narrowed_to_one_category() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                programs: vec!["Chrome=Logs".to_string()],
                ..Default::default()
            },
        )
        .expect("Chrome is in Logs");
        // Naming one program means only that program: Firefox was never asked
        // for, so it is unticked and the job skips it entirely.
        assert_eq!(names(&selection), ["Chrome"]);
        assert_eq!(selection.programs[0].1, ["Logs"]);
        // The exclusions the job reads have to agree with what is reported:
        // Firefox as a whole program, Chrome's Cache entries.
        let (excluded, pairs) = app.exclusions();
        assert!(excluded.contains(&Arc::from("Firefox")));
        assert!(pairs.contains(&(Arc::from("Chrome"), Arc::from("Cache"))));
        assert!(!pairs.contains(&(Arc::from("Chrome"), Arc::from("Logs"))));
    }

    #[test]
    fn a_program_star_is_the_whole_program() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                programs: vec!["Chrome=*".to_string()],
                ..Default::default()
            },
        )
        .expect("Chrome exists");
        assert_eq!(names(&selection), ["Chrome"]);
        assert_eq!(selection.programs[0].1, ["Cache", "Logs"]);
    }

    #[test]
    fn excluded_programs_are_not_cleaned() {
        let mut app = sample();
        let selection = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                exclude_programs: vec!["chrome".to_string()],
                ..Default::default()
            },
        )
        .expect("Chrome exists");
        assert_eq!(names(&selection), ["Firefox"]);
        let (excluded, _) = app.exclusions();
        assert!(excluded.contains(&Arc::from("Chrome")));
    }

    #[test]
    fn an_unknown_name_is_an_error_with_a_suggestion() {
        let mut app = sample();
        let error = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["Cach".to_string()],
                ..Default::default()
            },
        )
        .expect_err("Cach is not a category");
        assert_eq!(error.suggestions, ["Cache"]);
        assert!(error.to_string().contains("did you mean `Cache`"));
    }

    #[test]
    fn an_unknown_subcategory_is_an_error() {
        let mut app = sample();
        let error = apply(
            &mut app,
            &SelectionArgs {
                categories: vec!["Cache/Nope".to_string()],
                ..Default::default()
            },
        )
        .expect_err("Cache has no subcategory called Nope");
        assert_eq!(error.what, "subcategory");
        assert_eq!(error.suggestions, ["Browser", "Code"]);
    }

    #[test]
    fn excluding_a_subcategory_that_is_not_there_is_not_an_error() {
        let mut app = sample();
        apply(
            &mut app,
            &SelectionArgs {
                all: true,
                exclude_categories: vec!["Cache/Nope".to_string()],
                ..Default::default()
            },
        )
        .expect("excluding something absent is harmless");
    }

    #[test]
    fn a_program_outside_the_selected_categories_is_an_error() {
        // Discord is not in the sample database at all, so it cannot be
        // resolved even though the categories are wide open.
        let mut app = sample();
        let error = apply(
            &mut app,
            &SelectionArgs {
                all: true,
                programs: vec!["Discord".to_string()],
                ..Default::default()
            },
        )
        .expect_err("Discord is not in the database");
        assert_eq!(error.what, "program");
    }
}
