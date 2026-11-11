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
  - [Deciding what to delete](#deciding-what-to-delete)
  - [Paths](#paths)
  - [Removing](#removing)
  - [Counting](#counting)
  - [Diagnostics and webhooks](#diagnostics-and-webhooks)
  - [Hook: before the run](#hook-before-the-run-on_started)
  - [Hook: after a run that went wrong](#hook-after-a-run-that-went-wrong-on_error)
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

## 🔎 Checking your scripts

A script that does nothing is the hardest kind of bug to find: the cleaner simply
is not in the list. `cli scripts` answers what loaded and what did not:

```text
$ cross-cleaner scripts
2 scripts in C:\Users\me\AppData\Roaming\Cross-Cleaner\Scripts
  broken.rn 0 file, 0 registry, 0 custom, 0 hooks
    error: No local variable `nope`
      1 │ pub fn main() { nope }
        ·                 ^^^^ No local variable `nope`
  demo.rn 1 file, 0 registry, 0 custom, 0 hooks

1 script did not load. Nothing they declared is in the list.
```

The counts are what each file contributed: file rules, registry rules, custom
cleaners and hooks. `--json` gives the same as one object, for a bug report.
The exit status is non-zero when a script failed to load, so a wrapper script can
notice.

## 🚀 How a script is run

1. **Load** — at startup the file is compiled. A compile error is reported and
   that file is skipped; the rest still work.
2. **`main()`** — called once, right after compiling. This is where a script
   declares what it cleans. A script without `main` is an error: nothing it
   declares could ever be reached.
3. **Handlers** — a `custom_cleaner` names a function. The cleaning job calls it
   with the expanded path when the user's selection reaches that cleaner.
4. **`on_started`** — a handler registered with `cleaner::on_started(name)` is
   called once before a real cleaning run starts deleting. Synchronous, so it
   can close a copy or take a backup while the files are still there. Never for a
   scan or a dry run.
5. **`on_finished` / `on_error`** — called once after a real run, with a
   [`Report`](#report). `on_finished` when the run went clean; `on_error` when it
   left something behind (a locked file, a denied directory), with the reason as
   a second argument. Never for a scan or a dry run.

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
    .category("Cache")                // required: the category it lives in
    .sub_category("Images")           // optional: the sub-category row
    .class("Utility")                 // optional: program class, defaults to "Other"
    .path("{username}/AppData/Local/My App/cache")   // required
    .os(["windows"])                  // optional: restrict to some systems
    .remove_all_in_dir()              // what to remove (at least one)
    .register();                      // commit it — call last
```

The program name here is the same string the built-in catalog uses, so a rule
for `"Google Chrome"` lands in the existing row instead of creating a second
one. A rule that names no category, or no path, is rejected with an error naming
the rule: both are what the user selects it by, so a guess would only make it
invisible.

Path placeholders, expanded exactly as for the built-in catalog:

| Placeholder | Becomes |
| --- | --- |
| `{username}` | The current user's name (`cleaner::user()`) |
| `{home}` | The current user's home directory (`cleaner::home()`) |
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

#### Narrowing what a rule deletes

The engine applies these on the same walk, so a `--dry-run` measures a narrowed
rule exactly as the real run would clean it. They are a property of the rule, not
of the files it happens to find today.

| Method | Effect |
| --- | --- |
| `.exclude(["user-data", "*.lock"])` | Skip entries whose *name* matches; a directory keeps everything under it |
| `.older_than(7)` | Only entries last modified more than 7 days ago |
| `.min_size("1 MB")` | Only entries at least this large (`"512 KB"`, `"2 GB"`, or a number of bytes) |
| `.max_size("2 GB")` | Only entries at most this large |
| `.max_depth(3)` | Stop descending past this level; `1` is the entries directly inside the path |
| `.skip_if_running(["chrome.exe"])` | Do nothing at all while one of these programs is open |
| `.to_trash()` | Move entries to the recycle bin instead of deleting, where the platform has one |
| `.paths([...])` | Additional paths for the same rule; one entry per path, one row in the results |

```rune
cleaner::file_cleaner("My App")
    .category("Logs")
    .path("{username}/AppData/Local/My App/logs")
    .paths(["{username}/AppData/Local/My App/crash"])
    .exclude(["keep.txt", "*.lock"])
    .older_than(30)
    .max_depth(2)
    .skip_if_running(["my-app.exe"])
    .to_trash()
    .remove_all_in_dir()
    .register();
```

`exclude` is matched against the entry name, not the full path, so `"*.lock"`
works at any depth. `skip_if_running` is the rule that keeps a cache clean while
the program that owns it is closed — the alternative is a half-cleaned cache the
program rewrites on its next start.

`to_trash` costs disk space until the user empties the bin, so use it for a
user's own data and leave system junk to plain deletion.

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
    .paths(["{username}/AppData/Roaming/My App/crash"])  // optional: more of the same
    .handler("clean_old")        // function name; defaults to "clean"
    .id("my-app-logs")           // optional: the id used to toggle it
    .glob()                      // optional: path is a glob, handler runs per match
    .sequential()                // optional: run it alone instead of in the pool
    .within("{username}/AppData/Roaming/My App")   // optional: the safe fence, see below
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

#### Fencing a handler with `within`

A handler decides for itself what to delete, which makes a path bug in it the one
mistake in this API with no undo. `.within(root)` is the answer: every
`cleaner::remove_*` call in that handler refuses to touch anything outside
`root`, and a refused path is not counted.

```rune
cleaner::custom_cleaner("My App")
    .category("Cache")
    .path("{username}/AppData/Local/My App/cache")
    .within("{username}/AppData/Local/My App")
    .handler("clean")
    .register();

pub fn clean(path) {
    // Even if `path` is empty or wrong, nothing outside the App directory goes.
    cleaner::remove_all(path);
}
```

The fence is resolved once, when the script loads, so `..` in a pattern or an
empty variable cannot move it. Use it for every handler that deletes by
computed path: it costs one check and turns "a script wiped my drive" into
"the rule did nothing".

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

### Deciding what to delete

| Function | Returns | Notes |
| --- | --- | --- |
| `cleaner::older_than(path, days)` | `bool` | Modified more than `days` days ago; `false` for a missing path |
| `cleaner::matches(path, pattern)` | `bool` | The *name* matching a glob, without expanding a pattern |
| `cleaner::unlocked(path)` | `bool` | Opens the file for writing right now — asks, does not damage |
| `cleaner::locked(path)` | `bool` | The inverse: another program holds it |
| `cleaner::lock_files(dir)` | `[String]` | Every file under `dir` that is locked right now |

```rune
pub fn clean(path) {
    for entry in cleaner::list(path) {
        // Only week-old temp files, and only ones nothing is still writing.
        if cleaner::matches(entry, "*.tmp")
            && cleaner::older_than(entry, 7)
            && cleaner::unlocked(entry)
        {
            cleaner::remove_file(entry);
        }
    }
}
```

`lock_files` is for the whole-directory decision — leave the cache alone while
one file in it is still open, rather than cleaning half of it:

```rune
let busy = cleaner::lock_files(path);

if busy.is_empty() {
    cleaner::remove_all(path);
} else {
    cleaner::log(`left ${path} alone: ${busy.len()} locked`);
}
```

### Paths

| Function | Returns | Notes |
| --- | --- | --- |
| `cleaner::user()` | `String` | What `{username}` expands to |
| `cleaner::home()` | `String` | What `{home}` expands to |
| `cleaner::appdata()` | `String` | `AppData\Roaming`, `~/.config`, `Application Support` |
| `cleaner::join([parts])` | `String` | Joined with the platform separator |
| `cleaner::parent(path)` | `Option<String>` | The containing directory |
| `cleaner::filename(path)` | `Option<String>` | The last component |
| `cleaner::expand(pattern)` | `[String]` | The rule-path placeholders, one entry per drive |

`join` takes its pieces, so a handler that still needs its argument clones it:
`cleaner::join([path.clone(), "cache"])`.

```rune
pub fn clean(path) {
    let stamp = cleaner::join([path.clone(), "last-run.txt"]);

    for candidate in cleaner::expand("{drive}/Users/{username}/AppData/Local/My App/tmp") {
        cleaner::log(candidate);
    }
}
```

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
| `cleaner::notify(title, body)` | `bool` — the app's own desktop notification |
| `cleaner::progress(text)` | One line in the app's log; the frontends show it as the current step |
| `cleaner::dry_run()` | `bool` — the run is a scan, not a real one |
| `cleaner::cancelled()` | `bool` — the user asked the run to stop |
| `cleaner::is_protected(path)` | `bool` — inside Windows, Program Files, the home root or this app |
| `cleaner::running(["chrome.exe"])` | `bool` — one of these programs is open |
| `cleaner::version()` | `String` — the Cross Cleaner version, e.g. `"2.0.4"` |
| `cleaner::require_api(n)` | Errors unless the API is at least level `n` |
| `cleaner::script_path()` | `String` — the file this handler runs from |

Prefer `notify` over `process` for a desktop notification: it works on every
platform without the script knowing which one it is.

Prefer `log`/`warn` over `println!` too: the TUI and the window app own the
screen, and terminal output from a worker thread tears the frame apart.

### A long handler that can be stopped

A handler that loops over 200 000 files has to check whether the user has given
up, or the window stays busy long after they asked it to stop:

```rune
pub fn clean(path) {
    for entry in cleaner::glob(`${path}/**/*.log`) {
        if cleaner::cancelled() {
            cleaner::warn("stopped on request");
            return;
        }

        cleaner::remove_file(entry);
    }
}
```

`dry_run()` is the other half of the same idea: a handler that runs during
`--dry-run` should look and report, not delete. Handlers are named as unmeasured
in a scan, so this only matters for a handler that still wants to say something
useful about what it would find.

### Hook: before the run (`on_started`)

```rune
pub fn main() {
    cleaner::on_started("prepare");
}

pub fn prepare() {
    // The files are all still there: this is the moment to close a copy,
    // take a backup, or check that the machine is idle.
    let marker = cleaner::join([cleaner::appdata(), "..", "Local", "My App", "cleaning.lock"]);
    cleaner::write(marker, "cleaning");
}
```

Synchronous, and called before the first deletion of a real run — a hook that
takes ten seconds delays the cleaning by ten seconds, which is the point. Never
for a scan or a dry run.

### Hook: after a run that went wrong (`on_error`)

```rune
pub fn main() {
    cleaner::on_error("report_problem");
}

pub async fn report_problem(report, problem) {
    // `problem` is a line like "3 files were still in use".
    let body = json::to_string(#{
        "text": `${report.summary()} — but: ${problem}`,
    })?;

    cleaner::http_post("https://example.com/hook", body);
}
```

A run that finished but left something behind calls `on_error` *instead of*
`on_finished`: one message per run, and the one worth sending is the one that
says something did not go to plan.

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
| `cleaner::registry::read_dword(path, name)` | `Option<u32>` | A `REG_DWORD` as the number it is |
| `cleaner::registry::read_qword(path, name)` | `Option<u64>` | A `REG_QWORD` as the number it is |
| `cleaner::registry::write(path, name, data)` | `bool` | Creates the key if needed |
| `cleaner::registry::write_dword(path, name, value)` | `bool` | Writes a `REG_DWORD` |
| `cleaner::registry::write_binary(path, name, data)` | `bool` | Writes a `REG_BINARY` from bytes |
| `cleaner::registry::values(path)` | `[String]` | Names of the values directly under the key |
| `cleaner::registry::values_full(path)` | `[RegistryValue]` | Names *with their type and text* |
| `cleaner::registry::keys(path)` | `[String]` | Names of the sub-keys, one level down |
| `cleaner::registry::subkeys(path, depth)` | `[String]` | Sub-keys `depth` levels down, parents before children |
| `cleaner::registry::export(path)` | `String` | The key and everything under it as a `.reg` file |
| `cleaner::registry::backup(path, file)` | `bool` | `export` written to `file` |
| `cleaner::registry::delete_value(path, name)` | `bool` | |
| `cleaner::registry::delete_key(path)` | `bool` | The key with everything under it |

`values` answers *which* values exist; `values_full` answers what they are, and
the type is the part that decides whether deleting is safe — a `REG_SZ` is
usually something a person typed, a `REG_BINARY` may be a license blob, and
neither is the cache a script set out to clear.

```rune
pub fn clean(path) {
    let base = "HKEY_CURRENT_USER\\Software\\My App\\Recent";

    // Every sub-key, three levels down; parents come first, so deleting front
    // to back never removes a key a later path still needs.
    for sub in cleaner::registry::subkeys(base, 3) {
        if sub.starts_with("session-") {
            cleaner::registry::delete_key(format!("{}\\{}", base, sub));
        }
    }
}
```

Look before deleting, and keep a copy the user can restore by double-clicking:

```rune
pub fn clean(path) {
    let base = "HKEY_CURRENT_USER\\Software\\My App\\Cache";

    for value in cleaner::registry::values_full(base) {
        // Only the counters this app regenerates, never anything typed by hand.
        if value.kind == "REG_DWORD" && value.name.starts_with("cache-") {
            let backup = cleaner::join([cleaner::appdata(), "My App", "cache.reg"]);

            if cleaner::registry::backup(base, backup) {
                cleaner::registry::delete_value(base, value.name);
            }
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
| `started_at` | `u64` | When the run started (Unix seconds); `0` if no frontend recorded it |
| `duration` | `u64` | How long the run took, in seconds; `0` if the start was not recorded |
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

### `RegistryValue`

One row of `cleaner::registry::values_full`.

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | `String` | The value's name; empty for the default value |
| `kind` | `String` | The registry type: `"REG_SZ"`, `"REG_DWORD"`, `"REG_BINARY"`, … |
| `text` | `String` | The number for `REG_DWORD`/`REG_QWORD`, the string itself, `<N bytes>` for binary |

### `Report` totals for a run that left something behind

| Member | Type | Meaning |
| --- | --- | --- |
| `locked_files` | `u64` | Files another program still held open |
| `locked_bytes` | `u64` | What those files would have freed |
| `denied_files` | `u64` | Files the system refused to delete |
| `denied_bytes` | `u64` | What those files would have freed |
| `.incomplete()` | `bool` | Either of the two counts is non-zero |

Read these instead of parsing the reason string an `on_error` handler receives:
the string is for a message to a person, the numbers are for a decision.

## 📚 Standard library

Installed for every script:

| Module | Comes from | What it gives |
| --- | --- | --- |
| `std` | Rune | Strings, vectors, iterators, `Option`, `Result`, `format!`, `std::io`, `std::fmt` |
| `fs` | rune-modules | `fs::read_to_string(path).await` |
| `json` | rune-modules | `json::to_string(value)`, `json::from_string(text)` |
| `rand` | rune-modules | Random numbers and choices |
| `time` | rune-modules | `time::sleep(duration).await` |
| `process` | rune-modules | `process::Command` — run an external program. **Off by default**, see below |

`process` is the one library that can bypass everything that makes cleaning
measurable: `process::Command` running `Remove-Item -Recurse` deletes files the
engine never counted and the user never saw in the plan. It is therefore opt-in,
through the config file or the environment:

```text
# config
scripts_allow_process = true

# or, for one run
CROSS_CLEANER_SCRIPTS_PROCESS=1 cross-cleaner clean
```

With it off, a script that mentions `process::Command` fails to load with an
error saying so — a clear error is better than a handler that silently does
nothing. With it on, prefer `cleaner::notify` and the `cleaner::remove_*` calls
for anything they can already do.

```rune
pub async fn notify(report) {
    // Give the desktop a notification too, next to the webhook.
    cleaner::notify("Cross Cleaner", report.summary());

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