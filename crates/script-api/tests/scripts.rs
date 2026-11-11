//! End-to-end tests: scripts are written to a temporary directory, loaded, and
//! their effects are observed on the filesystem and in the collected registry.
//!
//! Loading mutates process-wide state (the collector), so every test holds the
//! same lock: the point here is what a real script does, not parallelism.

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

use database::structures::Cleared;
use script_api::RunTotals;

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
            cleaner::file_cleaner("Empty").category("Cache").path("/tmp/nothing").register();
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
fn a_glob_cleaner_counts_every_path_it_was_handed() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    fs::create_dir_all(cache.join("a")).unwrap();
    fs::create_dir_all(cache.join("b")).unwrap();
    fs::write(cache.join("a").join("one.tmp"), "012").unwrap();
    fs::write(cache.join("a").join("two.tmp"), "0123").unwrap();
    fs::write(cache.join("b").join("three.tmp"), "01234").unwrap();
    fs::write(cache.join("keep.txt"), "keep").unwrap();

    // The host, not the script, expands the pattern: the handler sees one path
    // per match and every removal it makes belongs to that path's counters.
    let pattern = format!(
        "{}/**/*.tmp",
        cache.display().to_string().replace('\\', "/")
    );

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "glob.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Glob Demo")
                    .category("Cache")
                    .path("{pattern}")
                    .glob()
                    .handler("clean")
                    .register();
            }}

            pub fn clean(path) {{
                cleaner::remove_file(path);
            }}
            "#,
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

    assert!(!cache.join("a").join("one.tmp").exists());
    assert!(!cache.join("a").join("two.tmp").exists());
    assert!(!cache.join("b").join("three.tmp").exists());
    assert!(cache.join("keep.txt").exists());

    // Three matches, 12 bytes, and one listed path per match — not one batched
    // entry, and not a triple-counted one.
    assert_eq!(result.files, 3);
    assert_eq!(result.bytes, 12);
    assert_eq!(result.paths.len(), 3);
    assert!(result.working);
}

#[test]
fn one_failing_path_does_not_stop_the_others() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("one.tmp"), "1").unwrap();
    fs::write(cache.join("boom.tmp"), "2").unwrap();
    fs::write(cache.join("two.tmp"), "3").unwrap();

    let pattern = format!("{}/*.tmp", cache.display().to_string().replace('\\', "/"));

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "boom.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Boom Demo")
                    .category("Cache")
                    .path("{pattern}")
                    .glob()
                    .handler("clean")
                    .register();
            }}

            pub fn clean(path) {{
                if cleaner::filename(path) == Some("boom.tmp") {{
                    panic!("boom");
                }}

                cleaner::remove_file(path);
            }}
            "#,
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    let cleaner = scripts.custom[0].clone();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { (cleaner.function)(&cleaner, None).await });

    // The panic belongs to `boom.tmp`: the other two are still cleaned and
    // counted, and the file that panicked is left where it was.
    assert!(!cache.join("one.tmp").exists());
    assert!(!cache.join("two.tmp").exists());
    assert!(cache.join("boom.tmp").exists());
    assert_eq!(result.files, 2);
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
                cleaner::custom_cleaner("Async App").category("Cache")
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
            cleaner::file_cleaner("Half Done").category("Cache").path("/tmp/x").remove_all_in_dir().register();
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
            cleaner::file_cleaner("Half Done").category("Cache").path("/tmp/x").remove_all_in_dir().register();
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
            category_totals: Vec::new(),
            paths: Vec::new(),
            paths_omitted: 0,
        },
        Cleared {
            program: "Big".into(),
            removed_bytes: 2_000,
            removed_files: 3,
            removed_directories: 2,
            affected_categories: vec!["Cache".into(), "Logs".into()],
            category_totals: Vec::new(),
            paths: Vec::new(),
            paths_omitted: 0,
        },
        Cleared {
            program: "Big".into(),
            removed_bytes: 5,
            removed_files: 0,
            removed_directories: 1,
            affected_categories: vec!["Logs".into()],
            category_totals: Vec::new(),
            paths: Vec::new(),
            paths_omitted: 0,
        },
    ];

    script_api::notify_finished(
        RunTotals {
            bytes: 2_015,
            files: 4,
            folders: 3,
            ..RunTotals::default()
        },
        &cleared,
    );

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
fn the_decisions_and_path_helpers_answer_about_real_files() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let out = dir.path().join("answers.txt");
    let missing = dir.path().join("not-here");

    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("drop.tmp"), "0123456789").unwrap();
    fs::write(cache.join("keep.txt"), "keep").unwrap();

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "decisions.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Decisions")
                    .category("Cache")
                    .path({})
                    .handler("clean")
                    .sequential()
                    .register();
            }}

            pub fn clean(path) {{
                for entry in cleaner::list(path) {{
                    // A fresh file is older than zero days, so this deletes it;
                    // `matches` decides on the name alone.
                    if cleaner::matches(entry, "*.tmp") && cleaner::older_than(entry, 0) {{
                        cleaner::remove_file(entry);
                    }}
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

    assert!(!cache.join("drop.tmp").exists(), "older_than + matches");
    assert!(cache.join("keep.txt").exists());
    assert_eq!(result.files, 1);
    assert_eq!(result.bytes, 10);

    // The helpers on their own, so a broken one fails on its own name rather
    // than as a missing file somewhere above.
    let scripts = load(
        dir.path().join("scripts-helpers").as_path(),
        "helpers.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Helpers").category("Cache").path({}).handler("check").register();
            }}

            pub fn check(path) {{
                let out = {};
                // `join` takes its pieces, so the handler clones rather than
                // handing its own argument away.
                let keep = cleaner::join([path.clone(), "keep.txt"]);

                let parent = match cleaner::parent(keep.clone()) {{
                    Some(value) => value,
                    None => format!("{{}}", ""),
                }};

                let parent_name = match cleaner::filename(parent) {{
                    Some(value) => value,
                    None => format!("{{}}", ""),
                }};

                let name = match cleaner::filename(keep.clone()) {{
                    Some(value) => value,
                    None => format!("{{}}", ""),
                }};

                cleaner::write(
                    out,
                    format!(
                        "{{}}|{{}}|{{}}|{{}}|{{}}|{{}}|{{}}",
                        cleaner::exists(keep.clone()),
                        cleaner::older_than({}, 0),
                        cleaner::unlocked(keep.clone()),
                        cleaner::locked(keep),
                        cleaner::lock_files(path).len(),
                        name,
                        parent_name,
                    ),
                );
            }}
            "#,
            literal(&cache),
            literal(&out),
            literal(&missing),
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

    assert_eq!(fields[0], "true", "exists: {text}");
    assert_eq!(fields[1], "false", "a missing path is not old: {text}");
    assert_eq!(fields[2], "true", "nothing holds keep.txt: {text}");
    assert_eq!(fields[3], "false", "locked is the inverse: {text}");
    assert_eq!(fields[4], "0", "nothing is locked: {text}");
    assert_eq!(fields[5], "keep.txt", "filename: {text}");
    assert_eq!(fields[6], "cache", "parent: {text}");
}

#[test]
fn the_started_and_error_hooks_run_in_their_own_phase() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started.txt");
    let failed = dir.path().join("failed.txt");
    let finished = dir.path().join("finished.txt");

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "hooks.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::on_started("before");
                cleaner::on_finished("after");
                cleaner::on_error("problem");
            }}

            pub fn before() {{
                cleaner::write({}, "started");
            }}

            pub fn after(report) {{
                cleaner::write({}, "finished");
            }}

            pub fn problem(report, reason) {{
                cleaner::write({}, reason);
            }}
            "#,
            literal(&started),
            literal(&finished),
            literal(&failed),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    script_api::notify_started();

    // The started hook is synchronous: by the time this returns, it has run.
    assert_eq!(fs::read_to_string(&started).unwrap(), "started");

    // A run that went clean calls `is_finished`, not `on_error`.
    script_api::notify_finished(RunTotals::default(), &[]);
    wait_for_file(&finished);
    assert!(!failed.exists(), "a clean run must not call on_error");

    // A run that left locked files calls `on_error`, with the reason.
    let locked = vec![Cleared {
        program: "Demo".into(),
        removed_bytes: 12,
        removed_files: 2,
        removed_directories: 0,
        affected_categories: vec!["Cache".into()],
        category_totals: Vec::new(),
        paths: Vec::new(),
        paths_omitted: 0,
    }];

    script_api::notify_failed(
        RunTotals {
            bytes: 12,
            files: 2,
            ..RunTotals::default()
        },
        &locked,
        "3 files were still in use",
    );
    assert_eq!(
        wait_for_file(&failed),
        "3 files were still in use",
        "the reason reaches the handler"
    );
}

/// Waits for a hook to write its file, and returns its contents.
fn wait_for_file(path: &Path) -> String {
    for _ in 0..100 {
        if let Ok(contents) = fs::read_to_string(path) {
            return contents;
        }

        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    panic!("the hook never wrote {}", path.display());
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

            cleaner::file_cleaner("Here").category("Cache")
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

#[test]
fn selection_filters_and_extra_paths_reach_the_database() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "filters.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("Filtered")
                .category("Cache")
                .path("/tmp/one")
                .paths(["/tmp/two", "/tmp/three"])
                .exclude(["*.lock", "user-data"])
                .older_than(7)
                .min_size("1 MB")
                .max_size("2 GB")
                .max_depth(3)
                .skip_if_running(["chrome.exe"])
                .to_trash()
                .remove_all_in_dir()
                .register();
        }
        "#,
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    // One entry per path, same program, so the results fold into one row.
    assert_eq!(scripts.files.len(), 3);
    let paths: Vec<String> = scripts
        .files
        .iter()
        .map(|entry| entry.path.as_string())
        .collect();
    assert_eq!(paths, vec!["/tmp/one", "/tmp/two", "/tmp/three"]);

    let filter = &scripts.files[0].filter;
    assert_eq!(
        filter.exclude.iter().map(|p| &**p).collect::<Vec<_>>(),
        vec!["*.lock", "user-data"]
    );
    assert_eq!(filter.older_than_seconds, Some(7 * 86_400));
    assert_eq!(filter.min_size, Some(1_048_576));
    assert_eq!(filter.max_size, Some(2_147_483_648));
    assert_eq!(filter.max_depth, Some(3));
    assert_eq!(
        filter
            .skip_if_running
            .iter()
            .map(|p| &**p)
            .collect::<Vec<_>>(),
        vec!["chrome.exe"]
    );
    assert!(filter.to_trash);
}

#[test]
fn within_keeps_a_handler_inside_its_own_directory() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");

    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("a.tmp"), "aaaa").unwrap();
    fs::write(outside.join("b.tmp"), "bbbb").unwrap();

    let scripts = load(
        dir.path().join("scripts").as_path(),
        "fenced.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Fenced")
                    .category("Cache")
                    .path({root})
                    .within({root})
                    .handler("clean")
                    .register();
            }}

            // A bug: the pattern points at the parent, not at the root it was
            // given. `within` is what turns that into "nothing happened".
            pub fn clean(path) {{
                for entry in cleaner::glob({outside_pattern}) {{
                    cleaner::remove_file(entry);
                }}

                for entry in cleaner::glob(`${{path}}/*.tmp`) {{
                    cleaner::remove_file(entry);
                }}
            }}
            "#,
            root = literal(&root),
            outside_pattern = literal(&outside.join("**").join("*.tmp")),
        ),
    );

    assert!(scripts.errors.is_empty(), "{:?}", scripts.errors);

    let cleaner = scripts.custom[0].clone();
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { (cleaner.function)(&cleaner, None).await });

    assert!(
        outside.join("b.tmp").exists(),
        "a removal outside `within` must be refused"
    );
    assert!(!root.join("a.tmp").exists(), "inside the fence still works");
    assert_eq!(result.files, 1, "the refused file is not counted");
    assert_eq!(result.bytes, 4);
}

#[test]
fn every_script_is_reported_even_when_it_fails() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path()).unwrap();

    fs::write(
        dir.path().join("good.rn"),
        r#"
        pub fn main() {
            cleaner::file_cleaner("Good")
                .category("Cache")
                .path("/tmp/good")
                .remove_all_in_dir()
                .register();
        }
        "#,
    )
    .unwrap();

    fs::write(
        dir.path().join("bad.rn"),
        "pub fn main() { this is not rune }",
    )
    .unwrap();

    let scripts = script_api::load_from(dir.path());

    // Two files, two rows: the one that failed has to be visible, or a user
    // whose script does nothing has no way to find out why.
    assert_eq!(scripts.reports.len(), 2);
    let good = scripts
        .reports
        .iter()
        .find(|report| report.path.ends_with("good.rn"))
        .unwrap();
    let bad = scripts
        .reports
        .iter()
        .find(|report| report.path.ends_with("bad.rn"))
        .unwrap();

    assert!(good.error.is_none());
    assert_eq!(good.files, 1);
    assert!(bad.error.is_some());
    assert_eq!(bad.files, 0);
}

#[test]
fn a_rule_without_a_category_is_refused_rather_than_guessed() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();

    let scripts = load(
        dir.path(),
        "uncategorised.rn",
        r#"
        pub fn main() {
            cleaner::file_cleaner("No Category")
                .path("/tmp/whatever")
                .remove_all_in_dir()
                .register();
        }
        "#,
    );

    // A cleaner lands in a category the user selects; guessing "Other" would
    // hide it from every category and no one would know why.
    assert!(scripts.files.is_empty());
    assert_eq!(scripts.errors.len(), 1);
    assert!(
        scripts.errors[0].message.contains("category"),
        "{:?}",
        scripts.errors
    );
}

#[cfg(windows)]
#[test]
fn a_script_can_read_types_export_and_back_up_a_key() {
    let _guard = lock();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("typed.txt");
    let dump = dir.path().join("dump.txt");

    let key = format!(
        "HKEY_CURRENT_USER\\Software\\Cross-Cleaner\\ScriptTests\\typed{}",
        std::process::id(),
    );
    let key = key.replace('\\', "\\\\");

    let scripts = load(
        dir.path(),
        "typed.rn",
        &format!(
            r#"
            pub fn main() {{
                cleaner::custom_cleaner("Typed App")
                    .category("Registry")
                    .path({work})
                    .handler("walk")
                    .register();
            }}

            pub fn walk(path) {{
                let key = "{key}";

                // A DWORD written by the script, read back as a number.
                let wrote = cleaner::registry::write_dword(key, "Count", 41);
                let count = match cleaner::registry::read_dword(key, "Count") {{
                    Some(value) => value,
                    None => 0,
                }};
                let no_qword = match cleaner::registry::read_qword(key, "Count") {{
                    Some(value) => value,
                    None => 0,
                }};

                // The type has to come back with the value: "is this safe to
                // delete" is answered by the kind, not by the text.
                let typed = cleaner::registry::values_full(key);
                let kind = if typed.len() > 0 {{
                    typed[0].kind
                }} else {{
                    "none"
                }};

                // A backup is the whole subtree as a file, so a script can undo
                // what it is about to delete.
                let text = cleaner::registry::export(key);
                let backed_up = cleaner::registry::backup(key, {dump});

                cleaner::write(
                    {out},
                    format!("{{}}|{{}}|{{}}|{{}}|{{}}|{{}}", wrote, count, no_qword, kind, text.len(), backed_up),
                );

                cleaner::registry::delete_key(key);
            }}
            "#,
            work = literal(dir.path()),
            key = key,
            dump = literal(&dump),
            out = literal(&out),
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

    assert_eq!(fields[0], "true", "{text}");
    assert_eq!(fields[1], "41", "{text}");
    assert_eq!(
        fields[2], "0",
        "a DWORD is not a QWORD, and the answer says so: {text}"
    );
    assert!(
        fields[3].contains("DWORD"),
        "the value type comes back with the value: {text}"
    );
    assert!(
        fields[4].parse::<usize>().unwrap() > 0,
        "the export has content: {text}"
    );
    assert_eq!(fields[5], "true", "{text}");
    assert!(dump.exists(), "the backup file was written");
    assert!(fs::read_to_string(&dump).unwrap().contains("Count"));
}
