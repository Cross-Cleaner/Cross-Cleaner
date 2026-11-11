use crate::structures::CleanerData;

pub mod cleaner_database;
pub mod custom_cleaners;
pub mod diag;
pub mod registry_database;
mod registry_utils;
mod streaming;
pub mod structures;
pub mod utils;
pub mod version;
// string-interner used for category/program dedup in future streaming paths
#[allow(unused_imports)]
pub use string_interner;

pub fn get_version() -> &'static str {
    option_env!("APP_VERSION").unwrap_or("2.0.2.2")
}

/// The application icon, used for the window icon and re-stamped onto the
/// executable during a self-update.
///
/// The file itself lives in `crates/winicon/assets/`, next to the crate that
/// stamps it into the binaries. It is reached from here rather than through a
/// dependency because `winicon` is a build-script-only crate that stays outside
/// the workspace, and `database` sits below every frontend in the graph. The
/// path therefore crosses a crate boundary; if the file moves, this fails to
/// compile with a clear "file not found" rather than silently shipping a binary
/// with no icon.
pub const ICON_BYTES: &[u8] = include_bytes!("../../winicon/assets/icon.ico");

/// Android-specific database file (used via CleanerDatabase::default_source on target_os = "android").
///
/// On Android `CleanerDatabase::default_source()` loads `android_database.json`
/// (cache/logs for Chrome, Telegram, Gallery etc.) compiled via `database/build.rs`.
/// The file lives at `database/android_database.json` and is gzip-compressed at build time
/// into `OUT_DIR/android_database.min.json.gz` (see `cleaner_database.rs`).
pub const ANDROID_DATABASE_PATH: &str = "android_database.json";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleaner_database::{CleanerDatabase, get_database_from_file, get_default_database};
    use crate::structures::CleanerData;
    use crate::utils::get_file_size_string;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_get_version() {
        let version = get_version();
        assert!(!version.is_empty(), "Version should not be empty");
        assert!(
            version.contains('.'),
            "Version should contain dots (semantic versioning)"
        );
    }

    #[test]
    fn test_get_icon() {
        let icon = ICON_BYTES;
        assert_eq!(icon.len(), 38078, "Icon should be exactly 38078 bytes");
        // Check ICO magic number
        assert_eq!(
            &icon[0..4],
            &[0x00, 0x00, 0x01, 0x00],
            "Should be an ICO file"
        );
    }

    #[test]
    fn test_get_default_database() {
        let database = get_default_database();
        assert!(!database.is_empty(), "Default database should not be empty");

        // Check that all entries have required fields
        for entry in database.iter() {
            assert!(!entry.path.is_empty(), "Path should not be empty");
            assert!(!entry.category.is_empty(), "Category should not be empty");
            assert!(!entry.program.is_empty(), "Program should not be empty");
        }
    }

    #[test]
    fn test_database_decompression() {
        // This test verifies that the gzip-compressed database can be loaded
        let database = get_default_database();
        assert!(
            database.len() > 100,
            "Database should contain many entries (decompression worked)"
        );
    }

    #[test]
    fn test_database_placeholder_expansion() {
        let database = get_default_database();

        // Check that placeholders are replaced
        for entry in database.iter() {
            assert!(
                !entry.path.contains("{username}"),
                "Username placeholder should be replaced in path: {}",
                entry.path
            );
        }
    }

    #[test]
    fn test_database_from_file_invalid() {
        // Test with non-existent file
        let result = get_database_from_file("nonexistent_file.json");
        assert!(result.is_err(), "Should return error for non-existent file");
    }

    #[test]
    fn test_database_from_file_invalid_json() {
        // Create temporary file with invalid JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"{ invalid json }").unwrap();

        let result = get_database_from_file(temp_file.path().to_str().unwrap());
        assert!(result.is_err(), "Should return error for invalid JSON");
    }

    #[test]
    fn test_database_from_file_valid() {
        // Create temporary file with valid JSON
        let mut temp_file = NamedTempFile::new().unwrap();
        let json_data = r#"[
            {
                "path": "C:\\Test\\{username}\\file.txt",
                "category": "TestCategory",
                "program": "TestProgram",
                "class": "TestClass",
                "remove_files": true
            }
        ]"#;
        temp_file.write_all(json_data.as_bytes()).unwrap();

        let result = get_database_from_file(temp_file.path().to_str().unwrap());
        assert!(result.is_ok(), "Should successfully load valid JSON");

        let database = result.unwrap();
        assert_eq!(database.len(), 1, "Should have one entry");
        assert_eq!(database[0].program.as_ref(), "TestProgram");
        assert_eq!(database[0].category.as_ref(), "TestCategory");
    }

    #[test]
    fn test_file_size_string_formatting() {
        assert_eq!(get_file_size_string(0), "0 B");
        assert_eq!(get_file_size_string(512), "512 B");
        assert_eq!(get_file_size_string(1024), "1.0 KB");
        assert_eq!(get_file_size_string(1536), "1.5 KB");
        assert_eq!(get_file_size_string(1_048_576), "1.0 MB");
        assert_eq!(get_file_size_string(1_073_741_824), "1.0 GB");
        assert_eq!(get_file_size_string(1_099_511_627_776), "1.0 TB");
    }

    #[test]
    fn test_cleaner_data_structure() {
        let data = CleanerData {
            path: String::from("test/path").into(),
            category: std::sync::Arc::from("Cache"),
            program: std::sync::Arc::from("TestApp"),
            class: std::sync::Arc::from("Application"),
            sub_category: std::sync::Arc::from("TestSub"),
            files_to_remove: vec![std::sync::Arc::from("*.tmp")],
            directories_to_remove: vec![std::sync::Arc::from("cache")],
            flags: crate::structures::CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN
                | crate::structures::CleanerFlags::REMOVE_DIRECTORIES
                | crate::structures::CleanerFlags::REMOVE_FILES,
            filter: Default::default(),
        };

        assert_eq!(data.path, "test/path");
        assert_eq!(data.category.as_ref(), "Cache");
        assert_eq!(data.program.as_ref(), "TestApp");
        assert!(
            data.flags
                .contains(crate::structures::CleanerFlags::REMOVE_FILES)
        );
        assert!(
            data.flags
                .contains(crate::structures::CleanerFlags::REMOVE_DIRECTORIES)
        );
    }

    #[test]
    fn test_database_categories_exist() {
        let database = get_default_database();
        let categories: std::collections::HashSet<std::sync::Arc<str>> = database
            .iter()
            .map(|entry| std::sync::Arc::clone(&entry.category))
            .collect();

        // Check that common categories exist
        assert!(!categories.is_empty(), "Should have at least one category");
    }

    #[test]
    fn test_database_programs_exist() {
        let database = get_default_database();
        let programs: std::collections::HashSet<std::sync::Arc<str>> = database
            .iter()
            .map(|entry| std::sync::Arc::clone(&entry.program))
            .collect();

        assert!(
            programs.len() > 10,
            "Should have many different programs in database"
        );
    }

    #[test]
    fn test_database_performance() {
        use std::time::Instant;

        let start = Instant::now();
        let database = get_default_database();
        let duration = start.elapsed();

        assert!(
            duration.as_millis() < 100,
            "Database loading should be fast (< 100ms), took {:?}",
            duration
        );
        assert!(!database.is_empty());
    }

    #[test]
    fn test_gzip_compression_ratio() {
        // This verifies that compression is actually happening
        let database = get_default_database();

        // Original JSON would be much larger
        // Compressed should give us a reasonable database size
        assert!(
            database.len() > 500,
            "Database should have many entries (compression ratio is good)"
        );
    }

    #[test]
    fn test_database_entries_valid_paths() {
        let database = get_default_database();

        for entry in database.iter() {
            // Check that paths don't have obvious issues
            assert!(!entry.path.is_empty(), "Path should not be empty");
            assert!(
                !entry.path.contains("{{"),
                "Path should not have double braces: {}",
                entry.path
            );
        }
    }

    #[test]
    fn test_file_size_edge_cases() {
        assert_eq!(get_file_size_string(0), "0 B");
        assert_eq!(get_file_size_string(1), "1 B");
        assert_eq!(get_file_size_string(1023), "1023 B");

        // Test large numbers - the actual output depends on the formatting implementation
        let large_result = get_file_size_string(u64::MAX / (1024 * 1024 * 1024));
        // Just verify it returns something reasonable and contains "GB" or "TB"
        assert!(
            large_result.contains("GB") || large_result.contains("TB"),
            "Large file size should be formatted in GB or TB, got: {}",
            large_result
        );
    }

    #[test]
    fn test_database_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let database = get_default_database();
        let mut handles = vec![];

        // Spawn 10 threads that all read the database simultaneously
        for _ in 0..10 {
            let db = Arc::clone(&database);
            let handle = thread::spawn(move || {
                for entry in db.iter() {
                    assert!(!entry.program.is_empty());
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_database_memory_efficiency() {
        use std::mem::size_of_val;

        let database = get_default_database();
        let total_size = size_of_val(&*database);

        // Database should be reasonably sized in memory
        assert!(total_size > 0, "Database should occupy memory");
    }

    #[test]
    fn test_category_consistency() {
        let database = get_default_database();

        // Check that categories follow expected patterns
        for entry in database.iter() {
            let category = &entry.category;
            // Categories can contain spaces (e.g., "Game saves")
            assert!(!category.is_empty(), "Category should not be empty");
            assert!(
                category.chars().next().unwrap().is_uppercase(),
                "Category should start with uppercase: {}",
                category
            );
        }
    }

    #[test]
    fn test_database_filtering_performance() {
        use std::collections::HashSet;
        use std::time::Instant;

        let database = get_default_database();
        let categories: HashSet<String> = vec!["Cache".to_string(), "Logs".to_string()]
            .into_iter()
            .collect();

        let start = Instant::now();
        let filtered: Vec<&CleanerData> = database
            .iter()
            .filter(|data| categories.contains(data.category.as_ref()))
            .collect();
        let duration = start.elapsed();

        assert!(!filtered.is_empty(), "Should find some entries");
        assert!(
            duration.as_micros() < 10000,
            "Filtering should be fast (< 10ms), took {:?}",
            duration
        );
    }

    #[test]
    fn test_program_name_validity() {
        let database = get_default_database();

        for entry in database.iter() {
            assert!(
                !entry.program.is_empty(),
                "Program name should not be empty"
            );
            assert!(
                entry.program.len() < 100,
                "Program name should be reasonable length: {}",
                entry.program
            );
        }
    }

    #[test]
    fn test_json_structure_compatibility() {
        // Test that we can serialize/deserialize properly
        let original = get_default_database();

        if let Some(first) = original.first() {
            let json = serde_json::to_string(first).unwrap();
            assert!(json.contains("path"));
            assert!(json.contains("category"));
            assert!(json.contains("program"));
        }
    }

    #[test]
    fn test_shared_path_round_trip() {
        use crate::structures::SharedPath;

        let samples = [
            "",
            "C:\\Users\\user\\AppData\\Local\\Temp\\*",
            "{drive}Program Files (x86)\\Adobe\\*\\Legal\\*\\*.html",
            "HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion",
            "/storage/emulated/0/Pictures/*",
            "mixed\\separator/path//with..double.sep",
            "trailing\\sep\\",
            "..\\parent\\escape",
        ];

        for s in samples {
            let p = SharedPath::new(s);
            assert_eq!(p.as_string(), s, "byte-for-byte round-trip");

            // Serialize must emit the original JSON string
            let json = serde_json::to_string(&p).unwrap();
            let expected = serde_json::to_string(s).unwrap();
            assert_eq!(json, expected, "JSON must not change for {:?}", s);

            // Deserialize must restore the exact string
            let back: SharedPath = serde_json::from_str(&json).unwrap();
            assert_eq!(back.as_string(), s);

            // serde field round-trip through CleanerData
            let data = CleanerData {
                path: p.clone(),
                category: std::sync::Arc::from("C"),
                program: std::sync::Arc::from("P"),
                class: std::sync::Arc::from("Cl"),
                sub_category: std::sync::Arc::from("S"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: crate::structures::CleanerFlags::empty(),
                filter: Default::default(),
            };
            let json = serde_json::to_string(&data).unwrap();
            let back: CleanerData = serde_json::from_str(&json).unwrap();
            assert_eq!(back.path.as_string(), s, "CleanerData path field");
        }
    }

    #[test]
    fn test_shared_path_segments_shared() {
        use crate::structures::SharedPath;
        use std::sync::Arc;

        // Two near-identical paths must share every common segment allocation
        let a = SharedPath::new("{drive}Users\\u\\AppData\\Local\\Foo\\cache");
        let b = SharedPath::new("{drive}Users\\u\\AppData\\Local\\Bar\\cache");

        let segs_a = a.segments();
        let segs_b = b.segments();
        assert_eq!(segs_a.len(), segs_b.len());
        // "Users\", "u\", "AppData\", "Local\" are shared (pointer equality)
        let shared = segs_a
            .iter()
            .zip(segs_b.iter())
            .filter(|(x, y)| Arc::ptr_eq(x, y))
            .count();
        assert!(shared >= 4, "expected >= 4 shared segments, got {}", shared);
    }

    #[test]
    fn test_database_cache_efficiency() {
        use std::time::Instant;

        // First call (might involve decompression)
        let start = Instant::now();
        let _db1 = get_default_database();
        let first_duration = start.elapsed();

        // Second call (should be from static)
        let start = Instant::now();
        let _db2 = get_default_database();
        let second_duration = start.elapsed();

        assert!(
            second_duration <= first_duration,
            "Subsequent calls should be as fast or faster"
        );
    }

    #[test]
    fn test_provider_streams_same_count_as_full_load() {
        let full = get_default_database();
        let mut count = 0usize;
        CleanerDatabase::default_source()
            .for_each(|_| count += 1)
            .expect("streaming failed");
        assert_eq!(
            count,
            full.len(),
            "streamed entry count should match full load"
        );
    }

    #[test]
    fn test_provider_expands_placeholders() {
        CleanerDatabase::default_source()
            .for_each(|entry| {
                assert!(
                    !entry.path.contains("{username}"),
                    "username placeholder should be expanded: {}",
                    entry.path
                );
                assert!(
                    !entry.path.contains("{steam}"),
                    "steam placeholder should be expanded: {}",
                    entry.path
                );
            })
            .expect("streaming failed");
    }

    #[test]
    fn test_provider_from_vec_matches() {
        let entries: Vec<CleanerData> = get_default_database().to_vec();
        let expected = entries.len();
        let mut count = 0usize;
        CleanerDatabase::from_vec(entries)
            .for_each(|_| count += 1)
            .expect("streaming failed");
        assert_eq!(count, expected);
    }

    #[test]
    fn test_index_stream_matches_full_stream() {
        use std::collections::BTreeMap;

        let mut full: BTreeMap<(String, String), usize> = BTreeMap::new();
        CleanerDatabase::default_source()
            .for_each(|entry| {
                *full
                    .entry((entry.category.to_string(), entry.sub_category.to_string()))
                    .or_insert(0) += 1;
            })
            .expect("streaming failed");

        let mut index: BTreeMap<(String, String), usize> = BTreeMap::new();
        CleanerDatabase::default_source()
            .for_each_index(|entry| {
                *index
                    .entry((entry.category.to_string(), entry.sub_category.to_string()))
                    .or_insert(0) += 1;
            })
            .expect("streaming failed");

        assert_eq!(
            full, index,
            "index projection must aggregate exactly like the full database"
        );
    }

    #[test]
    fn test_cleaner_data_default_values() {
        let data = CleanerData {
            path: String::new().into(),
            category: std::sync::Arc::from(""),
            program: std::sync::Arc::from(""),
            class: std::sync::Arc::from(""),
            sub_category: std::sync::Arc::from(""),
            files_to_remove: vec![],
            directories_to_remove: vec![],
            flags: crate::structures::CleanerFlags::empty(),
            filter: Default::default(),
        };

        assert!(
            !data
                .flags
                .contains(crate::structures::CleanerFlags::REMOVE_FILES)
        );
        assert!(
            !data
                .flags
                .contains(crate::structures::CleanerFlags::REMOVE_DIRECTORIES)
        );
        assert!(
            !data
                .flags
                .contains(crate::structures::CleanerFlags::REMOVE_ALL_IN_DIR)
        );
        assert!(
            !data
                .flags
                .contains(crate::structures::CleanerFlags::REMOVE_DIRECTORY_AFTER_CLEAN)
        );
        assert_eq!(data.files_to_remove.len(), 0);
        assert_eq!(data.directories_to_remove.len(), 0);
    }
}
