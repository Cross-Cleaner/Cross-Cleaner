/// Per-entry statistics returned by a `custom_glob_cleaner!` body.
/// Aggregated by the macro into the final `CleanerResult`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GlobCleanStats {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

impl GlobCleanStats {
    /// One removed file with `bytes` size
    pub fn file(bytes: u64) -> Self {
        Self {
            files: 1,
            folders: 0,
            bytes,
        }
    }

    /// One removed folder
    pub fn folder() -> Self {
        Self {
            files: 0,
            folders: 1,
            bytes: 0,
        }
    }
}

/// Register a custom cleaning that works on a glob pattern.
///
/// The body is executed for EVERY file/folder matched by the glob pattern.
/// Inside the body you describe what to do with the entry (`path`).
/// Return `Ok(GlobCleanStats)` to count the action or `Err(_)` to skip the entry.
///
/// Generated wrapper expands `*` (done by the registry), runs
/// `$crate::glob::glob` on `path` and aggregates results.
///
/// Note: glob patterns must use forward slashes (`/`).
///
/// Forms:
/// ```ignore
/// // Full: with os filter, args, and sequential flag
/// let _ = cleaner::custom_glob_cleaner! {
///     id: "my_cleaner",
///     program: "Custom: My cleaner",
///     category: "Logs",
///     sub_category: "Logs",
///     os: ["windows"],                    // [] = all OS
///     sequential: false,                  // true = runs one at a time
///     args: ["7"],                        // [] = no args
///     glob: "C:/ProgramData/MyApp/*.tmp",
///     |path, args, dry_run| {
///         let bytes = std::fs::metadata(path)?.len();
///         if !dry_run {
///             std::fs::remove_file(path)?;
///         }
///         Ok(cleaner::custom_cleaners::GlobCleanStats::file(bytes))
///     }
/// };
///
/// // Without args: `|path, dry_run|` or `|path|`
/// // Without os (all OS): omit `os: [...]` line
/// ```
#[macro_export]
macro_rules! custom_glob_cleaner {
    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        sequential: $seq:expr,
        glob: $pattern:expr,
        |$path:ident, $args:ident, $dry_run:ident| $body:block
    ) => {{
        fn __custom_glob_entry(
            $path: &std::path::Path,
            $args: &[String],
            $dry_run: bool,
        ) -> Result<$crate::custom_cleaners::GlobCleanStats, std::io::Error> $body

        fn __custom_glob_wrapper(
            data: &$crate::database::structures::CustomCleaner,
            sender: Option<tokio::sync::mpsc::Sender<String>>,
            dry_run: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::database::structures::CleanerResult> + Send>> {
            let data = data.clone();
            Box::pin(async move {
                use std::sync::atomic::{AtomicU64, Ordering};
                use rayon::iter::IntoParallelRefIterator;
                use rayon::iter::ParallelIterator;

                let mut result = $crate::database::structures::CleanerResult {
                    files: 0,
                    folders: 0,
                    bytes: 0,
                    working: false,
                    path: data.path.clone(),
                    paths: Vec::new(),
                    paths_omitted: 0,
                    program: data.program.clone(),
                    category: data.category.clone(),
                    sub_category: data.sub_category.clone(),
                    // A custom cleaner runs its own body per matched entry rather
                    // than walking deletability flags ahead of time, so locked/denied
                    // stay zero.
                    locked_files: 0,
                    locked_bytes: 0,
                    locked: Vec::new(),
                    denied_files: 0,
                    denied_bytes: 0,
                    denied: Vec::new(),
                };

                // Stream glob without materializing full Vec<PathBuf> at once.
                // Entries are processed in bounded chunks (1024) to limit peak RAM
                // when glob expands to tens of thousands of files (e.g. Pictures/**/*).
                let path_str = data.path.to_string();
                let glob_iter = match ::glob::glob(&path_str) {
                    Ok(g) => g,
                    Err(_) => return result,
                };

                let completed = AtomicU64::new(0);
                let total_bytes = AtomicU64::new(0);
                let total_files = AtomicU64::new(0);
                let total_folders = AtomicU64::new(0);
                let mut total: u64 = 0;

                // Per-path detail, for the results overlay to list. The paths are
                // interned segment by segment, so N entries below one directory
                // share a single copy of it instead of storing N.
                let removed: std::sync::Mutex<Vec<$crate::database::structures::ClearedPath>> =
                    std::sync::Mutex::new(Vec::new());

                // Bounded chunk to keep peak PathBuf allocation low.
                let mut chunk: Vec<std::path::PathBuf> = Vec::with_capacity(1024);
                let mut any = false;
                for entry in glob_iter.filter_map(Result::ok) {
                    any = true;
                    chunk.push(entry);
                    if chunk.len() == 1024 {
                        total += chunk.len() as u64;
                        let completed_ref = &completed;
                        let total_bytes_ref = &total_bytes;
                        let total_files_ref = &total_files;
                        let total_folders_ref = &total_folders;
                        let sender_ref = &sender;
                        let removed_ref = &removed;
                        chunk.par_iter().for_each(|entry_path| {
                            if let Ok(stats) = __custom_glob_entry(entry_path, &data.args, dry_run) {
                                if stats.files > 0 || stats.folders > 0 || stats.bytes > 0 {
                                    total_files_ref.fetch_add(stats.files, Ordering::Relaxed);
                                    total_folders_ref.fetch_add(stats.folders, Ordering::Relaxed);
                                    total_bytes_ref.fetch_add(stats.bytes, Ordering::Relaxed);
                                    if let Ok(mut removed) = removed_ref.lock() {
                                        removed.push($crate::database::structures::ClearedPath {
                                            path: $crate::database::structures::SharedPath::new(
                                                &entry_path.to_string_lossy(),
                                            ),
                                            removed_bytes: stats.bytes,
                                            removed_files: stats.files,
                                            removed_directories: stats.folders,
                                        });
                                    }
                                }
                            }
                            let cur = completed_ref.fetch_add(1, Ordering::Relaxed) + 1;
                            if cur % 5 == 0 {
                                let bytes = total_bytes_ref.load(Ordering::Relaxed);
                                if let Some(s) = sender_ref.as_ref() {
                                    let _ = s.try_send(format!(
                                        "Compressing {}/{} files... {}",
                                        cur,
                                        cur,
                                        $crate::database::utils::get_file_size_string(bytes)
                                    ));
                                }
                            }
                        });
                        chunk.clear();
                    }
                }
                if !any {
                    return result;
                }
                if !chunk.is_empty() {
                    total += chunk.len() as u64;
                    let removed_ref = &removed;
                    chunk.par_iter().for_each(|entry_path| {
                        if let Ok(stats) = __custom_glob_entry(entry_path, &data.args, dry_run) {
                            if stats.files > 0 || stats.folders > 0 || stats.bytes > 0 {
                                total_files.fetch_add(stats.files, Ordering::Relaxed);
                                total_folders.fetch_add(stats.folders, Ordering::Relaxed);
                                total_bytes.fetch_add(stats.bytes, Ordering::Relaxed);
                                if let Ok(mut removed) = removed_ref.lock() {
                                    removed.push($crate::database::structures::ClearedPath {
                                        path: $crate::database::structures::SharedPath::new(
                                            &entry_path.to_string_lossy(),
                                        ),
                                        removed_bytes: stats.bytes,
                                        removed_files: stats.files,
                                        removed_directories: stats.folders,
                                    });
                                }
                            }
                        }
                        let _ = completed.fetch_add(1, Ordering::Relaxed);
                    });
                }

                if let Some(s) = sender.as_ref() {
                    let bytes = total_bytes.load(Ordering::Relaxed);
                    let done = completed.load(Ordering::Relaxed);
                    let _ = s.try_send(format!(
                        "Compressing {}/{} files... {}",
                        done,
                        total,
                        $crate::database::utils::get_file_size_string(bytes)
                    ));
                }

                result.files = total_files.load(Ordering::Relaxed);
                result.folders = total_folders.load(Ordering::Relaxed);
                result.bytes = total_bytes.load(Ordering::Relaxed);
                result.working = result.files > 0 || result.folders > 0;
                result.paths = removed.into_inner().unwrap_or_default();

                result
            })
        }

        $crate::database::custom_cleaners::register_custom_cleaner(
            $crate::database::structures::CustomCleaner {
                id: String::from($id),
                program: $crate::database::structures::intern_arc($program),
                category: $crate::database::structures::intern_arc($category),
                sub_category: $crate::database::structures::intern_arc($sub_category),
                path: String::from($pattern).into(),
                args: vec![],
                os: vec![$(String::from($os)),*],
                function: __custom_glob_wrapper,
                sequential: $seq,
            },
        )
    }};

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        sequential: $seq:expr,
        glob: $pattern:expr,
        |$path:ident, $args:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [$($os),*],
            sequential: $seq,
            glob: $pattern,
            |$path, $args, __custom_glob_ignored_dry_run| $body
        }
    };

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        sequential: $seq:expr,
        glob: $pattern:expr,
        |$path:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [$($os),*],
            sequential: $seq,
            glob: $pattern,
            |$path, __custom_glob_ignored_args, __custom_glob_ignored_dry_run| $body
        }
    };

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        glob: $pattern:expr,
        |$path:ident, $args:ident, $dry_run:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [$($os),*],
            sequential: false,
            glob: $pattern,
            |$path, $args, $dry_run| $body
        }
    };

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        glob: $pattern:expr,
        |$path:ident, $dry_run:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [$($os),*],
            sequential: false,
            glob: $pattern,
            |$path, __custom_glob_ignored_args, $dry_run| $body
        }
    };

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        os: [$($os:expr),* $(,)?],
        glob: $pattern:expr,
        |$path:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [$($os),*],
            sequential: false,
            glob: $pattern,
            |$path, __custom_glob_ignored_args, __custom_glob_ignored_dry_run| $body
        }
    };

    (
        id: $id:expr,
        program: $program:expr,
        category: $category:expr,
        sub_category: $sub_category:expr,
        glob: $pattern:expr,
        |$path:ident| $body:block
    ) => {
        $crate::custom_glob_cleaner! {
            id: $id,
            program: $program,
            category: $category,
            sub_category: $sub_category,
            os: [],
            sequential: false,
            glob: $pattern,
            |$path, __custom_glob_ignored_args, __custom_glob_ignored_dry_run| $body
        }
    };
}

/// Register every built-in custom cleaner.
///
/// To add a new custom cleaning:
/// 1. Write a function below: `fn my_cleaning(data: &CustomCleaner) -> CleanerResult`
/// 2. Add one registration line here with metadata:
///    id, program, category, sub_category, path, args, os (empty = all OS).
/// 3. Or use the `custom_glob_cleaner!` macro (see its docs).
pub fn register_all() {
    // Removes the CodeContainers.Offline entry (offline recent projects list)
    // from Visual Studio ApplicationPrivateSettings.xml
    #[cfg(windows)]
    let _ = custom_glob_cleaner! {
        id: "VSCode clearing recent projects",
        program: "Visual Studio",
        category: "LastActivity",
        sub_category: "Recent Documents",
        os: ["windows"],
        glob: "{drive}/Users/*/AppData/Local/Microsoft/VisualStudio/*/ApplicationPrivateSettings.xml",
        |path, dry_run| {
            remove_code_containers_offline(path, dry_run)
        }
    };

    // Removes the FileLists section (recent files list) from dnSpy.xml
    #[cfg(windows)]
    let _ = custom_glob_cleaner! {
        id: "dnSpy clearing recent files",
        program: "dnSpy",
        category: "LastActivity",
        sub_category: "Recent Documents",
        os: ["windows"],
        glob: "{drive}/Users/*/AppData/Roaming/dnSpy/dnSpy.xml",
        |path, dry_run| {
            remove_dnspy_file_lists(path, dry_run)
        }
    };

    // Image optimizer: Pictures recursive
    #[cfg(windows)]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures",
        program: "Windows",
        category: "Images",
        sub_category: "Compress",
        os: ["windows"],
        sequential: true,
        glob: "{drive}\\Users\\*\\Pictures\\**\\*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };

    // Image optimizer: ShareX Screenshots recursive
    #[cfg(windows)]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures in ShareX",
        program: "ShareX",
        category: "Images",
        sub_category: "Compress",
        os: ["windows"],
        sequential: true,
        glob: "{drive}\\Users\\*\\Documents\\ShareX\\Screenshots\\*\\*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };

    #[cfg(target_os = "android")]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures in Screenshots",
        program: "System",
        category: "Images",
        sub_category: "Compress",
        os: ["android"],
        sequential: true,
        glob: "/storage/emulated/0/Pictures/Screenshots/*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };

    #[cfg(target_os = "android")]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures in System",
        program: "System",
        category: "Images",
        sub_category: "Compress",
        os: ["android"],
        sequential: true,
        glob: "/storage/emulated/0/Pictures/**/*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };

    #[cfg(target_os = "linux")]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures",
        program: "Linux",
        category: "Images",
        sub_category: "Compress",
        os: ["linux"],
        sequential: true,
        glob: "/home/*/Pictures/**/*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };

    #[cfg(target_os = "linux")]
    let _ = custom_glob_cleaner! {
        id: "Optimize pictures",
        program: "Omarchy",
        category: "Images",
        sub_category: "Compress",
        os: ["linux"],
        sequential: true,
        glob: "/home/*/.local/state/omarchy/clipboard-images/*",
        |path, _args, dry_run| {
            crate::image_optimizer::optimize_single_with(path, dry_run)
        }
    };
}

// Visual Studio
#[cfg(windows)]
fn remove_code_containers_offline(
    path: &std::path::Path,
    dry_run: bool,
) -> std::io::Result<crate::custom_cleaners::GlobCleanStats> {
    const SETTING_START: &str = "<collection name=\"CodeContainers.Offline\">";
    const SETTING_END: &str = "</collection>";

    let content = std::fs::read_to_string(path)?;

    let Some(start) = content.find(SETTING_START) else {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    };
    let Some(end_rel) = content[start..].find(SETTING_END) else {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    };
    let end = start + end_rel + SETTING_END.len();

    if !dry_run {
        let mut new_content = String::with_capacity(content.len() - (end - start));
        new_content.push_str(&content[..start]);
        new_content.push_str(&content[end..]);

        std::fs::write(path, new_content)?;
    }

    Ok(crate::custom_cleaners::GlobCleanStats::file(
        (end - start) as u64,
    ))
}
/// Removes the whole `<section _="FileLists" name="(Default)">...</section>`
/// element (with nested FileList/File entries - the recent files list)
/// from dnSpy.xml. Handles nested `<section>` blocks and self-closing
/// `<section ... />` tags.
#[cfg(windows)]
fn remove_dnspy_file_lists(
    path: &std::path::Path,
    dry_run: bool,
) -> std::io::Result<crate::custom_cleaners::GlobCleanStats> {
    const SECTION_START: &str = "<section _=\"FileLists\"";
    const OPEN: &str = "<section";
    const CLOSE: &str = "</section>";

    let content = std::fs::read_to_string(path)?;

    let Some(start) = content.find(SECTION_START) else {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    };

    // Opening tag ends at its first '>'
    let Some(tag_end_rel) = content[start..].find('>') else {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    };
    let mut pos = start + tag_end_rel + 1;
    let mut depth = 1usize;

    while depth > 0 {
        let open_idx = content[pos..].find(OPEN).map(|i| pos + i);
        let close_idx = content[pos..].find(CLOSE).map(|i| pos + i);

        let (is_open, idx) = match (open_idx, close_idx) {
            (Some(o), Some(c)) => {
                if o < c {
                    (true, o)
                } else {
                    (false, c)
                }
            }
            (Some(o), None) => (true, o),
            (None, Some(c)) => (false, c),
            (None, None) => return Ok(crate::custom_cleaners::GlobCleanStats::default()),
        };

        if is_open {
            let tag_end = content[idx..]
                .find('>')
                .map(|i| idx + i)
                .unwrap_or(content.len());
            let self_closing = content[idx..tag_end].trim_end().ends_with('/');
            pos = tag_end + 1;
            if !self_closing {
                depth += 1;
            }
        } else {
            depth -= 1;
            pos = idx + CLOSE.len();
        }
    }

    if !dry_run {
        let mut new_content = String::with_capacity(content.len() - (pos - start));
        new_content.push_str(&content[..start]);
        new_content.push_str(&content[pos..]);

        std::fs::write(path, new_content)?;
    }

    Ok(crate::custom_cleaners::GlobCleanStats::file(
        (pos - start) as u64,
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn vscode_dry_run_measures_without_modifying_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ApplicationPrivateSettings.xml");
        let xml = "<root><collection name=\"CodeContainers.Offline\"><value>abc</value></collection><other/></root>";
        std::fs::write(&path, xml).unwrap();

        let dry = remove_code_containers_offline(&path, true).unwrap();
        assert_eq!(dry.files, 1);
        assert!(dry.bytes > 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), xml);

        let real = remove_code_containers_offline(&path, false).unwrap();
        assert_eq!(dry, real);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "<root><other/></root>"
        );
    }

    #[test]
    fn dnspy_dry_run_measures_without_modifying_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dnSpy.xml");
        let xml = "<settings><section _=\"FileLists\" name=\"(Default)\"><section name=\"item\"/></section><keep/></settings>";
        std::fs::write(&path, xml).unwrap();

        let dry = remove_dnspy_file_lists(&path, true).unwrap();
        assert_eq!(dry.files, 1);
        assert!(dry.bytes > 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), xml);

        let real = remove_dnspy_file_lists(&path, false).unwrap();
        assert_eq!(dry, real);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "<settings><keep/></settings>"
        );
    }
}
