//! End-to-end tests: scripts are written to a temporary directory, loaded, and
//! their effects are observed on the filesystem and in the collected registry.
//!
//! Loading mutates process-wide state (the collector), so every test holds the
//! same lock: the point here is what a real script does, not parallelism.

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use database::structures::Cleared;

/// Guards the process-wide script collector.
fn lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A path as a Rune string literal: forward slashes, so the script needs no
/// escaping — Windows accepts them as well.
fn literal(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "/"))
}

/// Writes `body` to `dir/<name>` and loads every script in `dir`.
fn load(dir: &Path, name: &str, body: &str) -> script_api::Scripts {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), body).unwrap();
    script_api::load_from(dir)
}

#[test]
fn a_declarative_file_rule_is_collected() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "demo.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Demo App")
                .category("Cache")
                .sub_category("Images")
                .class("Utility")
                .path("{username}/AppData/Local/Demo App/cache")
                .remove_all_in_dir()
                .register();
        }
        "#,
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);
    assert_eq!(scripts.files.len(), 1);

    let rule = &scripts.files[0];
    assert_eq!(&*rule.program, "Demo App");
    assert_eq!(&*rule.category, "Cache");
    assert_eq!(&*rule.sub_category, "Images");
    assert_eq!(&*rule.class, "Utility");
    assert_eq!(
        rule.path.as_string(),
        "{username}/AppData/Local/Demo App/cache"
    );
    assert!(
        rule.flags
            .contains(database::structures::CleanerFlags::REMOVE_ALL_IN_DIR)
    );
}

#[test]
fn a_rule_with_nothing_to_remove_is_an_error() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "empty.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Empty").path("/tmp/nothing").register();
        }
        "#,
    );

    // The rule is refused, and the script is named so the user can fix it.
    assert!(scripts.files.is_empty());
    assert_eq!(scripts.errors.len(), 1);
    assert!(scripts.errors[0].message.contains("nothing to remove"));
}

#[test]
fn a_custom_cleaner_reports_what_it_removed() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let nested = cache.join("nested");

    fs::create_dir_all(&nested).unwrap();
    fs::write(cache.join("keep.txt"), "keep").unwrap();
    fs::write(cache.join("drop.tmp"), "0123456789").unwrap();
    fs::write(nested.join("another.tmp"), "01234").unwrap();

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "demo.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Demo")
                    .category("Cache")
                    .path({})
                    .handler("clean")
                    .register();
            }}

            pub fn clean(path) {{
                // A template literal: backticks, with `${{name}}` interpolation.
                let pattern = `${{path}}/**/*.tmp`;

                for entry in cleaner::glob(pattern) {{
                    cleaner::remove_file(entry);
                }}
            }}
            "#,
            literal(&cache),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);
    assert_eq!(scripts.custom.len(), 1);

    let cleaner = scripts.custom[0].clone();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { (cleaner.function)(&cleaner, None).await });

    // Two files, 15 bytes; the directories stay because the script only
    // removed files.
    assert!(!cache.join("drop.tmp").exists(), "the handler did not run");
    assert!(!nested.join("another.tmp").exists());
    assert!(cache.join("keep.txt").exists());
    assert_eq!(result.files, 2);
    assert_eq!(result.folders, 0);
    assert_eq!(result.bytes, 15);
    assert_eq!(result.paths.len(), 1);
    assert!(result.working);
}

#[test]
fn a_handler_may_await_and_use_std() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("notes.txt"), "hello").unwrap();

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "async.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Async App")
                    .path({})
                    .register();
            }}

            // Default handler name is `clean`.
            pub async fn clean(path) {{
                let notes = fs::read_to_string(`${{path}}/notes.txt`).await?;
                cleaner::log(`read: ${{notes}}`);

                if notes.trim() == "hello" {{
                    cleaner::remove_file(`${{path}}/notes.txt`);
                }}
            }}
            "#,
            literal(&cache),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    let cleaner = scripts.custom[0].clone();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { (cleaner.function)(&cleaner, None).await });

    assert_eq!(result.files, 1);
    assert_eq!(result.bytes, 5);
    assert!(!cache.join("notes.txt").exists());
}

#[test]
fn a_broken_script_contributes_nothing_and_is_reported() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "broken.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Half Done").path("/tmp/x").remove_all_in_dir().register();
            this is not rune
        }
        "#,
    );

    // The rule registered before the syntax error is rolled back, so the user
    // does not end up with half a cleaner and no error next to it.
    assert!(scripts.files.is_empty());
    assert_eq!(scripts.errors.len(), 1);
    assert!(scripts.errors[0].path.ends_with("broken.rn"));
}

#[test]
fn a_panicking_main_is_rolled_back() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "panics.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Half Done").path("/tmp/x").remove_all_in_dir().register();
            panic!("boom");
        }
        "#,
    );

    assert!(scripts.files.is_empty());
    assert_eq!(scripts.errors.len(), 1);
    assert!(scripts.errors[0].message.contains("main()"));
}

#[test]
fn the_finished_hook_receives_the_per_program_breakdown() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("report.txt");

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "notify.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::on_finished("notify");
            }}

            pub fn notify(report) {{
                let text = format!(
                    "total={{}} files={{}} folders={{}} programs={{}}\n",
                    report.bytes, report.files, report.folders, report.programs,
                );

                for entry in report.entries() {{
                    let line = format!(
                        "program={{}} files={{}} folders={{}} bytes={{}} size={{}} categories={{}}\n",
                        entry.program,
                        entry.files,
                        entry.folders,
                        entry.bytes,
                        entry.size(),
                        entry.categories().len(),
                    );

                    text += line;
                }}

                cleaner::write({}, text);
            }}
            "#,
            literal(&out),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    let cleared = vec![
        Cleared {
            program: "Small".into(),
            removed_bytes: 10,
            removed_files: 1,
            removed_directories: 0,
            affected_categories: vec!["Cache".into()],
            paths: Vec::new(),
            paths_omitted: 0,
        },
        Cleared {
            program: "Big".into(),
            removed_bytes: 2_000,
            removed_files: 3,
            removed_directories: 2,
            affected_categories: vec!["Cache".into(), "Logs".into()],
            paths: Vec::new(),
            paths_omitted: 0,
        },
        Cleared {
            program: "Big".into(),
            removed_bytes: 5,
            removed_files: 0,
            removed_directories: 1,
            affected_categories: vec!["Logs".into()],
            paths: Vec::new(),
            paths_omitted: 0,
        },
    ];

    script_api::notify_finished(2_015, 4, 3, &cleared);

    // The hook runs on a background thread; wait for it to land.
    let mut text = String::new();
    for _ in 0..100 {
        if let Ok(contents) = fs::read_to_string(&out) {
            text = contents;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");

    // Totals first.
    assert!(lines[0].contains("total=2015"), "{text}");
    assert!(lines[0].contains("files=4"), "{text}");
    assert!(lines[0].contains("folders=3"), "{text}");
    assert!(lines[0].contains("programs=2"), "{text}");

    // Largest first, and one entry per program: the two `Big` rows are folded.
    assert!(lines[1].starts_with("program=Big"), "{text}");
    assert!(lines[1].contains("bytes=2005"), "{text}");
    assert!(lines[1].contains("files=3"), "{text}");
    assert!(lines[1].contains("folders=3"), "{text}");
    assert!(lines[1].contains("categories=2"), "{text}");

    assert!(lines[2].starts_with("program=Small"), "{text}");
    assert!(lines[2].contains("bytes=10"), "{text}");
    assert!(lines[2].contains("categories=1"), "{text}");
}

#[test]
fn rules_can_be_restricted_to_other_operating_systems() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "os.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Not Here")
                .path("/tmp/whatever")
                .os(["plan9"])
                .remove_all_in_dir()
                .register();

            cleaner::file_cleaner("Here")
                .path("/tmp/whatever")
                .os([cleaner::os()])
                .remove_all_in_dir()
                .register();
        }
        "#,
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);
    assert_eq!(scripts.files.len(), 1);
    assert_eq!(&*scripts.files[0].program, "Here");
}

#[test]
fn a_script_that_does_not_exist_yet_leaves_an_empty_set() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let scripts = script_api::load_from(dir.path().join("missing"));

    // Loading creates the directory, so the user has somewhere to put a script.
    assert!(dir.path().join("missing").is_dir());
    assert!(scripts.files.is_empty());
    assert!(scripts.custom.is_empty());
    assert!(scripts.errors.is_empty());
}

/// The imperative registry API, driven by a real script and checked against a
/// scratch key under `HKEY_CURRENT_USER`.
#[cfg(windows)]
#[test]
fn a_script_can_read_and_remove_registry_values() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("registry.txt");

    // A per-run key, so a leftover from an interrupted run cannot make this
    // test pass or fail by accident.
    let key = format!(
        "HKEY_CURRENT_USER\\Software\\Cross-Cleaner\\ScriptTests\\{}",
        std::process::id(),
    );
    // Rune has no raw strings: the script gets the key with doubled backslashes.
    let key = key.replace('\\', "\\\\");

    let scripts = load(
        dir.path(),
        "registry.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Registry App")
                    .category("Registry")
                    .path({})
                    .handler("walk")
                    .register();
            }}

            pub fn walk(path) {{
                let key = "{}";

                let before = match cleaner::registry::read(key, "Marker") {{
                    Some(value) => value,
                    None => "missing",
                }};

                let written = cleaner::registry::write(key, "Marker", "hello");

                let read_back = match cleaner::registry::read(key, "Marker") {{
                    Some(value) => value,
                    None => "missing",
                }};

                let listed = cleaner::registry::values(key).len();
                let removed = cleaner::registry::delete_value(key, "Marker");
                let key_still_there = cleaner::registry::exists(key);

                cleaner::write(
                    {},
                    format!(
                        "{{}}|{{}}|{{}}|{{}}|{{}}|{{}}",
                        before, written, read_back, listed, removed, key_still_there,
                    ),
                );

                cleaner::registry::delete_key(key);
            }}
            "#,
            literal(dir.path()),
            key,
            literal(&out),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    let cleaner = scripts.custom[0].clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { (cleaner.function)(&cleaner, None).await });

    let text = fs::read_to_string(&out).unwrap();
    let fields: Vec<&str> = text.split('|').collect();

    // Nothing was there yet, the write and the read agreed, at least the value
    // written is listed, and it is gone again while the key itself survives
    // until `delete_key`.
    assert_eq!(fields[0], "missing", "{text}");
    assert_eq!(fields[1], "true", "{text}");
    assert_eq!(fields[2], "hello", "{text}");
    assert!(fields[3].parse::<usize>().unwrap() >= 1, "{text}");
    assert_eq!(fields[4], "true", "{text}");
    assert_eq!(fields[5], "true", "{text}");
}
