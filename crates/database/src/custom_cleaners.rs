use crate::structures::{CleanerResult, CustomCleaner};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use tokio::sync::mpsc;

type Registry = RwLock<HashMap<String, CustomCleaner>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a built-in custom cleaner. Call once per cleaner at startup
/// (see cleaner::custom_cleaners::register_all).
/// Returns false if a cleaner with the same id is already registered.
pub fn register_custom_cleaner(cleaner: CustomCleaner) -> bool {
    registry()
        .write()
        .expect("custom cleaner registry poisoned")
        .insert(cleaner.id.clone(), cleaner)
        .is_none()
}

/// All registered custom cleaners for the current OS, with placeholders expanded.
/// Cleaners whose path contains {drive} are expanded into one entry per drive.
pub fn get_custom_cleaners() -> Vec<CustomCleaner> {
    registry()
        .read()
        .expect("custom cleaner registry poisoned")
        .values()
        .filter(|cleaner| cleaner.matches_current_os())
        .cloned()
        .flat_map(expand_placeholders)
        .collect()
}

/// Get a single registered cleaner by id, with {username} expanded.
/// NOTE: {drive} stays as-is here (per-drive expansion happens in get_custom_cleaners).
pub fn get_custom_cleaner(id: &str) -> Option<CustomCleaner> {
    registry()
        .read()
        .expect("custom cleaner registry poisoned")
        .get(id)
        .cloned()
        .map(|mut cleaner| {
            cleaner.path = cleaner.path.replace(
                "{username}",
                &whoami::username().expect("Failed to get username"),
            );
            cleaner
        })
}

/// Ids of all registered custom cleaners (all OS, without filtering).
pub fn custom_cleaner_ids() -> Vec<String> {
    registry()
        .read()
        .expect("custom cleaner registry poisoned")
        .keys()
        .cloned()
        .collect()
}

/// Execute the cleaning function of a custom cleaner.
pub async fn run_custom_cleaner(
    cleaner: &CustomCleaner,
    sender: Option<mpsc::Sender<String>>,
) -> CleanerResult {
    (cleaner.function)(cleaner, sender).await
}

/// Expand placeholders: {home}, {username}, {steam}, {drive} (one entry per drive
/// letter).
fn expand_placeholders(cleaner: CustomCleaner) -> Vec<CustomCleaner> {
    let ctx = crate::cleaner_database::expansion();
    let path = cleaner.path.replace("{home}", &ctx.home);
    let path = path.replace("{username}", &ctx.username);
    let path = path.replace("{steam}", &ctx.steam);

    if !path.contains("{drive}") {
        return vec![CustomCleaner { path, ..cleaner }];
    }

    // WARN: Windows only ({drive} placeholder makes sense on Windows)
    let drives = if cfg!(windows) {
        disk_name::get_letters()
    } else {
        Vec::new()
    };

    drives
        .into_iter()
        .map(|drive| CustomCleaner {
            path: path.replace("{drive}", &drive),
            ..cleaner.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structures::SharedPath;

    fn test_cleaner(id: &str) -> CustomCleaner {
        CustomCleaner {
            id: String::from(id),
            program: std::sync::Arc::from("TestProgram"),
            category: std::sync::Arc::from("Logs"),
            sub_category: std::sync::Arc::from("Logs"),
            path: String::from("{username}/test.log").into(),
            args: vec![],
            os: vec![],
            sequential: false,
            function: |_, _| {
                Box::pin(async move {
                    CleanerResult {
                        files: 0,
                        folders: 0,
                        bytes: 0,
                        working: false,
                        path: SharedPath::default(),
                        paths: Vec::new(),
                        paths_omitted: 0,
                        locked_files: 0,
                        locked_bytes: 0,
                        locked: Vec::new(),
                        denied_files: 0,
                        denied_bytes: 0,
                        denied: Vec::new(),
                        program: std::sync::Arc::from(""),
                        category: std::sync::Arc::from(""),
                        sub_category: std::sync::Arc::from(""),
                    }
                })
            },
        }
    }

    #[test]
    fn test_register_and_get() {
        assert!(register_custom_cleaner(test_cleaner("reg_test_1")));
        assert!(!register_custom_cleaner(test_cleaner("reg_test_1")));

        let cleaner = get_custom_cleaner("reg_test_1").expect("should be registered");
        assert_eq!(cleaner.id, "reg_test_1");
        assert!(cleaner.matches_current_os());

        let expanded = get_custom_cleaner("reg_test_1").unwrap();
        assert!(!expanded.path.contains("{username}"));

        assert!(get_custom_cleaner("nonexistent_id").is_none());
    }

    #[test]
    fn test_ids_and_os_filter() {
        register_custom_cleaner(test_cleaner("ids_test_1"));

        let ids = custom_cleaner_ids();
        assert!(ids.iter().any(|id| id == "ids_test_1"));

        let all = get_custom_cleaners();
        assert!(all.iter().any(|c| c.id == "ids_test_1"));
    }

    #[tokio::test]
    async fn test_run_custom_cleaner() {
        let mut cleaner = test_cleaner("run_test_1");
        cleaner.function = |data, _sender| {
            let data = data.clone();
            Box::pin(async move {
                let mut r = CleanerResult {
                    files: 0,
                    folders: 0,
                    bytes: 0,
                    working: false,
                    path: data.path.clone(),
                    paths: Vec::new(),
                    paths_omitted: 0,
                    locked_files: 0,
                    locked_bytes: 0,
                    locked: Vec::new(),
                    denied_files: 0,
                    denied_bytes: 0,
                    denied: Vec::new(),
                    program: data.program.clone(),
                    category: data.category.clone(),
                    sub_category: data.sub_category.clone(),
                };
                r.working = true;
                r.files = 1;
                r
            })
        };

        let result = run_custom_cleaner(&cleaner, None).await;
        assert!(result.working);
        assert_eq!(result.files, 1);
        assert_eq!(result.program.as_ref(), "TestProgram");
    }

    #[test]
    fn test_matches_current_os() {
        let mut cleaner = test_cleaner("os_test_1");

        cleaner.os = vec![];
        assert!(cleaner.matches_current_os());

        let current = if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "macos"
        };
        cleaner.os = vec![String::from(current)];
        assert!(cleaner.matches_current_os());

        cleaner.os = vec![String::from("someotheros")];
        assert!(!cleaner.matches_current_os());
    }

    #[test]
    fn test_expand_placeholders_drive() {
        let mut cleaner = test_cleaner("drive_test_1");
        cleaner.path = String::from("{drive}/Users/{username}/data").into();

        let expanded = expand_placeholders(cleaner);

        if cfg!(windows) {
            assert!(!expanded.is_empty(), "must expand to at least one drive");
            for e in &expanded {
                assert!(e.path.contains("/Users/"), "drive replaced: {}", e.path);
                assert!(
                    !e.path.contains("{drive}"),
                    "no placeholder left: {}",
                    e.path
                );
                assert!(
                    !e.path.contains("{username}"),
                    "no placeholder left: {}",
                    e.path
                );
                assert!(
                    e.path.to_string().starts_with('\\')
                        || e.path.as_string().as_bytes()[1] == b':'
                );
            }
            let paths: std::collections::HashSet<crate::structures::SharedPath> =
                expanded.iter().map(|e| e.path.clone()).collect();
            assert_eq!(paths.len(), expanded.len(), "each drive = unique path");
            assert!(expanded.iter().all(|e| e.id == "drive_test_1"));
        } else {
            assert!(expanded.is_empty(), "no drives on non-windows");
        }
    }

    #[test]
    fn test_expand_placeholders_no_drive() {
        let cleaner = test_cleaner("no_drive_test_1");
        let expanded = expand_placeholders(cleaner);
        assert_eq!(expanded.len(), 1);
        assert_eq!(
            expanded[0].path,
            "WindowsUser/test.log".replace(
                "WindowsUser",
                &whoami::username().expect("Failed to get username")
            ),
        );
    }

    #[test]
    fn test_get_custom_cleaners_expands_drive() {
        let mut cleaner = test_cleaner("drive_multi_test_1");
        cleaner.path = String::from("{drive}/some/path.log").into();
        register_custom_cleaner(cleaner);

        let all = get_custom_cleaners();
        let matched: Vec<&CustomCleaner> = all
            .iter()
            .filter(|c| c.id == "drive_multi_test_1")
            .collect();

        if cfg!(windows) {
            assert!(!matched.is_empty());
            assert!(matched.iter().all(|c| !c.path.contains("{drive}")));
        } else {
            assert!(
                matched.is_empty(),
                "{{drive}} entry without drives is dropped"
            );
        }
    }
}
