//! Every script under `examples/scripts` is compiled here.
//!
//! The examples are the documentation the user actually copies, so they must
//! stay valid against the API: a change that breaks one breaks this test as
//! well, instead of breaking the user's script three releases later.
//!
//! Loading is all that happens — `main` registers rules, no handler is called —
//! so this touches no files and no registry.

use std::path::PathBuf;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("scripts")
}

#[test]
fn every_example_compiles() {
    let directory = examples();

    assert!(
        directory.is_dir(),
        "example scripts are missing: {}",
        directory.display()
    );

    let hooks = script_api::finished_hooks();
    let scripts = script_api::load_from(&directory);

    assert!(
        scripts.errors.is_empty(),
        "an example script does not compile:\n{:#?}",
        scripts.errors
    );

    assert!(
        !scripts.files.is_empty(),
        "the examples should register at least one file rule"
    );
    assert!(
        !scripts.registry.is_empty(),
        "the examples should register at least one registry rule"
    );
    assert!(
        !scripts.custom.is_empty(),
        "the examples should register at least one custom cleaner"
    );
    assert!(
        script_api::finished_hooks() > hooks,
        "the examples should register at least one finished hook"
    );
}
