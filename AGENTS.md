# 🤖 AGENTS.md — rules for AI agents

This file tells AI agents (coding assistants, autonomous agents, bots) how to
work inside the **Cross Cleaner** repository. Read it before making any change.

Human contributors should read [CONTRIBUTING.md](CONTRIBUTING.md) instead —
everything about commits below is a summary of it, and it stays the source of
truth.

## 📌 What this project is

Cross Cleaner is a system cleanup tool written in Rust: it removes temporary
files, caches and other system junk. It is a Cargo **workspace** that builds
three frontends (CLI, TUI, desktop GUI) plus an Android edition, all sharing one
cleaning engine and one cleaning database. It targets Windows, Linux, macOS and
Android; release builds are small, portable binaries.

Build and packaging details live in [BUILD.md](BUILD.md).

## 🗂️ Project structure

```text
Cargo.toml, Cargo.lock      Workspace root; release profile is size-optimized
crates/
  appcore/                  Shared application core (config, categories,
                            cleaning orchestration for the GUI apps, updater,
                            sounds, browser helpers)
  cleaner/                  The cleaning engine: path matching, deletable
                            entries, custom cleaners, image optimizer
  database/                 Cleaning database: structures, parsing, registry
                            support and the platform JSON catalogs
  cli/                      Command-line edition (categories / programs /
                            plan / clean / update)
  tui/                      Terminal UI edition
  gui/                      GUI toolkit code (app, config, taskbar,
                            notifications, theming)
  desktop/                  Desktop GUI binary (window edition)
  android/                  Android edition support
  selfupdate/               Self-update / update checks (behind the
                            `self-update` Cargo feature)
  winicon/                  Windows icon resource + build dependency
  script-api/               Rune scripting API for user cleaners
.github/
  workflows/                CI: dev_build, release, publish, aur,
                            update_program_lists
  actions/                  Reusable actions: aur-prepare, aur-publish,
                            linux-package
packaging/
  aur/                      AUR packaging (PKGBUILD, -bin PKGBUILD)
  linux/                    Linux packaging (desktop files, appdata, nfpm,
                            AppRun, maintainer scripts)
LIST_WINDOWS.md / LIST_REGISTRY.md / LIST_LINUX.md / LIST_MACOS.md /
LIST_ANDROID.md / LIST_CUSTOM.md   Generated catalogs — see below
build_setup.iss             Windows Inno Setup installer script
```

Versioning note: release versions have four components (`2.0.4.2.1`); the
`Cargo.toml` files carry only the first three.

## ✍️ Commit rules — mandatory

If the agent ever creates a commit in this repository, it **must** follow
[CONTRIBUTING.md](CONTRIBUTING.md#-commit-messages). In short:

- Format: `<type>(<optional scope>): <description>` — Conventional Commits.
- `type` is lowercase, from the fixed set: `feat`, `fix`, `perf`, `chore`,
  `build`, `ci`, `docs`, `refactor`, `test`, `style`, `revert`.
- Description: imperative mood, lowercase, no trailing period
  (`fix: guard against an empty program list`, not `Fixed the bug.`).
- Breaking changes: add `!` before the colon and a `BREAKING CHANGE:` paragraph
  in the body.
- Scopes in this repo: `cli`, `tui`, `ui`/`gui`, `database`, `cleaner`,
  `android`, `selfupdate`, `build`, `ci`/`release`, `deps`,
  `windows`/`linux`/`macos`.
- **The commit messages are the changelog** — release notes are generated from
  the commits between release tags, so a wrong type hides (or misplaces) a real
  change in the release notes. Label `feat`, `fix` and `perf` precisely.
- One concern per commit; do not mix a bugfix with reformatting or refactoring.
- Do not rewrite `main` history between releases (no rebase/force push that
  drops already-pushed commits from the release range).
- Do not hand-replicate CI-generated commits:
  `chore: apply rustfmt` (run `cargo fmt` locally instead), the release
  version bumps, and the database sorting commits belong to the CI jobs.

Before finishing work, run:

```bash
cargo fmt
cargo test
cargo build --no-default-features
```

## 🚫 Forbidden edits — cleaning databases

**AI agents must not edit, add entries to, remove entries from, reformat or
reorder the cleaning database files.** This is a hard rule, not a preference:

- `crates/database/windows_database.json`
- `crates/database/linux_database.json`
- `crates/database/macos_database.json`
- `crates/database/android_database.json`
- `crates/database/registry_database.json`
- `crates/database/program_links.json`

These files are the safety-critical data of the project: an incorrect pattern
there can delete user data on real machines. Changes to them are made by
humans, and the CI `sort_json` job owns their ordering (sorted by `program`,
case-insensitive), so hand edits are doubly unwelcome.

If a task seems to require a new cleaning entry in the JSON databases above:

1. Ask the user / a maintainer to make (or approve) the data change.
2. At most, prepare a concrete proposal (exact JSON snippets, sources) in a
   comment, issue or PR description — not as a direct edit to the files above.

### What *is* allowed: `crates/cleaner/src/custom_cleaners.rs`

Unlike the JSON databases, built-in custom cleanings live in code:
`crates/cleaner/src/custom_cleaners.rs`. **AI agents may edit this file.**
It is the supported place to add cleaning logic (via `custom_glob_cleaner!`
and friends), subject to one hard constraint: keep the `id` / `program` /
`category` / `path` field formatting intact — the CI parses that file with a
regex to build the "Built-in custom cleanings" docs, and changing the field
layout breaks the generated `LIST_CUSTOM.md`.

Care is still required: these cleaners run deletes on real machines, so prefer
precise globs, never widen a pattern "just in case", and describe the effect in
the commit message (`feat(cleaner): ...`, `fix(cleaner): ...`).

## 🚫 Generated files — do not edit by hand

These are produced by CI and every manual change will be overwritten:

| Files | Generated by |
| --- | --- |
| `LIST_WINDOWS.md`, `LIST_REGISTRY.md`, `LIST_LINUX.md`, `LIST_MACOS.md`, `LIST_ANDROID.md`, `LIST_CUSTOM.md` | `update_program_lists.yml` |
| `version =` lines in `Cargo.toml` / `Cargo.lock` | release workflow |
| `version =` line in `flake.nix` | `update_flake.yml` (dispatched by the release workflow) |
| Order of `crates/database/*_database.json` | `sort_json` job |

Edit the sources instead:

- program catalogs: `crates/database/*_database.json` — **humans only** (above);
- built-in custom cleanings: `crates/cleaner/src/custom_cleaners.rs`;
- catalog table links: `crates/database/program_links.json` — also forbidden
  for direct AI edits (see above).

When editing `custom_cleaners.rs`, keep the `id` / `program` / `category` /
`path` field formatting intact — the CI parses it with a regex to build the
"Built-in custom cleanings" docs.

## 🌿 Branching and PRs

- Branch off `main`, one topic per pull request.
- PR titles must be specific and user-facing; describe what changed and how to
  verify it. Mention platform-specific effects explicitly — the project builds
  for four platforms, and a one-platform change must say so.
- CI (`.github/workflows/dev_build.yml`) runs on every PR or push touching
  Rust, Cargo, JSON or workflow files.

## 📄 License

Contributions are licensed under the [GNU GPL v3.0](LICENSE), same as the
project.
