<div align="center">
<h1>🌟 Cross Cleaner 🌟</h1>
<br>
An addon-style, bloat-free <a href="https://github.com/Cross-Cleaner/Cross-Cleaner">system cleanup tool</a> that removes temporary files, cache and other system junk from your computer.
<br>
<a href="https://www.rust-lang.org"><img src="https://img.shields.io/static/v1?label=Made%20with&message=Rust&logo=rust&labelColor=e82833&color=b11522" alt="Made with Rust"></a>
<a href="https://github.com/Cross-Cleaner/Cross-Cleaner/blob/main/LICENSE"><img src="https://img.shields.io/github/license/Cross-Cleaner/Cross-Cleaner?logo=mdBook" alt="Github License"></a>
<br>
<a href="https://github.com/Cross-Cleaner/Cross-Cleaner/actions"><img src="https://github.com/Cross-Cleaner/Cross-Cleaner/actions/workflows/dev_build.yml/badge.svg" alt="Build Status"></a>
<a href="https://github.com/Cross-Cleaner/Cross-Cleaner/releases"><img src="https://img.shields.io/github/downloads/Cross-Cleaner/Cross-Cleaner/total" alt="Downloads"/></a>
<a href="https://github.com/Cross-Cleaner/Cross-Cleaner/issues"><img src="https://img.shields.io/github/issues/Cross-Cleaner/Cross-Cleaner" alt="GitHub Issues"/></a>
<a href="https://github.com/Cross-Cleaner/Cross-Cleaner/stargazers"><img src="https://badgen.net/github/stars/Cross-Cleaner/Cross-Cleaner" alt="GitHub Stars"/></a>
<br>
<a href="https://discord.gg/wmJdUBaztX"><img src="https://img.shields.io/badge/support/help/issues-discord-brightgreen" alt="Discord"/></a>
<br>
<p>Join our Discord server for support, updates and community discussions 🤫</p>
</div>

## 📌 About the Project

**Cross Cleaner** is a high-performance tool for cleaning temporary files, cache, and other system "junk" from your computer. Built with Rust for optimal speed and reliability.

### Key Features

- 🚀 **Multi-threaded**: Leverages rayon for parallel processing on multi-core systems
- 🔒 **Secure**: Carefully preserves critical system files
- 💻 **Cross-Platform**: Full support for [Windows](https://github.com/Cross-Cleaner/Cross-Cleaner/blob/main/LIST_WINDOWS.md), [MacOS](https://github.com/Cross-Cleaner/Cross-Cleaner/blob/main/LIST_MACOS.md), [Linux](https://github.com/Cross-Cleaner/Cross-Cleaner/blob/main/LIST_LINUX.md) and [Android](https://github.com/Cross-Cleaner/Cross-Cleaner/blob/main/LIST_ANDROID.md)
- 🎯 **User-Friendly**: Clean, minimalist interface for easy operation
- 📄 **Custom-DataBase**: Ability to use custom cleanup database

### Demo Desktop Edition
<img width="570" height="557" alt="image" src="https://github.com/user-attachments/assets/069cf3ab-9515-4daf-a6f2-99bc28de24ba" />

### Demo TUI Edition
<img width="560" height="234" alt="image" src="https://github.com/user-attachments/assets/16000f98-a27d-444a-b88e-1060377e5cb9" />

## 🖥️ Command Line Edition

The same cleaner, driven from a terminal. It shares the database, the
selection rules and the cleaning engine with the two apps above — the only
thing that changes is how the selection is made.

```bash
# What can be cleaned?
cli categories
cli programs -c Cache

# What would a selection touch, without touching it?
cli plan -c Cache -c Logs --paths

# Clean it.
cli clean -c Cache -c Logs
cli clean --all                      # everything, like the window app's "clean all"
cli clean -c "Cache/Browser"         # one subcategory
cli clean -p Chrome                  # one program, all of its categories
cli clean -p "Chrome=Logs"           # one program, one of its categories
cli clean -a -e Discord              # everything except one program

# See what it would actually free, without deleting anything.
cli clean -c Cache --dry-run

# For a script.
cli clean -a --json | jq .bytes
```

Selection is as detailed as in the terminal app — a category, a subcategory of
one, a whole program, or a program narrowed to some of its categories. Names are
matched case-insensitively, and a name that does not exist is an error with the
closest real names next to it rather than a run that quietly cleans something
else.

`--dry-run` walks the same code the cleaner itself uses, so it measures rather
than estimates. Files another program is holding open are counted separately
instead of being promised as free, and cleaners that cannot be measured without
running (custom cleaners, registry entries) are named as such:

```text
$ cli clean -c Browser --dry-run
Would free 0 bytes in 0 files and 0 directories across 1 program

  Vivaldi  0 B  0 files · 0 directories

7 files are in use and will not be removed:
  7.6 MB held back — close the programs using them and run again.
```

The walk costs about as much I/O as the run does, so a full `--all` scan is not
instant. That is the price of an honest number.

| Command | What it does |
|---|---|
| `cli categories` | Categories with their subcategories and entry counts |
| `cli programs [-c CAT]` | Programs of the selected categories, with a search filter |
| `cli plan [selection]` | Which database patterns a selection covers, with `--paths` |
| `cli clean [selection]` | Clean it, or measure it with `--dry-run` |
| `cli update` | Check for and install a newer release |

Common flags: `-a/--all`, `-c/--category`, `-x/--exclude-category`,
`-p/--program`, `-e/--exclude-program`, `-y/--yes`, `-v/--verbose`,
`-q/--quiet`, `--json`, `--no-color`, `--dry-run`.

Exit codes: `0` success, `1` a failure, `130` interrupted with Ctrl-C.

## 🧩 User Scripts

Your own cleanings, written in [Rune](https://rune-rs.github.io/) — a small
Rust-like language — and picked up by every edition (CLI, TUI, desktop). A
script adds programs and patterns to the same lists the built-in catalog lives
in, so they are selected, counted and cleaned exactly like everything else.

Drop `*.rn` files into the `Scripts` folder:

| System | Location |
|---|---|
| Windows | `%APPDATA%\Cross-Cleaner\Scripts` |
| Linux | `~/.config/Cross-Cleaner/Scripts` |
| macOS | `~/Library/Application Support/Cross-Cleaner/Scripts` |

A complete script — a static rule, a rule that decides at cleaning time, and a
Telegram notification when the run is over:

```rune
pub fn main() {
    // Everything in the cache folder goes, and the folder itself goes too.
    cleaner::file_cleaner("My App")
        .category("Cache")
        .path("{username}/AppData/Local/My App/Cache")
        .remove_all_in_dir()
        .remove_directory_after_clean()
        .register();

    // Hand-picked files: the handler runs at cleaning time and receives the
    // expanded path. Only the ones it removes are counted.
    cleaner::custom_cleaner("My App")
        .category("Logs")
        .path("{username}/AppData/Roaming/My App/logs")
        .handler("clean_logs")
        .register();

    // Called once after every cleaning run, with what was freed.
    cleaner::on_finished("notify");
}

pub fn clean_logs(path) {
    let week = 7 * 24 * 60 * 60;
    let cutoff = cleaner::now() - week;

    for file in cleaner::glob(`${path}/**/*.log`) {
        let old = match cleaner::modified(file) {
            Some(when) => when < cutoff,
            None => false,
        };

        if old {
            cleaner::remove_file(file);
        }
    }
}

pub async fn notify(report) {
    // `report` carries the totals and one row per program.
    let body = json::to_string(#{ "text": report.details() })?;
    cleaner::http_post("https://api.telegram.org/bot<token>/sendMessage", body);
}
```

What a script can reach:

| | |
|---|---|
| `cleaner::file_cleaner(..)` | Declarative paths, globs and whole folders |
| `cleaner::registry_cleaner(..)` | Declarative registry keys and values (Windows) |
| `cleaner::custom_cleaner(..)` | A path plus your own handler |
| `cleaner::glob`, `list`, `is_file`, `modified`, `now` | Look before you delete |
| `cleaner::remove_file`, `remove_files`, `remove_all` | Delete, and count what went |
| `cleaner::registry::{read, values, keys, delete_key, ..}` | Inspect and prune the registry (Windows) |
| `cleaner::on_finished(handler)` | Report the run: bytes, files, folders, per program |
| `cleaner::http_get`, `http_post`, `log`, `warn` | Webhooks and diagnostics |

Rune's `std` (strings, iterators, `Option`, `Result`, `format!`, `std::io`,
`std::fmt`) is available, plus the [rune-modules] `fs`, `json`, `rand`, `time`
and `process` modules. It is Rust-*like*, not Rust: no raw strings, no traits,
and backtick template literals interpolate with `${...}`.

A script that fails to compile, or whose `main` panics, is reported and skipped
without touching the databases it had half-registered; the others keep working.
`cli --no-scripts true` (and `--no-scripts` in the apps' config) turns them off
entirely, including the `on_finished` hooks.

[rune-modules]: https://docs.rs/rune-modules

## 📥 Installation

### Option 1: Download Pre-built Binary
Get the latest release from our [releases page](https://github.com/Cross-Cleaner/Cross-Cleaner/releases).

### Option 2: Arch Linux / AUR
Install the pre-built package with your favorite AUR helper:
```bash
yay -S cross-cleaner-bin
```

### Option 3: Nix / NixOS
The repository ships a flake that builds all three Linux front-ends (with the
self-updater compiled out, since Nix owns the installed version):
```bash
nix run github:Cross-Cleaner/Cross-Cleaner              # window app
nix run github:Cross-Cleaner/Cross-Cleaner#cross-cleaner-tui   # terminal app
nix run github:Cross-Cleaner/Cross-Cleaner#cross-cleaner-cli   # headless CLI
```
Or install it into your profile:
```bash
nix profile install github:Cross-Cleaner/Cross-Cleaner
```

### Option 4: Build from Source

1. Make sure you have [Rust](https://www.rust-lang.org/) installed (version 1.70 or higher):
```bash
rustc --version
```

2. Clone the repository:
```bash
git clone https://github.com/Cross-Cleaner/Cross-Cleaner.git
cd Cross-Cleaner
```

3. Build the project:
```bash
cargo build --release
```

4. The compiled binary will be located in `target/release`

## 🤝 Contributing

Pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for build
checks, commit message conventions (they end up in the release notes) and the
maintainer release process.
