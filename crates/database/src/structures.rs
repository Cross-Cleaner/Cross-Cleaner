use bitflags::bitflags;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::{Arc, OnceLock, RwLock};
use string_interner::{DefaultBackend, DefaultSymbol, StringInterner};

// ── string-interner backed dedup for category/program/class/sub_category ──
static INTERNER: OnceLock<RwLock<StringInterner<DefaultBackend>>> = OnceLock::new();
static ARC_POOL: OnceLock<RwLock<std::collections::HashMap<DefaultSymbol, Arc<str>>>> =
    OnceLock::new();

fn interner() -> &'static RwLock<StringInterner<DefaultBackend>> {
    INTERNER.get_or_init(|| RwLock::new(StringInterner::new()))
}
fn arc_pool() -> &'static RwLock<std::collections::HashMap<DefaultSymbol, Arc<str>>> {
    ARC_POOL.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

/// Intern `s` via `string_interner` and return shared `Arc<str>`.
/// Same string returns same allocation (pointer equality after dedup).
pub fn intern_arc(s: &str) -> Arc<str> {
    if s.is_empty() {
        return Arc::from("");
    }
    // Fast path: read interner to see if already present
    let sym = {
        let mut w = interner().write().unwrap();
        w.get_or_intern(s)
    };
    // Check Arc pool
    {
        let r = arc_pool().read().unwrap();
        if let Some(arc) = r.get(&sym) {
            return Arc::clone(arc);
        }
    }
    let mut w = arc_pool().write().unwrap();
    // double-check
    if let Some(arc) = w.get(&sym) {
        return Arc::clone(arc);
    }
    let resolved = {
        let r = interner().read().unwrap();
        r.resolve(sym).unwrap().to_owned()
    };
    let arc: Arc<str> = Arc::from(resolved);
    w.insert(sym, Arc::clone(&arc));
    arc
}

fn deserialize_shared_str<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(intern_arc(&s))
}
fn deserialize_shared_str_default<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt
        .map(|s| intern_arc(&s))
        .unwrap_or_else(|| intern_arc("Other")))
}
fn deserialize_shared_opt<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.map(|s| intern_arc(&s)).unwrap_or_else(|| Arc::from("")))
}

fn serialize_arc_str<S>(val: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(val)
}
fn default_class_arc() -> Arc<str> {
    intern_arc("Other")
}

// ── segment-interned shared path ──

/// Path stored as interned separator-delimited segments. Near-identical paths
/// (e.g. 100 `{drive}Users\{username}\AppData\...` entries) share every common
/// segment — only the differing tail is stored anew. Concatenating the segments
/// reproduces the original string byte-for-byte, so JSON output is unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SharedPath {
    segs: Arc<[Arc<str>]>,
}

impl SharedPath {
    /// Split `s` after every `/` or `\` (separator stays at the end of its
    /// segment) and intern each segment. Concatenation == original `s`.
    pub fn new(s: &str) -> Self {
        if s.is_empty() {
            return Self::default();
        }
        let mut segs: Vec<Arc<str>> = Vec::new();
        let mut start = 0;
        for (i, ch) in s.char_indices() {
            if ch == '/' || ch == '\\' {
                segs.push(intern_arc(&s[start..=i]));
                start = i + ch.len_utf8();
            }
        }
        if start < s.len() {
            segs.push(intern_arc(&s[start..]));
        }
        Self { segs: segs.into() }
    }

    /// Rebuild the original path string (byte-for-byte round-trip).
    pub fn as_string(&self) -> String {
        let mut out = String::new();
        for seg in self.segs.iter() {
            out.push_str(seg);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Interned segments (separator stays at the end of its segment).
    pub fn segments(&self) -> &[Arc<str>] {
        &self.segs
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.as_string().contains(needle)
    }

    pub fn starts_with(&self, prefix: &str) -> bool {
        self.as_string().starts_with(prefix)
    }

    pub fn replace(&self, from: &str, to: &str) -> SharedPath {
        SharedPath::new(&self.as_string().replace(from, to))
    }
}

impl From<&str> for SharedPath {
    fn from(s: &str) -> Self {
        SharedPath::new(s)
    }
}
impl From<String> for SharedPath {
    fn from(s: String) -> Self {
        SharedPath::new(&s)
    }
}

impl std::fmt::Display for SharedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for seg in self.segs.iter() {
            f.write_str(seg)?;
        }
        Ok(())
    }
}

impl PartialEq<str> for SharedPath {
    fn eq(&self, other: &str) -> bool {
        self.as_string() == other
    }
}
impl PartialEq<&str> for SharedPath {
    fn eq(&self, other: &&str) -> bool {
        self.as_string() == *other
    }
}
impl PartialEq<String> for SharedPath {
    fn eq(&self, other: &String) -> bool {
        self.as_string() == *other
    }
}

impl Serialize for SharedPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.as_string())
    }
}

impl<'de> Deserialize<'de> for SharedPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(SharedPath::new(&String::deserialize(deserializer)?))
    }
}

fn deserialize_vec_arc<'de, D>(deserializer: D) -> Result<Vec<Arc<str>>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Vec::<String>::deserialize(deserializer)?;
    Ok(v.into_iter().map(|s| intern_arc(&s)).collect())
}

// INFO: One path a cleaner deleted, with the space it freed.
//
// INFO: `path` is a [`SharedPath`] rather than a `String`: one run can delete
// tens of thousands of paths that all sit under the same `AppData\Local\...`
// directories, and splitting on the separators interns those directories once.
// Every entry then only stores the segment it is actually unique about.
#[derive(PartialEq, Eq, Clone, Debug, Default)]
pub struct ClearedPath {
    /// The path exactly as it was resolved, byte-for-byte as the cleaner saw it.
    pub path: SharedPath,
    pub removed_bytes: u64,
    pub removed_files: u64,
    pub removed_directories: u64,
}

// INFO: Struct for GUI table (tabled removed - CLI table not used)
#[derive(PartialEq, Clone)]
pub struct Cleared {
    pub program: String,
    pub removed_bytes: u64,
    pub removed_files: u64,
    pub removed_directories: u64,
    pub affected_categories: Vec<String>,
    /// What this program contributed to each category it was cleaned under.
    ///
    /// [`Self::removed_bytes`] and friends are the program's totals; a program
    /// that appears under two categories has one of these rows per category, so
    /// a per-category report can add them up without splitting the totals by
    /// guesswork.
    pub category_totals: Vec<CategoryTotal>,
    /// Every path this program deleted, largest first — what the results page
    /// shows for one row when the user asks where the space went.
    pub paths: Vec<ClearedPath>,
    /// Paths the cleaner deleted that did not fit into [`Self::paths`], because
    /// the run hit the cap that keeps one program's list from growing without
    /// bound. Reported next to the list rather than hidden, so a truncated row
    /// never reads as a complete one.
    pub paths_omitted: usize,
}

/// One program's share of one category, for the per-category report.
#[derive(PartialEq, Eq, Clone, Debug, Default)]
pub struct CategoryTotal {
    pub category: String,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

impl PartialEq<Option<Cleared>> for &Cleared {
    fn eq(&self, other: &Option<Cleared>) -> bool {
        match other {
            Some(other) => other.program.eq(&*self.program),
            None => false,
        }
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CleanerFlags: u8 {
        const REMOVE_ALL_IN_DIR              = 1 << 0;
        const REMOVE_DIRECTORY_AFTER_CLEAN   = 1 << 1;
        const REMOVE_DIRECTORIES             = 1 << 2;
        const REMOVE_FILES                   = 1 << 3;
    }
}

// helper for legacy bool/u8 fields
#[derive(Deserialize)]
#[serde(untagged)]
enum BoolOrU8 {
    Bool(bool),
    U8(u8),
}

impl BoolOrU8 {
    fn as_u8(&self) -> u8 {
        match self {
            BoolOrU8::Bool(b) => *b as u8,
            BoolOrU8::U8(n) => *n,
        }
    }
}

// INFO: Selection rules a declarative entry can carry on top of "what to
// remove": a way to say "not everything, and not all of it".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanerFilter {
    /// Name globs that are never touched, even when they match what is being
    /// removed. A directory named by one is skipped with everything inside it.
    pub exclude: Vec<Arc<str>>,
    /// Only files modified at least this long ago.
    pub older_than_seconds: Option<u64>,
    /// Only files at least this large.
    pub min_size: Option<u64>,
    /// Only files at most this large.
    pub max_size: Option<u64>,
    /// How deep a directory walk may go, counting the entries directly inside
    /// the matched path as depth 1.
    pub max_depth: Option<u32>,
    /// Process names that must not be running for this entry to run at all.
    pub skip_if_running: Vec<Arc<str>>,
    /// Move what is removed to the recycle bin instead of deleting it.
    pub to_trash: bool,
}

impl CleanerFilter {
    /// Whether the filter would keep everything, i.e. whether a walk can take
    /// the plain delete-everything path.
    pub fn is_empty(&self) -> bool {
        self.exclude.is_empty()
            && self.older_than_seconds.is_none()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.max_depth.is_none()
    }

    /// Whether a file of `size` bytes, last modified at `modified` (Unix
    /// seconds), `depth` levels down is something this filter wants removed.
    ///
    /// `max_depth` is checked by the walk, not here: it is a property of the
    /// position, not of the file. Everything else is a property of the file.
    pub fn accepts(&self, size: u64, modified: Option<u64>, now: u64) -> bool {
        if let Some(max) = self.max_size
            && size > max
        {
            return false;
        }

        if let Some(min) = self.min_size
            && size < min
        {
            return false;
        }

        if let Some(age) = self.older_than_seconds {
            // A file with no readable modification time is left alone: "older
            // than a week" cannot be proven, and deleting on a guess is exactly
            // what the filter exists to prevent.
            let Some(modified) = modified else {
                return false;
            };

            if now.saturating_sub(modified) < age {
                return false;
            }
        }

        true
    }
}

// INFO: Struct for clearing files and folders
#[derive(Clone)]
pub struct CleanerData {
    pub path: SharedPath,
    pub category: Arc<str>,
    pub program: Arc<str>,
    pub class: Arc<str>,
    pub sub_category: Arc<str>,
    pub files_to_remove: Vec<Arc<str>>,
    pub directories_to_remove: Vec<Arc<str>>,
    pub flags: CleanerFlags,
    /// Which of the matched items to actually remove.
    pub filter: CleanerFilter,
}

impl CleanerData {
    pub fn remove_all_in_dir(&self) -> bool {
        self.flags.contains(CleanerFlags::REMOVE_ALL_IN_DIR)
    }
    pub fn remove_directory_after_clean(&self) -> bool {
        self.flags
            .contains(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN)
    }
    pub fn remove_directories(&self) -> bool {
        self.flags.contains(CleanerFlags::REMOVE_DIRECTORIES)
    }
    pub fn remove_files(&self) -> bool {
        self.flags.contains(CleanerFlags::REMOVE_FILES)
    }
    pub fn set_remove_all_in_dir(&mut self, v: bool) {
        self.flags.set(CleanerFlags::REMOVE_ALL_IN_DIR, v);
    }
    pub fn set_remove_directory_after_clean(&mut self, v: bool) {
        self.flags
            .set(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN, v);
    }
    pub fn set_remove_directories(&mut self, v: bool) {
        self.flags.set(CleanerFlags::REMOVE_DIRECTORIES, v);
    }
    pub fn set_remove_files(&mut self, v: bool) {
        self.flags.set(CleanerFlags::REMOVE_FILES, v);
    }
}

// custom Deserialize to keep backward compat with old JSON bool fields
impl<'de> Deserialize<'de> for CleanerData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            path: String,
            category: String,
            program: String,
            #[serde(default = "default_class")]
            class: String,
            #[serde(default, alias = "sub_class", alias = "subCategory")]
            sub_category: String,
            #[serde(default)]
            files_to_remove: Vec<String>,
            #[serde(default)]
            directories_to_remove: Vec<String>,
            // new byte flag field
            #[serde(default)]
            flags: Option<u8>,
            // selection rules
            #[serde(default)]
            exclude: Vec<String>,
            #[serde(default)]
            older_than_days: Option<u64>,
            #[serde(default)]
            min_size: Option<u64>,
            #[serde(default)]
            max_size: Option<u64>,
            #[serde(default)]
            max_depth: Option<u32>,
            #[serde(default)]
            skip_if_running: Vec<String>,
            #[serde(default)]
            to_trash: bool,
            // legacy bool fields: accept bool or u8 (byte flag per-field)
            #[serde(default)]
            remove_all_in_dir: Option<BoolOrU8>,
            #[serde(default)]
            remove_directory_after_clean: Option<BoolOrU8>,
            #[serde(default)]
            remove_directories: Option<BoolOrU8>,
            #[serde(default)]
            remove_files: Option<BoolOrU8>,
        }

        let h = Helper::deserialize(deserializer)?;
        let mut bits: u8 = h.flags.unwrap_or(0);
        if let Some(v) = h.remove_all_in_dir {
            if v.as_u8() != 0 {
                bits |= CleanerFlags::REMOVE_ALL_IN_DIR.bits();
            }
        }
        if let Some(v) = h.remove_directory_after_clean {
            if v.as_u8() != 0 {
                bits |= CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN.bits();
            }
        }
        if let Some(v) = h.remove_directories {
            if v.as_u8() != 0 {
                bits |= CleanerFlags::REMOVE_DIRECTORIES.bits();
            }
        }
        if let Some(v) = h.remove_files {
            if v.as_u8() != 0 {
                bits |= CleanerFlags::REMOVE_FILES.bits();
            }
        }

        Ok(CleanerData {
            path: h.path.into(),
            category: intern_arc(&h.category),
            program: intern_arc(&h.program),
            class: intern_arc(&h.class),
            sub_category: intern_arc(&h.sub_category),
            files_to_remove: h
                .files_to_remove
                .into_iter()
                .map(|s| intern_arc(&s))
                .collect(),
            directories_to_remove: h
                .directories_to_remove
                .into_iter()
                .map(|s| intern_arc(&s))
                .collect(),
            flags: CleanerFlags::from_bits_truncate(bits),
            filter: CleanerFilter {
                exclude: h.exclude.iter().map(|s| intern_arc(s)).collect(),
                older_than_seconds: h
                    .older_than_days
                    .map(|days| days.saturating_mul(24 * 60 * 60)),
                min_size: h.min_size,
                max_size: h.max_size,
                max_depth: h.max_depth,
                skip_if_running: h.skip_if_running.iter().map(|s| intern_arc(s)).collect(),
                to_trash: h.to_trash,
            },
        })
    }
}

// Serialize emits both new `flags` and legacy bool fields for max compat
impl CleanerData {
    fn serialize_legacy<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("CleanerData", 16)?;
        s.serialize_field("path", &self.path)?;
        s.serialize_field("category", &self.category)?;
        s.serialize_field("program", &self.program)?;
        s.serialize_field("class", &self.class)?;
        s.serialize_field("sub_category", &self.sub_category)?;
        s.serialize_field("files_to_remove", &self.files_to_remove)?;
        s.serialize_field("directories_to_remove", &self.directories_to_remove)?;
        s.serialize_field("flags", &self.flags.bits())?;
        // legacy aliases so old readers still work if they ignore `flags`
        s.serialize_field(
            "remove_all_in_dir",
            &self.flags.contains(CleanerFlags::REMOVE_ALL_IN_DIR),
        )?;
        s.serialize_field(
            "remove_directory_after_clean",
            &self
                .flags
                .contains(CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN),
        )?;
        s.serialize_field(
            "remove_directories",
            &self.flags.contains(CleanerFlags::REMOVE_DIRECTORIES),
        )?;
        s.serialize_field(
            "remove_files",
            &self.flags.contains(CleanerFlags::REMOVE_FILES),
        )?;

        // Selection rules are written only when they are in use: an entry that
        // removes everything needs none of them, and a file full of empty
        // `<exclude/>` elements is harder to read than the data it describes.
        if !self.filter.exclude.is_empty() {
            s.serialize_field("exclude", &self.filter.exclude)?;
        }

        if let Some(days) = self.filter.older_than_seconds {
            s.serialize_field("older_than_days", &(days / 86_400))?;
        }

        if let Some(size) = self.filter.min_size {
            s.serialize_field("min_size", &size)?;
        }

        if let Some(size) = self.filter.max_size {
            s.serialize_field("max_size", &size)?;
        }

        if let Some(depth) = self.filter.max_depth {
            s.serialize_field("max_depth", &depth)?;
        }

        if !self.filter.skip_if_running.is_empty() {
            s.serialize_field("skip_if_running", &self.filter.skip_if_running)?;
        }

        if self.filter.to_trash {
            s.serialize_field("to_trash", &true)?;
        }

        s.end()
    }
}

// delegate Serialize to legacy-compatible impl
impl Serialize for CleanerData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.serialize_legacy(serializer)
    }
}

// INFO: Struct for clearing registry
// WARN: Windows only
#[cfg(windows)]
#[derive(Serialize, Deserialize, Clone)]
pub struct CleanerDataRegistry {
    #[serde(
        deserialize_with = "deserialize_shared_str",
        serialize_with = "serialize_arc_str"
    )]
    pub category: Arc<str>,
    #[serde(
        deserialize_with = "deserialize_shared_str",
        serialize_with = "serialize_arc_str"
    )]
    pub program: Arc<str>,
    #[serde(
        default = "default_class_arc",
        deserialize_with = "deserialize_shared_str_default",
        serialize_with = "serialize_arc_str"
    )]
    pub class: Arc<str>,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str",
        alias = "sub_class"
    )]
    pub sub_category: Arc<str>,

    #[serde(default)]
    pub remove_all_in_tree: bool,
    #[serde(default)]
    pub remove_all_in_registry: bool,

    #[serde(default)]
    pub path: SharedPath,

    #[serde(default, deserialize_with = "deserialize_vec_arc")]
    pub values_to_remove: Vec<Arc<str>>,

    #[serde(default, deserialize_with = "deserialize_vec_arc")]
    pub keys_to_remove: Vec<Arc<str>>,

    // INFO: Glob matched against the last segment(s) of resolved paths:
    // value names for remove_values, subkey names for remove_trees.
    // "true" = remove all values / the resolved key tree itself
    #[serde(default)]
    pub remove_values: String,

    #[serde(default)]
    pub remove_trees: String,
}

// INFO: Lightweight projection of CleanerData used for category scans. Unknown
// fields (files_to_remove, directories_to_remove, flags, class, ...) are ignored
// by serde and never allocated. `path` is kept only to detect the {drive}
// placeholder so entry counts stay identical to the full database.
#[derive(Deserialize, Clone)]
pub struct CleanerIndex {
    #[serde(default)]
    pub path: SharedPath,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str"
    )]
    pub category: Arc<str>,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str"
    )]
    pub program: Arc<str>,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str",
        alias = "sub_class",
        alias = "subCategory"
    )]
    pub sub_category: Arc<str>,
}

impl From<&CleanerData> for CleanerIndex {
    fn from(data: &CleanerData) -> Self {
        Self {
            path: data.path.clone(),
            category: Arc::clone(&data.category),
            program: Arc::clone(&data.program),
            sub_category: Arc::clone(&data.sub_category),
        }
    }
}

// INFO: Lightweight projection of CleanerDataRegistry for category scans.
// WARN: Windows only
#[cfg(windows)]
#[derive(Deserialize, Clone)]
pub struct RegistryIndex {
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str"
    )]
    pub category: Arc<str>,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str"
    )]
    pub program: Arc<str>,
    #[serde(
        default,
        deserialize_with = "deserialize_shared_opt",
        serialize_with = "serialize_arc_str",
        alias = "sub_class"
    )]
    pub sub_category: Arc<str>,
}

#[cfg(windows)]
impl From<&CleanerDataRegistry> for RegistryIndex {
    fn from(data: &CleanerDataRegistry) -> Self {
        Self {
            category: Arc::clone(&data.category),
            program: Arc::clone(&data.program),
            sub_category: Arc::clone(&data.sub_category),
        }
    }
}

fn default_class() -> String {
    String::from("Other")
}

// INFO: Built-in custom cleaning (defined in code, not JSON).
// Each entry describes: category, sub_category, target OS and the cleaning
// function itself. Functions are registered at runtime via
// database::custom_cleaners::register_custom_cleaner (see cleaner::custom_cleaners::register_all).
pub type CustomCleanFn =
    fn(
        &CustomCleaner,
        Option<tokio::sync::mpsc::Sender<String>>,
        bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CleanerResult> + Send>>;

#[derive(Clone)]
pub struct CustomCleaner {
    /// Unique id, used to enable/disable this cleaner from CLI/GUI
    pub id: String,
    pub program: Arc<str>,
    pub category: Arc<str>,
    pub sub_category: Arc<str>,
    /// Target file or directory. Supports {username} placeholder
    pub path: SharedPath,
    /// Extra arguments passed to the cleaning function
    pub args: Vec<String>,
    /// Operating systems this cleaning applies to (empty = all)
    /// Values: "windows", "linux", "macos"
    pub os: Vec<String>,
    /// The cleaning function that performs the custom cleanup
    pub function: CustomCleanFn,
    /// If true, this cleaner runs sequentially (not concurrent with others)
    /// so that its progress messages don't mix with other cleaners.
    pub sequential: bool,
}

impl CustomCleaner {
    pub fn matches_current_os(&self) -> bool {
        if self.os.is_empty() {
            return true;
        }
        let current = if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "macos"
        };
        self.os.iter().any(|os| os.eq_ignore_ascii_case(current))
    }
}

// INFO: Struct for task clearing (Result cleared)
pub struct CleanerResult {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub working: bool,
    /// The entry's own pattern, stored segment-interned for the same reason as
    /// [`ClearedPath::path`]: the database holds near-identical paths and this
    /// is one of them.
    pub path: SharedPath,
    /// One entry per item this cleaner actually deleted, with that item's own
    /// share of the counters. Empty for a cleaner that cannot name what it
    /// removed (a registry key, an entry that matched nothing).
    pub paths: Vec<ClearedPath>,
    /// Items deleted but not listed, because one cleaner's list hit its cap.
    pub paths_omitted: usize,
    pub program: Arc<str>,
    pub category: Arc<str>,
    pub sub_category: Arc<str>,
    /// Files found by a scan that something is holding open, so a real run
    /// would fail to remove them. Always zero for a run that removes.
    ///
    /// This is what keeps a dry run honest: the size it reports is the size that
    /// is *reachable*, and this is the part of the tree that is not. A scan that
    /// left it out would promise bytes that a real run cannot deliver.
    pub locked_files: u64,
    /// Bytes those locked files hold — the size a scan is not promising.
    pub locked_bytes: u64,
    /// Named locked files, capped the same way [`Self::paths`] is.
    pub locked: Vec<ClearedPath>,
    /// Files a scan found the ACL refuses to delete. Reported apart from
    /// [`Self::locked_files`] because the remedy differs: elevation, not closing
    /// the application that is holding them.
    pub denied_files: u64,
    /// Bytes those denied files hold.
    pub denied_bytes: u64,
    /// Named denied files, capped the same way [`Self::paths`] is.
    pub denied: Vec<ClearedPath>,
}
