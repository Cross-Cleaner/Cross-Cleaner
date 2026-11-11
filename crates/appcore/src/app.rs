//! Frontend-agnostic application state.
//!
//! [`AppState`] owns everything both frontends agree on: which page is open,
//! which categories and programs are selected, the cleaning job handle and its
//! progress, and the databases the selection is resolved against.
//!
//! The two frontends differ only in how they *present* this:
//!
//! | Concern | Where it lives |
//! |---|---|
//! | selection rules, program list, job control, ETA | here |
//! | widgets, layout, colors | `gui::pages` / `tui::pages` |
//! | key and mouse bindings | `gui::app` / `tui::app` |
//! | textures, sounds, notifications, window chrome | `gui` |

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::CustomCleaner;
use tokio::sync::mpsc;

use crate::categories::{CategoryState, Toggle, effective_sub};
use crate::cleaning;
pub use crate::cleaning::CleanResult;

/// Number of category checkboxes per row in the window frontend.
pub const CATEGORY_COLUMNS: usize = 2;

/// A top-level screen of the app. Both frontends render exactly these five.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Page {
    Main,
    ProgramSelection,
    Clearing,
    Results,
    Settings,
}

impl Page {
    /// Short label for a tab bar / header.
    pub fn title(self) -> &'static str {
        match self {
            Page::Main => "Categories",
            Page::ProgramSelection => "Programs",
            Page::Clearing => "Cleaning",
            Page::Results => "Results",
            Page::Settings => "Settings",
        }
    }

    /// True when the page shows a back arrow in the window frontend.
    pub fn has_back(self) -> bool {
        matches!(
            self,
            Page::Results | Page::ProgramSelection | Page::Settings
        )
    }
}

/// Sort order of the category list: the categories a user cares about come
/// first, everything else follows alphabetically.
fn category_priority(name: &str) -> u8 {
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

/// Accumulates category / subcategory statistics while streaming the databases
/// in one pass, so a category is described exactly once instead of once per
/// entry.
#[derive(Default)]
struct CategoryIndex {
    /// Real subcategories per category. Always contains an entry for every
    /// known category, even when the set is empty.
    subs: HashMap<Arc<str>, HashSet<Arc<str>>>,
    /// Whether a category also has entries without a `sub_category`.
    has_empty: HashMap<Arc<str>, bool>,
    /// Number of database paths per category.
    counts: HashMap<Arc<str>, usize>,
    /// Number of database paths per `(category, sub_category)`.
    sub_counts: HashMap<(Arc<str>, Arc<str>), usize>,
}

impl CategoryIndex {
    /// Streams every database once and folds the entries into the index.
    fn scan(
        database: &CleanerDatabase,
        custom_database: &[CustomCleaner],
        #[cfg(windows)] registry_database: Option<&RegistryDatabase>,
    ) -> Self {
        let mut index = Self::default();
        database
            .for_each_index(|data| index.add(&data.category, &data.sub_category))
            .expect("Failed to read cleaner database");
        for data in custom_database {
            index.add(&data.category, &data.sub_category);
        }
        // Registry entries without a category are internal cleanup of the app
        // itself and are never offered to the user.
        #[cfg(windows)]
        if let Some(registry) = registry_database {
            registry
                .for_each_index(|data| {
                    if !data.category.is_empty() {
                        index.add(&data.category, &data.sub_category);
                    }
                })
                .expect("Failed to read registry database");
        }
        index
    }

    /// Folds one database entry into the index.
    fn add(&mut self, category: &Arc<str>, sub_category: &str) {
        // Make sure the category shows up even when all of its entries share
        // an empty sub_category.
        self.subs.entry(Arc::clone(category)).or_default();
        *self.counts.entry(Arc::clone(category)).or_insert(0) += 1;

        let sub = effective_sub("", sub_category);
        *self
            .sub_counts
            .entry((Arc::clone(category), Arc::clone(&sub)))
            .or_insert(0) += 1;

        if sub.is_empty() {
            self.has_empty.entry(Arc::clone(category)).or_insert(true);
        } else {
            self.subs
                .get_mut(category)
                .expect("category was just inserted")
                .insert(sub);
        }
    }

    /// Turns the accumulated maps into the sorted category list, the checkbox
    /// labels and the per-`(category, sub_category)` counters — all derived
    /// from the same single pass over the databases.
    #[allow(clippy::type_complexity)]
    fn into_categories(
        self,
    ) -> (
        Vec<CategoryState>,
        Vec<String>,
        HashMap<(Arc<str>, Arc<str>), usize>,
    ) {
        let Self {
            mut subs,
            mut has_empty,
            counts,
            sub_counts,
        } = self;

        let mut names: Vec<Arc<str>> = subs.keys().cloned().collect();
        names.sort_by(|a, b| {
            let (a_prio, b_prio) = (category_priority(a), category_priority(b));
            if a_prio == b_prio {
                a.cmp(b)
            } else {
                a_prio.cmp(&b_prio)
            }
        });

        let mut categories = Vec::with_capacity(names.len());
        for name in names {
            let mut list: Vec<Arc<str>> =
                subs.remove(&name).unwrap_or_default().into_iter().collect();
            list.sort();
            categories.push(CategoryState {
                has_empty: has_empty.remove(&name).unwrap_or(false),
                name,
                subs: list,
                selected: HashSet::new(),
            });
        }

        let labels = categories
            .iter()
            .map(|category| match counts.get(&category.name).copied() {
                Some(n) if n > 0 => format!("{} ({n})", category.name),
                _ => category.name.to_string(),
            })
            .collect();

        (categories, labels, sub_counts)
    }
}

/// The whole application state, minus anything specific to a frontend.
pub struct AppState {
    // --- category page ---
    /// Every category found in the databases, with its subcategories.
    pub categories: Vec<CategoryState>,
    /// Precomputed checkbox labels like `"Cache (12)"`, parallel to
    /// [`Self::categories`]. Computed once so the UI does not rebuild them
    /// every frame.
    pub category_labels: Vec<String>,
    /// Number of database paths per `(category, sub_category)`.
    pub sub_counts: HashMap<(Arc<str>, Arc<str>), usize>,

    // --- program selection page ---
    /// Program checkboxes, parallel to [`Self::program_categories`] and
    /// [`Self::program_disabled`]. The `Rc<RefCell<bool>>` is what the window
    /// frontend binds its widget to.
    pub program_checkboxes: Vec<(Rc<RefCell<bool>>, Arc<str>)>,
    /// Selected categories that apply to each program.
    pub program_categories: Vec<Vec<Arc<str>>>,
    /// Categories the user disabled per program.
    pub program_disabled: Vec<HashSet<Arc<str>>>,
    /// Lowercased search text; used for filtering.
    pub search_query: String,
    /// The text exactly as typed, so the input field keeps its casing.
    pub search_query_visible: String,
    /// Indices into [`Self::program_checkboxes`] matching
    /// [`Self::search_query`], in order. Recomputed only when the query or the
    /// program list changes, so the UI never rescans the whole list per frame.
    pub filtered_programs: Vec<usize>,
    /// Programs that will be skipped, filled when cleaning starts.
    pub excluded_programs: HashSet<Arc<str>>,

    // --- cleaning / results ---
    /// Handle of the spawned cleaning job, `None` while idle.
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Name of the entry currently being cleaned, as sent by the job.
    pub progress_message: String,
    /// Progress messages from the cleaning job.
    pub progress_receiver: Option<mpsc::Receiver<String>>,
    /// Result channel: the job pushes its [`CleanResult`] here so the frontend
    /// can pick it up with a non-blocking `try_recv`.
    pub result_receiver: Option<std::sync::mpsc::Receiver<CleanResult>>,
    /// Finished run, kept while the results page is open.
    pub cleared_data: Option<CleanResult>,
    pub current_page: Page,
    pub current_task: usize,
    pub total_tasks: usize,
    pub cleaned_bytes: u64,
    /// When the first progress tick arrived, used for the ETA estimate.
    pub progress_start: Option<Instant>,

    // --- databases ---
    pub database: CleanerDatabase,
    pub custom_database: Arc<[CustomCleaner]>,
    #[cfg(windows)]
    pub registry_database: RegistryDatabase,

    /// Precomputed window title.
    pub window_title: String,
}

impl AppState {
    /// Builds the state from the cleaner, registry and custom-cleaner
    /// databases.
    #[cfg(windows)]
    pub fn from_database(
        database: CleanerDatabase,
        registry_database: RegistryDatabase,
        custom_database: Arc<[CustomCleaner]>,
    ) -> Self {
        let index = CategoryIndex::scan(&database, &custom_database, Some(&registry_database));
        Self::assemble(database, custom_database, registry_database, index)
    }

    /// Builds the state from the cleaner and custom-cleaner databases.
    #[cfg(not(windows))]
    pub fn from_database(database: CleanerDatabase, custom_database: Arc<[CustomCleaner]>) -> Self {
        let index = CategoryIndex::scan(&database, &custom_database);
        Self::assemble(database, custom_database, index)
    }

    fn assemble(
        database: CleanerDatabase,
        custom_database: Arc<[CustomCleaner]>,
        #[cfg(windows)] registry_database: RegistryDatabase,
        index: CategoryIndex,
    ) -> Self {
        let (categories, category_labels, sub_counts) = index.into_categories();
        Self {
            categories,
            category_labels,
            sub_counts,

            program_checkboxes: Vec::new(),
            program_categories: Vec::new(),
            program_disabled: Vec::new(),
            search_query: String::new(),
            search_query_visible: String::new(),
            filtered_programs: Vec::new(),
            excluded_programs: HashSet::new(),

            task_handle: None,
            progress_message: String::new(),
            progress_receiver: None,
            result_receiver: None,
            cleared_data: None,
            current_page: Page::Main,
            current_task: 0,
            total_tasks: 0,
            cleaned_bytes: 0,
            progress_start: None,

            database,
            custom_database,
            #[cfg(windows)]
            registry_database,

            window_title: String::from("Cross Cleaner"),
        }
    }

    // --- navigation ------------------------------------------------------

    /// Back navigation, shared by the title bar button, the Android system back
    /// gesture and the terminal's `Esc`. Returns `true` when the app handled
    /// the request, so the caller does not also quit.
    pub fn go_back(&mut self) -> bool {
        match self.current_page {
            Page::Results => {
                self.cleared_data = None;
                self.current_page = Page::Main;
                true
            }
            Page::ProgramSelection | Page::Settings | Page::Clearing => {
                self.current_page = Page::Main;
                true
            }
            Page::Main => false,
        }
    }

    // --- selection -------------------------------------------------------

    /// Currently selected subcategories per category, in the shape the cleaning
    /// job expects. Categories with nothing selected are omitted.
    pub fn selected_map(&self) -> HashMap<Arc<str>, HashSet<Arc<str>>> {
        self.categories
            .iter()
            .filter(|c| !c.selected.is_empty())
            .map(|c| (Arc::clone(&c.name), c.selected.clone()))
            .collect()
    }

    /// True when at least one category or subcategory is ticked.
    pub fn has_selection(&self) -> bool {
        self.categories.iter().any(|c| !c.selected.is_empty())
    }

    /// Label shown next to the category checkbox, e.g. `"Cache (12)"`.
    pub fn category_label(&self, index: usize) -> &str {
        self.category_labels.get(index).map_or("", String::as_str)
    }

    /// Label for one subcategory of `category`, e.g. `"Browser (3)"`. The
    /// empty subcategory is shown as "Uncategorized".
    pub fn sub_label(&self, category: &Arc<str>, sub: &Arc<str>) -> String {
        let count = self
            .sub_counts
            .get(&(Arc::clone(category), Arc::clone(sub)))
            .copied()
            .unwrap_or(0);
        let name = if sub.is_empty() {
            "Uncategorized"
        } else {
            sub.as_ref()
        };
        if count > 0 {
            format!("{name} ({count})")
        } else {
            name.to_string()
        }
    }

    /// Ticks or unticks the category itself, selecting all of its
    /// subcategories at once.
    pub fn toggle_category(&mut self, index: usize) -> Toggle {
        self.categories
            .get_mut(index)
            .map_or(Toggle::Off, CategoryState::toggle)
    }

    /// Ticks or unticks a single subcategory of the category at `index`.
    pub fn toggle_category_sub(&mut self, index: usize, sub: &Arc<str>) -> Toggle {
        self.categories
            .get_mut(index)
            .map_or(Toggle::Off, |category| category.toggle_sub(sub))
    }

    /// Ticks or unticks the program at `index`. Dropping the tick also clears
    /// its per-category exclusions, so re-ticking starts from a clean slate.
    pub fn toggle_program(&mut self, index: usize) -> Toggle {
        let Some((checkbox, _)) = self.program_checkboxes.get(index) else {
            return Toggle::Off;
        };
        if let Some(disabled) = self.program_disabled.get_mut(index) {
            disabled.clear();
        }
        let was_enabled = *checkbox.borrow();
        *checkbox.borrow_mut() = !was_enabled;
        if was_enabled { Toggle::Off } else { Toggle::On }
    }

    /// Enables or disables one category of the program at `index`, which is how
    /// a program belonging to several categories is trimmed down. Returns
    /// [`Toggle::On`] when the category ended up enabled.
    pub fn toggle_program_category(&mut self, index: usize, category: &Arc<str>) -> Toggle {
        let Some(disabled) = self.program_disabled.get_mut(index) else {
            return Toggle::Off;
        };
        if disabled.remove(category) {
            Toggle::On
        } else {
            disabled.insert(Arc::clone(category));
            Toggle::Off
        }
    }

    /// True when the program is fully selected (nothing excluded).
    pub fn is_program_checked(&self, index: usize) -> bool {
        self.program_master(index)
            && self
                .program_disabled
                .get(index)
                .is_some_and(HashSet::is_empty)
    }

    /// True when the program is ticked but some of its categories are excluded.
    pub fn is_program_indeterminate(&self, index: usize) -> bool {
        self.program_master(index)
            && self
                .program_disabled
                .get(index)
                .is_some_and(|disabled| !disabled.is_empty())
    }

    /// The program's raw tick, ignoring per-category exclusions.
    fn program_master(&self, index: usize) -> bool {
        self.program_checkboxes
            .get(index)
            .is_some_and(|(checkbox, _)| *checkbox.borrow())
    }

    /// Applies a new search query and recomputes [`Self::filtered_programs`].
    pub fn set_search(&mut self, query: &str) {
        self.search_query_visible = query.to_string();
        self.search_query = query.to_lowercase();
        self.rebuild_filtered_programs();
    }

    /// Recomputes `filtered_programs` from the program list and the query.
    /// Cheap, and only called when one of them changes.
    pub fn rebuild_filtered_programs(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_programs = (0..self.program_checkboxes.len()).collect();
            return;
        }
        let query = std::mem::take(&mut self.search_query);
        self.filtered_programs = self
            .program_checkboxes
            .iter()
            .enumerate()
            .filter(|(_, (_, program))| program.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        self.search_query = query;
    }

    /// Turns the current category selection into the program list shown by the
    /// program-selection page. Returns `false` when nothing is selected.
    pub fn build_program_list(&mut self) -> bool {
        if !self.has_selection() {
            return false;
        }
        let selected_map = self.selected_map();
        let mut programs: Vec<(Arc<str>, Vec<Arc<str>>)> = Vec::new();
        let mut add = |program: Arc<str>, category: Arc<str>| {
            if let Some(entry) = programs
                .iter_mut()
                .find(|(p, _)| p.as_ref() == program.as_ref())
            {
                if !entry.1.iter().any(|c| c.as_ref() == category.as_ref()) {
                    entry.1.push(category);
                }
            } else {
                programs.push((program, vec![category]));
            }
        };
        let _ = self.database.for_each(|data| {
            let eff = effective_sub("", &data.sub_category);
            if let Some(subs) = selected_map.get(data.category.as_ref())
                && subs.contains(&eff)
            {
                add(Arc::clone(&data.program), Arc::clone(&data.category));
            }
        });
        for data in self.custom_database.iter() {
            let eff = effective_sub("", &data.sub_category);
            if let Some(subs) = selected_map.get(data.category.as_ref())
                && subs.contains(&eff)
            {
                add(Arc::clone(&data.program), Arc::clone(&data.category));
            }
        }
        #[cfg(windows)]
        {
            let _ = self.registry_database.for_each_index(|data| {
                let eff = effective_sub("", &data.sub_category);
                if let Some(subs) = selected_map.get(data.category.as_ref())
                    && subs.contains(&eff)
                {
                    add(Arc::clone(&data.program), Arc::clone(&data.category));
                }
            });
        }
        programs.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, cats) in programs.iter_mut() {
            cats.sort();
        }

        self.program_checkboxes.clear();
        self.program_categories.clear();
        self.program_disabled.clear();
        for (program, cats) in programs {
            self.program_checkboxes
                .push((Rc::new(RefCell::new(true)), program));
            self.program_categories.push(cats);
            self.program_disabled.push(HashSet::new());
        }

        self.rebuild_filtered_programs();
        true
    }

    /// Starts a cleaning run over *every* category without going through the
    /// page flow.
    ///
    /// Returns `false` when a run is already in progress or the database has no
    /// programs to clean, so the caller can tell a no-op from a started run.
    pub fn quick_clean_all(&mut self) -> bool {
        if self.current_page == Page::Clearing {
            return false;
        }
        // Same result as ticking every category checkbox on the main page.
        for cat in &mut self.categories {
            cat.selected = cat.subs.iter().cloned().collect();
            if cat.has_empty || cat.subs.is_empty() {
                cat.selected.insert(Arc::from(""));
            }
        }
        if !self.build_program_list() {
            for cat in &mut self.categories {
                cat.selected.clear();
            }
            return false;
        }
        self.start_cleaning();
        true
    }

    // --- cleaning --------------------------------------------------------

    /// The two exclusion sets the cleaning job is started with, derived from
    /// the program page as it currently stands.
    ///
    /// Split out of [`Self::start_cleaning`] so a frontend that shows what a
    /// run *would* touch (the command line frontend's `--dry-run`) describes
    /// the selection exactly the way the job itself resolves it, instead of
    /// growing its own second copy of the rule.
    #[allow(clippy::type_complexity)]
    pub fn exclusions(&self) -> (HashSet<Arc<str>>, HashSet<(Arc<str>, Arc<str>)>) {
        let mut excluded_programs = HashSet::new();
        for (checkbox, program) in &self.program_checkboxes {
            if !*checkbox.borrow() {
                excluded_programs.insert(Arc::clone(program));
            }
        }

        // Per-program category exclusions collected in the program popups.
        let mut excluded_program_categories: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        for (i, (checkbox, program)) in self.program_checkboxes.iter().enumerate() {
            if *checkbox.borrow()
                && let Some(disabled) = self.program_disabled.get(i)
            {
                for cat in disabled {
                    excluded_program_categories.insert((Arc::clone(program), Arc::clone(cat)));
                }
            }
        }

        (excluded_programs, excluded_program_categories)
    }

    /// Snapshots the current program selection and spawns the cleaning job.
    /// Switches to the [`Page::Clearing`] page and clears the category
    /// selection, so returning to the main page starts from a blank slate.
    pub fn start_cleaning(&mut self) {
        let selected_map = self.selected_map();

        let (excluded_programs, excluded_program_categories) = self.exclusions();
        self.excluded_programs = excluded_programs.clone();

        let (progress_sender, progress_receiver) = mpsc::channel(32);
        self.progress_receiver = Some(progress_receiver);
        // The job runs on the tokio runtime while the UI thread must never
        // block, so its result is bridged onto a plain sync channel the frontend
        // polls with `try_recv` (see [`Self::poll_result`]).
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        self.result_receiver = Some(result_receiver);
        self.current_task = 0;
        self.total_tasks = 0;
        self.cleaned_bytes = 0;
        self.progress_message.clear();
        self.progress_start = None;

        let database = self.database.clone();
        let custom_database = Arc::clone(&self.custom_database);
        #[cfg(windows)]
        let registry_database = self.registry_database.clone();
        let excluded_programs = self.excluded_programs.clone();
        self.task_handle = Some(tokio::spawn(async move {
            let result = cleaning::work(
                selected_map,
                progress_sender,
                &database,
                &custom_database,
                #[cfg(windows)]
                &registry_database,
                excluded_programs,
                excluded_program_categories,
            )
            .await;
            let _ = result_sender.send(result);
        }));

        self.current_page = Page::Clearing;
        for cat in &mut self.categories {
            cat.selected.clear();
        }
    }

    /// Drains everything the cleaning job has queued and updates the progress
    /// fields. Returns `true` when at least one message was consumed, so the
    /// caller knows a repaint is worthwhile.
    pub fn drain_progress(&mut self) -> bool {
        let Some(receiver) = &mut self.progress_receiver else {
            return false;
        };
        let mut changed = false;
        while let Ok(message) = receiver.try_recv() {
            changed = true;
            if let Some((done, total, bytes)) = cleaning::parse_progress(&message) {
                self.current_task = done;
                self.total_tasks = total;
                self.cleaned_bytes = bytes;
                if self.progress_start.is_none() {
                    self.progress_start = Some(Instant::now());
                }
            } else {
                self.progress_message = message;
            }
        }
        changed
    }

    /// Picks up a finished cleaning run. On success the results are stored in
    /// [`Self::cleared_data`] and the results page is opened. Returns `true`
    /// exactly once per finished run.
    pub fn poll_result(&mut self) -> bool {
        let Some(receiver) = &self.result_receiver else {
            return false;
        };
        let Ok(result) = receiver.try_recv() else {
            return false;
        };
        self.result_receiver = None;
        self.task_handle = None;
        self.cleared_data = Some(result);
        self.current_page = Page::Results;
        true
    }

    /// True while a cleaning job is running.
    pub fn is_cleaning(&self) -> bool {
        self.task_handle.is_some()
    }

    /// Fill ratio of the progress bar, or `None` before the job reported its
    /// task count — the frontend shows a spinner instead of a bar then.
    pub fn progress_fraction(&self) -> Option<f32> {
        if self.total_tasks == 0 {
            return None;
        }
        Some((self.current_task as f32 / self.total_tasks as f32).clamp(0.0, 1.0))
    }

    /// Remaining time, extrapolated from the average duration of the entries
    /// finished so far. `None` at the start and at the end of a run, where there
    /// is nothing to extrapolate from.
    pub fn eta(&self) -> Option<String> {
        if self.current_task == 0 || self.current_task >= self.total_tasks {
            return None;
        }
        let start = self.progress_start?;
        let elapsed = start.elapsed().as_secs_f64();
        let per_task = elapsed / self.current_task as f64;
        let remaining = per_task * (self.total_tasks - self.current_task) as f64;
        let mins = (remaining / 60.0).floor() as u64;
        let secs = (remaining % 60.0).round() as u64;
        if mins > 0 {
            Some(format!("~{mins}m {secs:02}s"))
        } else {
            Some(format!("~{secs}s"))
        }
    }

    /// Name of the entry currently being cleaned, without the wire prefix.
    pub fn current_program(&self) -> &str {
        cleaning::program_name(&self.progress_message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    fn state(entries: Vec<CleanerData>) -> AppState {
        let database = CleanerDatabase::from_vec(entries);
        let custom = Arc::from(Vec::new());
        #[cfg(windows)]
        let registry = RegistryDatabase::from_vec(Vec::new());
        #[cfg(windows)]
        {
            AppState::from_database(database, registry, custom)
        }
        #[cfg(not(windows))]
        {
            AppState::from_database(database, custom)
        }
    }

    #[test]
    fn categories_are_sorted_by_priority_then_name() {
        let app = state(vec![
            entry("Documentation", "App1", ""),
            entry("Cache", "App2", ""),
            entry("Logs", "App3", ""),
            entry("Artifacts", "App4", ""),
        ]);
        let names: Vec<&str> = app.categories.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, vec!["Cache", "Logs", "Documentation", "Artifacts"]);
    }

    #[test]
    fn subcategories_are_collected_sorted_with_uncategorized_flag() {
        let app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
            entry("Cache", "Slack", ""),
        ]);
        assert_eq!(app.categories.len(), 1);
        let cache = &app.categories[0];
        assert_eq!(
            cache.subs.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
            vec!["Browser", "Code"]
        );
        assert!(cache.has_empty);
        assert_eq!(app.category_label(0), "Cache (3)");
        assert_eq!(
            app.sub_label(&cache.name.clone(), &Arc::from("Browser")),
            "Browser (1)"
        );
        assert_eq!(
            app.sub_label(&cache.name.clone(), &Arc::from("")),
            "Uncategorized (1)"
        );
    }

    #[test]
    fn category_without_subs_is_still_listed() {
        let mut app = state(vec![entry("Documentation", "App1", "")]);
        assert_eq!(app.categories.len(), 1);
        assert!(app.categories[0].subs.is_empty());
        // Every entry of this category has no sub_category, so the whole
        // category is "Uncategorized".
        assert!(app.categories[0].has_empty);
        // Ticking it still selects something, so a program list can be built.
        assert!(app.toggle_category(0).is_on());
        assert!(app.has_selection());
        assert!(app.build_program_list());
        assert_eq!(app.program_checkboxes.len(), 1);
    }

    #[test]
    fn build_program_list_needs_a_selection() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        assert!(!app.build_program_list());
        app.toggle_category(0);
        assert!(app.build_program_list());
        // Programs are checked by default and carry their categories.
        assert!(*app.program_checkboxes[0].0.borrow());
        assert_eq!(app.program_categories[0], vec![Arc::from("Cache")]);
    }

    #[test]
    fn program_list_deduplicates_programs_and_categories() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Logs", "Chrome", "App"),
            entry("Cache", "Firefox", "Browser"),
        ]);
        app.toggle_category(0);
        app.toggle_category(1);
        assert!(app.build_program_list());
        let names: Vec<&str> = app
            .program_checkboxes
            .iter()
            .map(|(_, n)| n.as_ref())
            .collect();
        assert_eq!(names, vec!["Chrome", "Firefox"]);
        assert_eq!(
            app.program_categories[0],
            vec![Arc::from("Cache"), Arc::from("Logs")]
        );
    }

    #[test]
    fn per_program_category_toggle_makes_it_indeterminate() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Logs", "Chrome", "App"),
        ]);
        app.toggle_category(0);
        app.toggle_category(1);
        assert!(app.build_program_list());
        assert!(app.is_program_checked(0));
        assert!(!app.is_program_indeterminate(0));
        assert!(!app.toggle_program_category(0, &Arc::from("Logs")).is_on());
        assert!(!app.is_program_checked(0));
        assert!(app.is_program_indeterminate(0));
        // Re-enabling brings the program back to fully checked.
        assert!(app.toggle_program_category(0, &Arc::from("Logs")).is_on());
        assert!(app.is_program_checked(0));
        // Unticking the program drops its per-category exclusions as well.
        assert!(!app.toggle_program(0).is_on());
        assert!(app.program_disabled[0].is_empty());
    }

    #[test]
    fn search_filters_the_program_list() {
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Firefox", "Browser"),
            entry("Cache", "Thunderbird", "Mail"),
        ]);
        app.toggle_category(0);
        assert!(app.build_program_list());
        assert_eq!(app.filtered_programs.len(), 3);
        app.set_search("fox");
        assert_eq!(app.filtered_programs.len(), 1);
        assert_eq!(
            app.program_checkboxes[app.filtered_programs[0]].1.as_ref(),
            "Firefox"
        );
        // The visible text keeps the original casing, the index is lowercased.
        assert_eq!(app.search_query_visible, "fox");
        assert_eq!(app.search_query, "fox");
        app.set_search("");
        assert_eq!(app.filtered_programs.len(), 3);
    }

    #[test]
    fn go_back_navigates_and_stops_at_the_main_page() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        assert!(!app.go_back(), "the main page has nothing to go back to");
        app.current_page = Page::Settings;
        assert!(app.go_back());
        assert_eq!(app.current_page, Page::Main);
        app.current_page = Page::Clearing;
        assert!(app.go_back());
        assert_eq!(app.current_page, Page::Main);
        app.cleared_data = Some((0, 0, 0, Arc::from(Vec::new())));
        app.current_page = Page::Results;
        assert!(app.go_back());
        assert_eq!(app.current_page, Page::Main);
        assert!(app.cleared_data.is_none(), "results are dropped on back");
    }

    #[test]
    fn quick_clean_all_selects_everything_and_starts() {
        // `start_cleaning` spawns onto the tokio runtime, so the call has to
        // happen from inside one.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let mut app = state(vec![
            entry("Cache", "Chrome", "Browser"),
            entry("Cache", "Chrome", "Code"),
            entry("Logs", "Firefox", ""),
        ]);
        let runtime = &runtime;
        runtime.block_on(async {
            assert!(app.quick_clean_all());
            assert_eq!(app.current_page, Page::Clearing);
            assert!(app.is_cleaning());
            // The category selection is consumed by the run.
            assert!(!app.has_selection());
            // A second run cannot start while one is in progress.
            assert!(!app.quick_clean_all());
        });
    }

    #[test]
    fn quick_clean_all_on_empty_database_is_a_noop() {
        let mut app = state(vec![]);
        assert!(!app.quick_clean_all());
        assert_eq!(app.current_page, Page::Main);
        assert!(!app.is_cleaning());
    }

    #[test]
    fn progress_parsing_updates_the_counters() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        let (tx, rx) = mpsc::channel(8);
        app.progress_receiver = Some(rx);
        tx.blocking_send("Cleaning: Chrome".to_string()).unwrap();
        tx.blocking_send("PROGRESS:2:5:4096".to_string()).unwrap();
        assert!(app.drain_progress());
        assert_eq!(app.current_program(), "Chrome");
        assert_eq!((app.current_task, app.total_tasks), (2, 5));
        assert_eq!(app.cleaned_bytes, 4096);
        assert_eq!(app.progress_fraction(), Some(0.4));
        // Nothing queued: no repaint needed.
        assert!(!app.drain_progress());
    }

    #[test]
    fn progress_fraction_is_none_before_the_first_tick() {
        let app = state(vec![entry("Cache", "Chrome", "Browser")]);
        assert_eq!(app.progress_fraction(), None);
        assert_eq!(app.eta(), None);
    }

    #[test]
    fn eta_is_absent_at_the_end_of_a_run() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        app.progress_start = Some(Instant::now());
        app.total_tasks = 4;
        app.current_task = 4;
        assert_eq!(app.eta(), None);
        app.current_task = 2;
        assert!(app.eta().is_some());
    }

    #[test]
    fn poll_result_stores_the_run_and_opens_the_results_page() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        let (tx, rx) = std::sync::mpsc::channel();
        app.result_receiver = Some(rx);
        assert!(!app.poll_result(), "nothing queued yet");
        tx.send((10u64, 2u64, 1u64, Arc::from(Vec::new()))).unwrap();
        assert!(app.poll_result());
        assert_eq!(app.current_page, Page::Results);
        assert!(app.cleared_data.is_some());
        assert!(!app.poll_result(), "a run is reported exactly once");
    }

    #[test]
    fn toggling_out_of_range_is_harmless() {
        let mut app = state(vec![entry("Cache", "Chrome", "Browser")]);
        assert_eq!(app.toggle_category(99), Toggle::Off);
        assert_eq!(app.toggle_program(0), Toggle::Off);
        assert_eq!(
            app.toggle_program_category(0, &Arc::from("Cache")),
            Toggle::Off
        );
        assert_eq!(app.category_label(99), "");
        assert!(!app.is_program_checked(0));
        assert!(!app.is_program_indeterminate(0));
    }
}
