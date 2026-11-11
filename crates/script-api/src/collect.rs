//! Where registrations land while a script runs.
//!
//! Scripts call `register()` from their `main()`, but the host is the one that
//! decides what to do with the result: declarative rules are layered over the
//! cleaner databases, custom cleaners get a compiled program attached, and a
//! script that fails half-way is rolled back as a whole. So the script-facing
//! API does not touch any registry itself — it only records what was declared
//! into the collector installed for the script currently running.

use std::path::PathBuf;
use std::sync::Mutex;

use database::structures::CleanerData;

use crate::ScriptError;
use crate::api::RegistryRule;

/// A custom cleaner declared by a script.
///
/// Not a [`database::structures::CustomCleaner`] yet: the compiled program only
/// exists once the script's `main()` has returned, and the id is only unique
/// once the script's file name is known.
#[derive(Debug, Clone)]
pub(crate) struct CustomDefinition {
    pub id: String,
    pub program: String,
    pub category: String,
    pub sub_category: String,
    pub path: String,
    pub os: Vec<String>,
    pub handler: String,
    pub glob: bool,
    pub sequential: bool,
    /// Fence for every removal the handler makes. `None` means the handler is
    /// not fenced, which is what a script that does not call `within()` gets.
    pub within: Option<String>,
}

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookKind {
    /// Before the deletions start, once per real run.
    Started,
    /// After the run, with the report.
    Finished,
    /// After a run that left something behind — denied or locked.
    Failed,
}

/// A hook declared by a script.
#[derive(Debug, Clone)]
pub(crate) struct HookDefinition {
    pub kind: HookKind,
    pub handler: String,
}

/// Everything the scripts of one directory declared.
#[derive(Default)]
pub(crate) struct Collected {
    pub files: Vec<CleanerData>,
    pub registry: Vec<RegistryRule>,
    pub custom: Vec<CustomDefinition>,
    /// Handlers registered with `cleaner::on_started` and friends.
    pub hooks: Vec<HookDefinition>,
    pub errors: Vec<ScriptError>,
    script: Option<PathBuf>,
}

/// How far the collector had got before a script started.
///
/// A script that fails is rolled back to its mark: a `main()` that registered
/// two of three rules and then panicked must not leave the first two behind,
/// or the user sees half a cleaner and no explanation for the missing half.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Mark {
    pub files: usize,
    pub registry: usize,
    pub custom: usize,
    pub hooks: usize,
    pub errors: usize,
}

static COLLECTOR: Mutex<Option<Collected>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Collected) -> R) -> Option<R> {
    let mut slot = match COLLECTOR.lock() {
        Ok(slot) => slot,
        // A panic while collecting must not turn every later registration into
        // a panic of its own; recover the data and carry on.
        Err(poisoned) => poisoned.into_inner(),
    };

    slot.as_mut().map(f)
}

/// Starts collecting. Called once per load, before any script runs.
pub(crate) fn begin() {
    let mut slot = match COLLECTOR.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    *slot = Some(Collected::default());
}

/// Takes everything collected so far. Called once per load, after the last
/// script ran.
pub(crate) fn finish() -> Collected {
    let mut slot = match COLLECTOR.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.take().unwrap_or_default()
}

/// Names the script whose registrations come next, so errors can point at it.
pub(crate) fn set_script(path: &std::path::Path) {
    with(|collected| collected.script = Some(path.to_path_buf()));
}

pub(crate) fn mark() -> Mark {
    counts()
}

/// What the collector holds now, in the same shape as a [`Mark`].
pub(crate) fn counts() -> Mark {
    with(|collected| Mark {
        files: collected.files.len(),
        registry: collected.registry.len(),
        custom: collected.custom.len(),
        hooks: collected.hooks.len(),
        errors: collected.errors.len(),
    })
    .unwrap_or_default()
}

/// Drops everything registered after `mark`.
pub(crate) fn rollback(mark: Mark) {
    with(|collected| {
        collected.files.truncate(mark.files);
        collected.registry.truncate(mark.registry);
        collected.custom.truncate(mark.custom);
        collected.hooks.truncate(mark.hooks);
    });
}

/// The custom cleaners declared after `mark`, in declaration order.
pub(crate) fn custom_since(mark: Mark) -> Vec<CustomDefinition> {
    with(|collected| collected.custom[mark.custom..].to_vec()).unwrap_or_default()
}

/// The hooks declared after `mark`, in declaration order.
pub(crate) fn hooks_since(mark: Mark) -> Vec<HookDefinition> {
    with(|collected| collected.hooks[mark.hooks..].to_vec()).unwrap_or_default()
}

pub(crate) fn file(entry: CleanerData) {
    with(|collected| collected.files.push(entry));
}

pub(crate) fn registry(rule: RegistryRule) {
    with(|collected| collected.registry.push(rule));
}

pub(crate) fn custom(definition: CustomDefinition) {
    with(|collected| collected.custom.push(definition));
}

pub(crate) fn hook(kind: HookKind, handler: String) {
    with(|collected| collected.hooks.push(HookDefinition { kind, handler }));
}

/// Reports a rule a script asked for but that cannot be used.
pub(crate) fn error(message: impl Into<String>) {
    with(|collected| {
        let path = collected.script.clone().unwrap_or_default();
        collected
            .errors
            .push(ScriptError::new(path, message.into()));
    });
}
