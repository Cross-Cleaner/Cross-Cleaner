# 🧩 Scripting API

Cross Cleaner can be extended with scripts written in [Rune], a small Rust-like
language. A script adds cleanings to the same lists the built-in catalog lives
in: they show up under the usual categories, are counted by the usual scan, are
cleaned by the usual job and report their results the usual way.

Read this together with [`examples/scripts/`](examples/scripts) — every example
there is compiled by the test suite, so it is guaranteed to be valid API.

- [Where scripts live](#-where-scripts-live)
- [How a script is run](#-how-a-script-is-run)
- [Rune, not Rust](#-rune-not-rust)
- [The `cleaner` module](#-the-cleaner-module)
  - [Declarative file rules](#declarative-file-rules-file_cleaner)
  - [Declarative registry rules](#declarative-registry-rules-registry_cleaner)
  - [Custom cleaners](#custom-cleaners-custom_cleaner)
  - [Looking around](#looking-around)
  - [Removing](#removing)
  - [Counting](#counting)
  - [Diagnostics and webhooks](#diagnostics-and-webhooks)
  - [The finished hook](#the-finished-hook-on_finished)
- [`cleaner::registry`](#cleanerregistry-windows)
- [Types](#-types)
- [Standard library](#-standard-library)
- [Errors, rollback and limits](#-errors-rollback-and-limits)

## 📂 Where scripts live

| System | Directory |
| --- | --- |
| Windows | `%APPDATA%\Cross-Cleaner\Scripts` |
| Linux | `$XDG_CONFIG_HOME/Cross-Cleaner/Scripts` (usually `~/.config/...`) |
| macOS | `~/Library/Application Support/Cross-Cleaner/Scripts` |

Every `*.rn` file in that directory is loaded. Sub-directories are not scanned:
one directory, flat, is the whole surface. The directory is created on first
start, so the user has somewhere to put the first script.

Two environment variables are for development and packaging:

| Variable | Effect |
| --- | --- |
| `CROSS_CLEANER_SCRIPTS_DIR` | Read scripts from this directory instead |
| `CROSS_CLEANER_SCRIPTS_STDIO` | `1` turns on `println!`/`dbg!` output (off by default: the TUI and the window app own the screen) |

Scripts are enabled everywhere, including `cli`. `cli --no-scripts true` turns
them off, which is what a scheduled run that must not send notifications wants.

## 🚀 How a script is run

1. **Load** — at startup the file is compiled. A compile error is reported and
   that file is skipped; the rest still work.
2. **`main()`** — called once, right after compiling. This is where a script
   declares what it cleans. A script without `main` is an error: nothing it
   declares could ever be reached.
3. **Handlers** — a `custom_cleaner` names a function. The cleaning job calls it
   with the expanded path when the user's selection reaches that cleaner.
4. **`on_finished`** — a handler registered with `cleaner::on_finished(name)` is
   called once after every real cleaning run, with a [`Report`](#report). Never
   for a scan or a dry run.

Handlers may be `async` and `.await`. A `custom_cleaner` handler that fails or
panics counts as having removed nothing and is logged; it never aborts the run.

## 🦀 Rune, not Rust

Scripts read like Rust, but this is Rune. The differences that bite first:

| Rust | Rune |
| --- | --- |
| `let mut x = 1;` | `let x = 1;` — everything is mutable |
| `r"C:\path"` | no raw strings: `"C:\\path"`, or write paths with `/` |
| `format!("{}", x)` | works, and so does `` `${x}` `` in a template literal |
| `use std::fs;` | no `use` needed for the modules listed below — they are installed bare |
| structs with fields, traits | not available; use plain functions and objects `#{ key: value }` |
| `x?` in any function | `?` only inside `pub async fn` (or an `async` block) |

Template literals are backticks, interpolation is `${ ... }`:

```rune
let path = `${base}/cache`;
let text = `freed ${report.bytes} bytes in ${report.files} files`;
```

An object literal is `#{ key: value }` — that is what `json::to_string` wants.

## 🧰 The `cleaner` module

Everything below is a function on the `cleaner` module, so a script that does
not `use` anything still calls `cleaner::remove_file(path)`.

### Declarative file rules (`file_cleaner`)

A rule is a path pattern plus what to remove there. No handler runs: the
cleaning engine does the work, so the rule costs the same as a built-in entry and
is measurable by `--dry-run`.

```rune
cleaner::file_cleaner("My App")        // the program name shown in the lists
    .category("Cache")                // required in practice: the category it lives in
    .sub_category("Images")           // optional: the sub-category row
    .class("Utility")                 // optional: program class, defaults to "Other"
    .path("{username}/AppData/Local/My App/cache")   // required
    .os(["windows"])                  // optional: restrict to some systems
    .remove_all_in_dir()              // what to remove (at least one)
    .register();                      // commit it — call last
```

Path placeholders, expanded exactly as for the built-in catalog:

| Placeholder | Becomes |
| --- | --- |
| `{username}` | The current user's profile directory |
| `{steam}` | The Steam installation directory |
| `{drive}` | The system drive, e.g. `C:` |

What to remove — call any combination:

| Method | Effect |
| --- | --- |
| `.files_to_remove([".log", "*.tmp"])` | Remove files matching these name globs |
| `.directories_to_remove(["Cache", "GPUCache"])` | Remove directories matching these name globs |
| `.remove_files()` | Remove the files directly inside the path |
| `.remove_directories()` | Remove the sub-directories of the path, with their contents |
| `.remove_all_in_dir()` | Remove everything inside the path, keeping the path |
| `.remove_directory_after_clean()` | Remove the path itself once it is empty |

A rule with nothing to remove is reported as an error instead of being
registered: a cleaner that cannot delete anything is always a mistake.

### Declarative registry rules (`registry_cleaner`)

Windows-only in effect; on other systems the rule is collected and simply has no
registry database to join.

```rune
cleaner::registry_cleaner("My App")
    .category("Registry")
    .sub_category("Recent")
    .path("HKEY_CURRENT_USER\\Software\\My App\\Recent")   // required
    .os(["windows"])
    .values_to_remove(["LastFile", "LastFolder"])          // exact names
    .keys_to_remove(["Backup"])                            // exact sub-key names
    .remove_values("session-*")                            // glob over value names
    .remove_trees("cache-*")                               // glob over sub-key names
    .remove_all_in_registry()                              // every value directly under the key
    .remove_all_in_tree()                                  // every value anywhere under the key
    .register();
```

`"*"` is a valid glob for `.remove_values("*")` / `.remove_trees("*")` and means
"all of them". Hive names may be written `HKEY_CURRENT_USER` or the regedit
abbreviation `HKCU`.

### Custom cleaners (`custom_cleaner`)

For anything a path rule cannot express: only files older than a week, only
files whose content says so, a layout that changes with the version.

```rune
cleaner::custom_cleaner("My App")
    .category("Logs")
    .sub_category("Old")
    .path("{username}/AppData/Roaming/My App/logs")  // required
    .handler("clean_old")        // function name; defaults to "clean"
    .id("my-app-logs")           // optional: the id used to toggle it
    .glob()                      // optional: path is a glob, handler runs per match
    .sequential()                // optional: run it alone instead of in the pool
    .os(["windows", "linux"])
    .register();

pub fn clean_old(path) {
    // ...
}
```

The handler receives the expanded path (a `String`) and reports what it removed
through the counting described below — a return value is not used, so a script
cannot overstate its result.

`custom_cleaner` handlers cannot be measured by `--dry-run` without running
them; a scan names them as unmeasured instead of promising their bytes.

### Looking around

| Function | Returns | Notes |
| --- | --- | --- |
| `cleaner::exists(path)` | `bool` | This is the check to use before deleting |
| `cleaner::is_file(path)` | `bool` | False for directories and symlinks |
| `cleaner::is_dir(path)` | `bool` | |
| `cleaner::list(path)` | `[String]` | Direct entries as full paths, sorted; a missing directory is `[]` |
| `cleaner::glob(pattern)` | `[String]` | `**` crosses directories; sorted; `[]` when nothing matches |
| `cleaner::size(path)` | `u64` | Bytes under `path`, directories included |
| `cleaner::modified(path)` | `Option<u64>` | Seconds since the Unix epoch |
| `cleaner::now()` | `u64` | Seconds since the Unix epoch |
| `cleaner::os()` | `String` | `"windows"`, `"linux"`, `"macos"`, `"android"` |
| `cleaner::read(path)` | `Option<String>` | UTF-8 text; `None` when missing or not text |
| `cleaner::write(path, data)` | `bool` | Creates the file and its directory |

`cleaner::read` is the synchronous counterpart of `fs::read_to_string`, which is
`async` and therefore unusable from a non-`async` handler.

### Removing

Every removal call returns a [`Stats`](#stats) and also adds to the totals of the
handler that is running, so the results page shows what actually went.

| Function | Effect |
| --- | --- |
| `cleaner::remove_file(path)` | One file |
| `cleaner::remove_directory(path)` | One directory and everything inside it |
| `cleaner::remove_files(path, "*.log")` | Files under `path` whose name matches the glob |
| `cleaner::remove_directories(path, "cache-*")` | Matching sub-directories and their contents |
| `cleaner::remove_all(path)` | Everything inside `path`, keeping `path` |

A file that cannot be deleted (held open, permissions) is not an error: it is
skipped, not counted, and the run continues. That is deliberate — one locked log
must not stop the other 2000 from being cleaned.

### Counting

| Function | Effect |
| --- | --- |
| `cleaner::stats()` | What the current handler has removed so far |
| `cleaner::track(files, folders, bytes)` | Add work the handler did itself |

`track` exists for deletions made with std (`fs::remove_file`) rather than with
`cleaner::remove_*`. Do not use both for the same file, or it counts twice.

```rune
pub fn clean(path) {
    let before = cleaner::stats();

    for entry in cleaner::list(path) {
        if entry.ends_with(".tmp") {
            cleaner::remove_file(entry);   // counted by the call itself
        }
    }

    let done = cleaner::stats();
    cleaner::log(`removed ${done.files - before.files} temp files`);
}
```

The cost of getting this wrong is visible to the user: the numbers on the
results page are the numbers these calls recorded.

### Diagnostics and webhooks

| Function | Effect |
| --- | --- |
| `cleaner::log(message)` | A line in the application log |
| `cleaner::warn(message)` | A warning in the application log |
| `cleaner::http_get(url)` | `Option<String>` — the body, `None` on any failure |
| `cleaner::http_post(url, body)` | `bool` — `true` when the server accepted it |

Prefer these over `println!`: the TUI and the window app own the screen, and
terminal output from a worker thread tears the frame apart.

### The finished hook (`on_finished`)

```rune
pub fn main() {
    cleaner::on_finished("notify");
}

pub async fn notify(report) {
    let body = json::to_string(#{
        "text": report.details(),
        "files": report.files,
    })?;

    cleaner::http_post("https://api.telegram.org/bot<token>/sendMessage", body);
}
```

The hook runs on a background thread after a real run — never for a scan or a
dry run — so a slow webhook does not hold up the results page. `cli clean` waits
up to ten seconds for it before exiting; the window apps let it run as long as
they stay open. A script that declares a hook still needs a `main`, even an
empty one.

## `cleaner::registry` (Windows)

The declarative `registry_cleaner` covers the common case. These functions are
for the case where a script has to look before it deletes. On other platforms
they are no-ops (`false`, `[]`, `None`), so a shared script still loads.

| Function | Returns | Notes |
| --- | --- | --- |
| `cleaner::registry::exists(path)` | `bool` | |
| `cleaner::registry::read(path, name)` | `Option<String>` | Text form of the value |
| `cleaner::registry::write(path, name, data)` | `bool` | Creates the key if needed |
| `cleaner::registry::values(path)` | `[String]` | Names of the values directly under the key |
| `cleaner::registry::keys(path)` | `[String]` | Names of the sub-keys, relative to the key |
| `cleaner::registry::delete_value(path, name)` | `bool` | |
| `cleaner::registry::delete_key(path)` | `bool` | The key with everything under it |

```rune
pub fn clean(path) {
    let base = "HKEY_CURRENT_USER\\Software\\My App\\Recent";

    for sub in cleaner::registry::keys(base) {
        if sub.starts_with("session-") {
            cleaner::registry::delete_key(format!("{}\\{}", base, sub));
        }
    }
}
```

Successful removals are recorded against the running handler, exactly like a
file deletion, so they appear in the results even though a registry entry has no
size.

## 📦 Types

### `Stats`

Returned by every removal call and by `cleaner::stats()`.

| Field | Type | Meaning |
| --- | --- | --- |
| `files` | `u64` | Files removed |
| `folders` | `u64` | Directories removed, including those removed with their contents |
| `bytes` | `u64` | Bytes freed |

### `Report`

Handed to every `on_finished` handler.

| Member | Type | Meaning |
| --- | --- | --- |
| `files` | `u64` | Files removed in the run |
| `folders` | `u64` | Directories removed in the run |
| `bytes` | `u64` | Bytes freed in the run |
| `programs` | `u64` | How many programs contributed |
| `.entries()` | `[ProgramReport]` | One row per program, largest first |
| `.summary()` | `String` | One sentence, e.g. `Free 1.2 GB in 3214 files and 87 folders across 12 programs` |
| `.details()` | `String` | One line per program — ready to drop into a message |
| `.size()` | `String` | The total as `"1.2 GB"` |
| `.program(name)` | `Option<ProgramReport>` | The row for one program, if it was cleaned |
| `.category(name)` | `[ProgramReport]` | The rows cleaned under one category |

### `ProgramReport`

| Member | Type | Meaning |
| --- | --- | --- |
| `program` | `String` | The program name as shown in the results page |
| `files` | `u64` | Files removed for it |
| `folders` | `u64` | Directories removed for it |
| `bytes` | `u64` | Bytes freed for it |
| `.categories()` | `[String]` | The categories it was cleaned under |
| `.in_category(name)` | `bool` | Whether one of them is `name` |
| `.size()` | `String` | Its total as `"1.2 GB"` |
| `.line()` | `String` | `Name: 12 files, 3 folders, 1.2 MB` |

A program cleaned under two categories is one row listing both, not two rows.

## 📚 Standard library

Installed for every script:

| Module | Comes from | What it gives |
| --- | --- | --- |
| `std` | Rune | Strings, vectors, iterators, `Option`, `Result`, `format!`, `std::io`, `std::fmt` |
| `fs` | rune-modules | `fs::read_to_string(path).await` |
| `json` | rune-modules | `json::to_string(value)`, `json::from_string(text)` |
| `rand` | rune-modules | Random numbers and choices |
| `time` | rune-modules | `time::sleep(duration).await` |
| `process` | rune-modules | `process::Command` — run an external program |

```rune
pub async fn notify(report) {
    // Give the desktop a notification too, next to the webhook.
    let mut command = process::Command::new("notify-send");
    command.arg("Cross Cleaner");
    command.arg(report.summary());
    command.status().await?;

    time::sleep(time::Duration::from_secs(1)).await?;
}
```

## 🛑 Errors, rollback and limits

- **Compile error** — the file is skipped and reported in the application log
  (`[warn] .../my.rn: error: ...` with the line and column). Nothing from it is
  registered.
- **`main` fails or panics** — everything that file registered is rolled back,
  so the user never gets half a cleaner and no explanation.
- **A handler fails at cleaning time** — logged, and counts as zero removed. The
  rest of the run continues.
- **The same program twice** — a script may register several entries for one
  program; they are grouped into one row in the results page, and `.id(...)`
  is only needed when both should be toggleable separately.
- **Nothing to clean** — a rule or handler that finds nothing simply contributes
  nothing; it is not an error.

Scripts run inside the application process, on the same thread pool as the
built-in cleaners. They are not sandboxed: a script can delete what the user can
delete. Keep globs precise and never widen a pattern "just in case".

[Rune]: https://rune-rs.github.io/