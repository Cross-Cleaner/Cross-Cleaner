//! User scripts for Cross Cleaner.
//!
//! A script is a [Rune] file in the `Scripts` directory inside Cross Cleaner's
//! per-user data directory:
//!
//! * Windows: `%APPDATA%\Cross-Cleaner\Scripts`
//! * Linux: `$XDG_CONFIG_HOME/Cross-Cleaner/Scripts`
//! * macOS: `~/Library/Application Support/Cross-Cleaner/Scripts`
//!
//! Every `*.rn` file there is compiled at startup and its `main()` is called
//! once. From `main()` a script declares what should be cleaned; the frontends
//! then treat those declarations like any other database entry — same
//! categories, same program list, same progress bar.
//!
//! Three kinds of declaration:
//!
//! * `file_cleaner` — declarative paths and globs, no code of its own;
//! * `registry_cleaner` — declarative registry values and keys (Windows);
//! * `custom_cleaner` — a path plus a handler that decides what to delete and
//!   reports how much it deleted.
//!
//! Plus `on_finished`, which runs a handler after a real cleaning run with a
//! per-program report of what was freed.
//!
//! # A complete script
//!
//! ```rune
//! pub fn main() {
//!     // 1. A static rule: everything inside the cache folder goes.
//!     cleaner::file_cleaner("My App")
//!         .category("Cache")
//!         .path("{username}/AppData/Local/My App/Cache")
//!         .remove_all_in_dir()
//!         // The folder itself goes too, once emptied.
//!         .remove_directory_after_clean()
//!         .register();
//!
//!     // 2. A rule that decides at cleaning time.
//!     cleaner::custom_cleaner("My App")
//!         .category("Logs")
//!         .path("{username}/AppData/Roaming/My App/logs")
//!         .handler("clean_logs")
//!         .register();
//!
//!     // 3. Something to run when the whole run is over.
//!     cleaner::on_finished("notify");
//! }
//!
//! // Only files older than a week. The removal calls return and accumulate the
//! // counts the results page shows.
//! pub fn clean_logs(path) {
//!     let cutoff = cleaner::now() - 7 * 24 * 60 * 60;
//!
//!     for entry in cleaner::list(path) {
//!         if cleaner::is_file(entry) && cleaner::modified(entry) < cutoff {
//!             cleaner::remove_file(entry);
//!         }
//!     }
//! }
//!
//! pub async fn notify(report) {
//!     let body = json::to_string(#{
//!         "text": report.summary(),
//!         "entries": report.entries().len(),
//!     })?;
//!
//!     cleaner::http_post("https://example.com/hook", body);
//! }
//! ```
//!
//! # Rune, not Rust
//!
//! The API is shaped like Rust and the scripts read like it, but it is Rune and
//! a few things differ — expected, and worth knowing before the first typo:
//!
//! | Rust | Rune |
//! | --- | --- |
//! | `let mut x = 1;` | `let x = 1;` — everything is mutable |
//! | `r"C:\path"` | no raw strings: `"C:\\path"`, or write paths with `/` |
//! | `format!("{}", x)` | `format!("{}", x)` works, and so does `` `${x}` `` |
//! | `use std::fs;` | `fs::read_to_string(path).await?` — the std modules are installed bare |
//! | `#[derive(...)]`, traits | not available; use plain functions and objects |
//!
//! Installed alongside the `cleaner` module, from [rune-modules]: `fs` (`read_to_string`), `json`
//! (`to_string`, `from_string`), `rand`, `time` (`sleep`) and `process`
//! (`Command`). Rune's own `std` brings strings, vectors, iterators, `Option`,
//! `Result` and `format!`. Terminal output (`println!`, `dbg!`) is off unless
//! `CROSS_CLEANER_SCRIPTS_STDIO` is set — use `log` and `warn` instead, which
//! go to the application log.
//!
//! Handlers may be `async` and `.await`; they are driven by a real runtime. The
//! counts of what a handler removed are collected from the removal calls
//! themselves, so a script cannot overstate them by returning a number.
//!
//! # Failure
//!
//! A script that fails to compile, or whose `main` fails, is rolled back
//! entirely — no half-registered cleaner — reported through `database::diag`
//! and skipped. The rest keep working, and a broken script never aborts the
//! application. A handler that fails at cleaning time is logged and counts as
//! zero.
//!
//! [Rune]: https://rune-rs.github.io/
//! [rune-modules]: https://docs.rs/rune-modules

mod api;
mod collect;
mod files;
mod loader;
mod registry;
mod registry_api;
mod remove;

pub use api::RegistryRule;
#[cfg(windows)]
pub use loader::with_registry_scripts;
pub use loader::{
    ScriptError, Scripts, finished_hooks, load, load_from, notify_finished,
    register_custom_cleaners, scripts_dir, wait_for_hooks, with_scripts,
};
