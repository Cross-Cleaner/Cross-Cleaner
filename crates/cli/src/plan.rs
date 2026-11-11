//! What a selection would touch, without touching it.
//!
//! [`appcore::cleaning::work`] answers "what did you delete" only after it has
//! deleted it, so a dry run cannot ask it. What it *does* do is filter every
//! database entry by the same three conditions, in the same order, over the same
//! databases. This module applies those conditions itself and hands the entries
//! over — which is the whole reason the conditions are worth spelling out
//! plainly in `cleaning::work`.
//!
//! The registry half is Windows-only and follows the databases
//! [`appcore::AppState`] holds, so `cli plan` and `cli clean` cover the same
//! entries.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use appcore::AppState;

use database::cleaner_database::CleanerDatabase;
use database::structures::CustomCleaner;

/// One database entry the selection would run, plus where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEntry {
    pub program: String,
    pub category: String,
    pub sub_category: String,
    pub path: String,
    /// Registry entries are cleaned by the registry cleaner rather than by the
    /// filesystem one, so the plan says which of the two this is.
    pub registry: bool,
}

/// Every entry the current selection on `state` would run.
pub fn entries(state: &AppState) -> Vec<PlannedEntry> {
    let selected = state.selected_map();
    let (excluded_programs, excluded_program_categories) = state.exclusions();
    let mut out = Vec::new();

    state
        .database
        .for_each(|data| {
            if keeps(
                &selected,
                &excluded_programs,
                &excluded_program_categories,
                &data.category,
                &data.sub_category,
                &data.program,
            ) {
                out.push(entry(
                    &data.program,
                    &data.category,
                    &data.sub_category,
                    &data.path.as_string(),
                    false,
                ));
            }
        })
        .expect("Failed to read cleaner database");

    for data in state.custom_database.iter() {
        if keeps(
            &selected,
            &excluded_programs,
            &excluded_program_categories,
            &data.category,
            &data.sub_category,
            &data.program,
        ) {
            out.push(entry(
                &data.id,
                &data.category,
                &data.sub_category,
                &data.path.as_string(),
                false,
            ));
        }
    }

    #[cfg(windows)]
    state
        .registry_database
        .for_each(|data| {
            if keeps(
                &selected,
                &excluded_programs,
                &excluded_program_categories,
                &data.category,
                &data.sub_category,
                &data.program,
            ) {
                out.push(entry(
                    &data.program,
                    &data.category,
                    &data.sub_category,
                    &data.path.as_string(),
                    true,
                ));
            }
        })
        .expect("Failed to read registry database");

    out
}

/// The three conditions a database entry has to pass, in the order
/// [`appcore::cleaning::work`] applies them.
fn keeps(
    selected: &HashMap<Arc<str>, HashSet<Arc<str>>>,
    excluded_programs: &HashSet<Arc<str>>,
    excluded_program_categories: &HashSet<(Arc<str>, Arc<str>)>,
    category: &Arc<str>,
    sub_category: &Arc<str>,
    program: &Arc<str>,
) -> bool {
    let Some(subs) = selected.get(category.as_ref()) else {
        return false;
    };
    // The key the job looks the entry up by is `effective_sub`, not the raw
    // `sub_category`, so it has to be normalized the same way here.
    let effective = appcore::effective_sub("", sub_category);
    subs.contains(&effective)
        && !excluded_programs.contains(program.as_ref())
        && !excluded_program_categories.contains(&(Arc::clone(program), Arc::clone(category)))
}

fn entry(
    program: &str,
    category: &str,
    sub_category: &str,
    path: &str,
    registry: bool,
) -> PlannedEntry {
    PlannedEntry {
        program: program.to_string(),
        category: category.to_string(),
        sub_category: if sub_category.is_empty() {
            "Uncategorized".to_string()
        } else {
            sub_category.to_string()
        },
        path: path.to_string(),
        registry,
    }
}

/// The categories and subcategories of a database, used by `cli categories`.
pub fn categories(database: &CleanerDatabase, custom: &[CustomCleaner]) -> Vec<CategoryView> {
    let mut views: Vec<CategoryView> = Vec::new();

    let mut add = |name: &Arc<str>, sub: &Arc<str>| {
        let index = match views.iter().position(|view| view.name == name.as_ref()) {
            Some(index) => index,
            None => {
                views.push(CategoryView {
                    name: name.to_string(),
                    subs: Vec::new(),
                    entries: 0,
                });
                views.len() - 1
            }
        };
        let view = &mut views[index];
        view.entries += 1;
        // INFO: the empty subcategory is matched before it is renamed. Once
        // "Uncategorized" is written into the row there is no way to tell it
        // apart from a subcategory that is literally called that, so every
        // uncategorized entry would open a row of its own.
        match view.subs.iter().position(|s| s.key == sub.as_ref()) {
            Some(existing) => view.subs[existing].entries += 1,
            None => view.subs.push(SubView {
                name: sub_label(sub),
                key: sub.to_string(),
                entries: 1,
            }),
        }
    };

    let _ = database.for_each_index(|data| add(&data.category, &data.sub_category));
    for data in custom {
        add(&data.category, &data.sub_category);
    }

    for view in &mut views {
        view.subs.sort_by(|a, b| a.key.cmp(&b.key));
    }
    // Same order the category page uses, so `cli categories` lists what the
    // window app lists, in the same sequence.
    views.sort_by(|a, b| {
        priority(&a.name)
            .cmp(&priority(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    views
}

/// What `cli categories` prints for one category.
pub struct CategoryView {
    pub name: String,
    /// Empty only when the category has no entries at all.
    pub subs: Vec<SubView>,
    pub entries: usize,
}

/// What `cli categories` prints for one subcategory.
pub struct SubView {
    /// Display name: the empty subcategory reads as "Uncategorized", the same
    /// word the window app uses for it.
    pub name: String,
    /// The raw `sub_category`, which is what the selection keys on. Kept
    /// alongside the display name so rows can be matched before they are
    /// renamed, and so `--sub` can be told which rows a name addresses.
    key: String,
    pub entries: usize,
}

/// How a raw `sub_category` reads in a listing.
fn sub_label(sub: &str) -> String {
    if sub.is_empty() {
        "Uncategorized".to_string()
    } else {
        sub.to_string()
    }
}

/// Sort order of the category list, matching `appcore::app`.
fn priority(name: &str) -> u8 {
    match name {
        "Cache" => 0,
        "Logs" => 1,
        "Crashes" => 2,
        "Documentation" => 3,
        "Backups" => 4,
        "LastActivity" => 5,
        _ => 6,
    }
}

/// The programs a selection resolves to, with the categories left enabled for
/// each, for `cli programs`.
pub fn program_views(state: &AppState) -> Vec<(String, Vec<String>)> {
    (0..state.program_checkboxes.len())
        .filter(|&index| *state.program_checkboxes[index].0.borrow())
        .map(|index| {
            (
                state.program_checkboxes[index].1.to_string(),
                state
                    .program_categories
                    .get(index)
                    .map(|categories| {
                        let disabled = state.program_disabled.get(index);
                        categories
                            .iter()
                            .filter(|category| !disabled.is_some_and(|set| set.contains(*category)))
                            .map(|category| category.to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use appcore::AppState;
    use database::cleaner_database::CleanerDatabase;
    use database::structures::{CleanerData, CleanerFlags, CustomCleaner};
    use std::sync::Arc;

    fn entry(category: &str, program: &str, sub: &str) -> CleanerData {
        CleanerData {
            path: format!("{}/{sub}", program).into(),
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

    fn paths(state: &AppState) -> Vec<String> {
        let mut paths: Vec<String> = entries(state).into_iter().map(|e| e.path).collect();
        paths.sort();
        paths
    }

    #[test]
    fn only_the_selected_subcategory_is_planned() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
            entry("Cache", "Firefox", "Browser"),
        ]);
        // Ticking the category first and then unticking Code is what the
        // terminal app's subcategory overlay does, and the plan has to narrow
        // to exactly that one subcategory afterwards.
        app.toggle_category(0);
        app.toggle_category_sub(0, &Arc::from("Code"));
        app.build_program_list();
        assert_eq!(paths(&app), ["Chrome/Browser", "Firefox/Browser"]);
    }

    #[test]
    fn a_category_ticked_on_its_own_plans_every_subcategory() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
            entry("Cache", "Firefox", "Browser"),
        ]);
        // Ticking only the subcategory, without the category first.
        app.toggle_category_sub(0, &Arc::from("Code"));
        app.build_program_list();
        assert_eq!(paths(&app), ["Chrome/Code"]);
    }

    #[test]
    fn a_whole_category_plans_every_subcategory() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
        ]);
        app.toggle_category(0);
        app.build_program_list();
        assert_eq!(paths(&app), ["Chrome/Browser", "Chrome/Code"]);
    }

    #[test]
    fn an_unticked_program_is_not_planned() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Firefox", "Browser"),
        ]);
        app.toggle_category(0);
        app.build_program_list();
        app.toggle_program(0);
        assert_eq!(paths(&app), ["Firefox/Browser"]);
    }

    #[test]
    fn an_excluded_program_category_is_not_planned() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Logs", "Chrome", "App"),
        ]);
        app.toggle_category(0);
        app.toggle_category(1);
        app.build_program_list();
        app.toggle_program_category(0, &Arc::from("Logs"));
        assert_eq!(paths(&app), ["Chrome/Browser"]);
    }

    #[test]
    fn categories_are_listed_with_counts_in_priority_order() {
        let database = CleanerDatabase::from_vec(vec![
            entry("Logs", "B", ""),
            entry("Cache", "A", "Browser"),
            entry("Cache", "A", "Browser"),
            entry("Artifacts", "C", ""),
        ]);
        let views = categories(&database, &[]);
        let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Cache", "Logs", "Artifacts"]);
        assert_eq!(views[0].entries, 2);
        assert_eq!(views[0].subs.len(), 1);
        assert_eq!(views[0].subs[0].name, "Browser");
        assert_eq!(views[0].subs[0].entries, 2);
    }

    #[test]
    fn an_entry_without_a_subcategory_is_shown_as_uncategorized() {
        let database = CleanerDatabase::from_vec(vec![entry("Logs", "B", "")]);
        let views = categories(&database, &[]);
        assert_eq!(views[0].subs[0].name, "Uncategorized");
    }

    /// Every uncategorized entry belongs to one row. Matching the rows by their
    /// display name instead of by the raw `sub_category` gave each of them a
    /// row of its own, which is what a category listing must never do.
    #[test]
    fn uncategorized_entries_share_one_row() {
        let database = CleanerDatabase::from_vec(vec![
            entry("Logs", "A", ""),
            entry("Logs", "B", ""),
            entry("Logs", "C", ""),
            entry("Logs", "D", "App"),
        ]);
        let views = categories(&database, &[]);
        assert_eq!(views[0].subs.len(), 2, "one row for each subcategory");
        let uncategorized = views[0]
            .subs
            .iter()
            .find(|sub| sub.name == "Uncategorized")
            .expect("an uncategorized row");
        assert_eq!(uncategorized.entries, 3);
        assert_eq!(views[0].entries, 4);
    }

    #[test]
    fn custom_cleaners_are_counted_like_database_entries() {
        let database = CleanerDatabase::from_vec(Vec::new());
        let custom = vec![CustomCleaner {
            id: "Optimize pictures".to_string(),
            program: Arc::from("Pictures"),
            category: Arc::from("Images"),
            sub_category: Arc::from("Pictures"),
            path: "pictures".into(),
            args: Vec::new(),
            os: Vec::new(),
            function: unreachable_cleaner,
            sequential: false,
        }];
        let views = categories(&database, &custom);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "Images");
    }

    fn unreachable_cleaner(
        _: &CustomCleaner,
        _: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = database::structures::CleanerResult> + Send>,
    > {
        Box::pin(async { unreachable!("the plan never runs a cleaner") })
    }
}
