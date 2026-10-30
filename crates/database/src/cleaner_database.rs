use crate::CleanerData;
use crate::registry_utils::get_steam_directory_from_registry;
use crate::streaming::for_each_array;
use crate::structures::CleanerIndex;
use disk_name::get_letters;
use flate2::read::GzDecoder;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Cached, OS-specific values used to expand placeholders in database paths.
struct ExpansionContext {
    username: String,
    drives: Vec<String>,
    steam: String,
}

fn expansion() -> &'static ExpansionContext {
    static CTX: OnceLock<ExpansionContext> = OnceLock::new();
    CTX.get_or_init(|| {
        // INFO: Get the username
        let username = whoami::username().expect("Failed to get username");

        // INFO: Getting a list of disks
        // WARN: Windows only
        let drives = if cfg!(windows) { get_letters() } else { vec![] };

        // INFO: Get the path to Steam
        // WARN: Windows only
        let steam = if cfg!(windows) {
            get_steam_directory_from_registry()
        } else {
            String::new()
        };

        ExpansionContext {
            username,
            drives,
            steam,
        }
    })
}

/// Expand {username}, {steam} and {drive} in an owned entry, invoking `f` for
/// each resulting entry ({drive} yields one entry per drive letter).
fn expand_into<F: FnMut(CleanerData)>(mut entry: CleanerData, ctx: &ExpansionContext, f: &mut F) {
    entry.path = entry.path.replace("{username}", &ctx.username);
    entry.path = entry.path.replace("{steam}", &ctx.steam);

    if cfg!(windows) && entry.path.contains("{drive}") {
        for drive in &ctx.drives {
            let mut drive_entry = entry.clone();
            drive_entry.path = drive_entry.path.replace("{drive}", drive);
            f(drive_entry);
        }
    } else {
        f(entry);
    }
}

/// Emit a lightweight index entry once per expanded path. Mirrors
/// [`expand_into`]'s {drive} behaviour (one call per drive, zero if no drives)
/// without replacing {username}/{steam}, since the indexed path is only used to
/// detect the {drive} placeholder.
fn emit_index<F: FnMut(CleanerIndex)>(entry: CleanerIndex, ctx: &ExpansionContext, f: &mut F) {
    if cfg!(windows) && entry.path.contains("{drive}") {
        for _ in 0..ctx.drives.len() {
            f(entry.clone());
        }
    } else {
        f(entry);
    }
}

/// Where the cleaner database is read from.
#[derive(Clone)]
enum CleanerSource {
    /// Built-in database compiled into the binary (minified + gzip).
    Default,
    /// Database file supplied on the command line (plain JSON array).
    File(PathBuf),
    /// In-memory database (tests).
    Memory(Arc<[CleanerData]>),
    /// Another source with extra entries layered on top (user scripts).
    ///
    /// Kept as a wrapper instead of merging eagerly so the built-in database
    /// keeps its streaming property: `with_entries` is called with a handful of
    /// script entries, and the compressed catalog behind `base` is still
    /// decompressed entry by entry on each walk.
    WithEntries {
        base: Box<CleanerSource>,
        entries: Arc<[CleanerData]>,
    },
}

/// Lazy handle over the cleaner database. Stores only the source (a path or a
/// marker); entries are streamed on demand via [`CleanerDatabase::for_each`],
/// so the full database is never resident in memory.
#[derive(Clone)]
pub struct CleanerDatabase {
    source: CleanerSource,
}

impl CleanerDatabase {
    /// Built-in database for the current operating system.
    pub fn default_source() -> Self {
        Self {
            source: CleanerSource::Default,
        }
    }

    /// Database loaded from an external JSON file.
    pub fn from_file<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            source: CleanerSource::File(path.into()),
        }
    }

    /// In-memory database (mainly for tests).
    pub fn from_vec(entries: Vec<CleanerData>) -> Self {
        Self {
            source: CleanerSource::Memory(entries.into()),
        }
    }

    /// Layer `entries` on top of this database.
    ///
    /// This is how user-registered rules (see the `mods-api` crate) join the
    /// catalog: the base keeps streaming from its own source, and the extra
    /// entries are expanded after it. An empty set returns the database
    /// unchanged, so a caller does not have to special-case "no scripts".
    pub fn with_entries<I>(self, entries: I) -> Self
    where
        I: IntoIterator<Item = CleanerData>,
    {
        let entries: Vec<CleanerData> = entries.into_iter().collect();

        if entries.is_empty() {
            return self;
        }

        Self {
            source: CleanerSource::WithEntries {
                base: Box::new(self.source),
                entries: entries.into(),
            },
        }
    }

    /// Stream every expanded entry through `f`. Re-reads and re-decompresses
    /// the source on each call; only one entry is alive at a time.
    pub fn for_each<F>(&self, mut f: F) -> Result<(), Box<dyn Error>>
    where
        F: FnMut(CleanerData),
    {
        stream_entries(&self.source, expansion(), &mut f)
    }

    /// Stream lightweight index entries ([`CleanerIndex`]) through `f`. Heavy
    /// fields (paths' file/dir lists, flags, class) are never deserialized, so
    /// scanning for categories/programs allocates far less than [`Self::for_each`].
    pub fn for_each_index<F>(&self, mut f: F) -> Result<(), Box<dyn Error>>
    where
        F: FnMut(CleanerIndex),
    {
        stream_index(&self.source, expansion(), &mut f)
    }
}

/// Walk `source`, handing every expanded entry to `f`.
fn stream_entries<F>(
    source: &CleanerSource,
    ctx: &ExpansionContext,
    f: &mut F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(CleanerData),
{
    match source {
        CleanerSource::Default => {
            #[cfg(target_os = "linux")]
            // NOTE: DataBase for Linux and Unix (minified and compressed at compile time)
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/linux_database.min.json.gz"));
            #[cfg(windows)]
            // NOTE: DataBase for Windows (minified and compressed at compile time)
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/windows_database.min.json.gz"));
            #[cfg(target_os = "macos")]
            // NOTE: DataBase for MacOS (minified and compressed at compile time)
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/macos_database.min.json.gz"));
            #[cfg(target_os = "android")]
            // NOTE: DataBase for Android (minified and compressed at compile time)
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/android_database.min.json.gz"));
            #[cfg(not(any(
                target_os = "linux",
                target_os = "windows",
                target_os = "macos",
                target_os = "android"
            )))]
            let compressed_data: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/linux_database.min.json.gz"));

            let decoder = GzDecoder::new(&compressed_data[..]);
            for_each_array(decoder, |entry| expand_into(entry, ctx, &mut *f))?;
        }
        CleanerSource::File(path) => {
            let reader = BufReader::new(File::open(path)?);
            for_each_array(reader, |entry| expand_into(entry, ctx, &mut *f))?;
        }
        CleanerSource::Memory(entries) => {
            for entry in entries.iter().cloned() {
                expand_into(entry, ctx, &mut *f);
            }
        }
        CleanerSource::WithEntries { base, entries } => {
            stream_entries(base, ctx, f)?;

            for entry in entries.iter().cloned() {
                expand_into(entry, ctx, &mut *f);
            }
        }
    }

    Ok(())
}

/// Streaming counterpart of [`stream_entries`] for [`CleanerIndex`].
fn stream_index<F>(
    source: &CleanerSource,
    ctx: &ExpansionContext,
    f: &mut F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(CleanerIndex),
{
    match source {
        CleanerSource::Default => {
            #[cfg(target_os = "linux")]
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/linux_database.min.json.gz"));
            #[cfg(windows)]
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/windows_database.min.json.gz"));
            #[cfg(target_os = "macos")]
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/macos_database.min.json.gz"));
            #[cfg(target_os = "android")]
            let compressed_data =
                include_bytes!(concat!(env!("OUT_DIR"), "/android_database.min.json.gz"));
            #[cfg(not(any(
                target_os = "linux",
                target_os = "windows",
                target_os = "macos",
                target_os = "android"
            )))]
            let compressed_data: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/linux_database.min.json.gz"));

            let decoder = GzDecoder::new(&compressed_data[..]);
            for_each_array(decoder, |entry| emit_index(entry, ctx, &mut *f))?;
        }
        CleanerSource::File(path) => {
            let reader = BufReader::new(File::open(path)?);
            for_each_array(reader, |entry| emit_index(entry, ctx, &mut *f))?;
        }
        CleanerSource::Memory(entries) => {
            for entry in entries.iter() {
                emit_index(CleanerIndex::from(entry), ctx, &mut *f);
            }
        }
        CleanerSource::WithEntries { base, entries } => {
            stream_index(base, ctx, f)?;

            for entry in entries.iter() {
                emit_index(CleanerIndex::from(entry), ctx, &mut *f);
            }
        }
    }

    Ok(())
}

/// Fully materialize the built-in database (convenience; loads everything in RAM).
pub fn get_default_database() -> Arc<[CleanerData]> {
    static DATABASE: OnceLock<Arc<[CleanerData]>> = OnceLock::new();

    DATABASE
        .get_or_init(|| {
            let mut entries = Vec::new();
            CleanerDatabase::default_source()
                .for_each(|entry| entries.push(entry))
                .expect("Failed to parse database");
            entries.into()
        })
        .clone()
}

/// Fully materialize a database from an external file.
pub fn get_database_from_file(file_path: &str) -> Result<Arc<[CleanerData]>, Box<dyn Error>> {
    let mut entries = Vec::new();
    CleanerDatabase::from_file(file_path).for_each(|entry| entries.push(entry))?;
    Ok(entries.into())
}
