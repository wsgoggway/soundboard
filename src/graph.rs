//! Audio graph management: an idempotent desired-state reconcile over the
//! PulseAudio (PipeWire) session.
//!
//! The desired graph (contract: `docs/pipewire-recipe.md`):
//!
//! - null-sink `soundboard` (description "Meme_Soundboard") — clips play here;
//! - null-sink `virtmic` (descriptions "Virtual_Mic_sink" for the sink,
//!   "Virtual_Mic" for its monitor source) — conferencing apps capture
//!   `virtmic.monitor`;
//! - loopback `soundboard.monitor -> virtmic` (always);
//! - loopback `<real_mic> -> virtmic` (only when `config.mic_mix`);
//! - loopback `soundboard.monitor -> <real_sink>` (only when
//!   `config.monitoring`).
//!
//! `real_mic` is the current default source, excluding any `*.monitor`; if the
//! default source is itself a monitor (or unset), the mic-mix loopback is
//! skipped with a warning. `real_sink` is the current default sink. Both are
//! resolved at reconcile time, so `soundboard up` re-targets the loopbacks
//! when the user changes default devices.
//!
//! NEVER used on purpose (documented broken variants, see the recipe):
//! `media.class=Audio/Source/Virtual` null-sinks, `pw-loopback`.
//!
//! Ownership is persistent, not heuristic: every module index we load is
//! recorded in a versioned state file
//! (`$XDG_STATE_HOME/soundboard/modules.json`, see [`default_state_path`]).
//!
//! - A module is OURS iff its index is in that state file AND it still
//!   exists AND its name still matches the recorded expected name
//!   (`module-null-sink` / `module-loopback`). Everything else is foreign
//!   and never unloaded — including a user's own lookalike module we never
//!   loaded.
//! - Adoption rule (reconcile/up only): a module shaped like ours but NOT in
//!   the state file is adopted (added to the state, no reload) iff it carries
//!   EXACTLY our desired args for its slot (normalized compare); this makes
//!   `up` a no-op after state-file loss instead of duplicating modules.
//! - Anything shaped like ours that is neither owned nor adoptable is left
//!   alone and listed in `Report.warnings` ("foreign loopback on virtmic
//!   left alone: ...").
//! - Teardown (down) unloads only state-listed modules; it does not adopt.
//!   Consequence: after losing the state file while the graph is up, `down`
//!   leaves the orphaned modules running — run `up` once (it adopts/reloads
//!   them back into the state), then `down`.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context as _, bail};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::{ModuleInfo as PulseModuleInfo, ServerInfo};
use libpulse_binding::context::{Context, FlagSet, State as ContextState};
use libpulse_binding::def;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{self, Operation};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Sink the clips are played into.
pub const SINK_MEMES: &str = "soundboard";
/// Monitor source of [`SINK_MEMES`].
pub const SOURCE_MEMES_MONITOR: &str = "soundboard.monitor";
/// Sink acting as the virtual microphone.
pub const SINK_VIRTMIC: &str = "virtmic";
/// Monitor source of [`SINK_VIRTMIC`] — what Zoom selects as "Virtual_Mic".
/// Contract constant; reserved for the UI (M3) and doctor output.
#[allow(dead_code)]
pub const SOURCE_VIRTMIC_MONITOR: &str = "virtmic.monitor";
/// Human-readable hint appended to `doctor` output.
pub const ZOOM_HINT: &str = "Zoom: Settings \u{2192} Audio \u{2192} Microphone = \"Virtual_Mic\"";

const MODULE_NULL_SINK: &str = "module-null-sink";
const MODULE_LOOPBACK: &str = "module-loopback";

/// A PulseAudio module as seen through introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    /// Module index in the server.
    pub index: u32,
    /// Module name, e.g. `module-null-sink`.
    pub name: String,
    /// Module argument string, e.g. `sink_name=soundboard ...`.
    pub args: String,
}

/// Logical slot a module of ours occupies; the unit of desired-state matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    SinkMemes,
    SinkVirtmic,
    LoopbackMemes,
    LoopbackMic,
    LoopbackMonitor,
}

impl Slot {
    /// Human-readable label for reports.
    fn label(self) -> &'static str {
        match self {
            Slot::SinkMemes => "sink soundboard",
            Slot::SinkVirtmic => "sink virtmic",
            Slot::LoopbackMemes => "loopback soundboard.monitor->virtmic",
            Slot::LoopbackMic => "loopback real-mic->virtmic",
            Slot::LoopbackMonitor => "loopback soundboard.monitor->real-sink",
        }
    }
}

/// Shape-classifies a module into one of our slots; `None` for anything else.
/// This is only the endpoint shape (name + key args): whether such a module
/// is actually ours is decided by the persisted state (see [`ModuleState`]) —
/// shape alone must never make a module unloadable.
fn classify(module: &ModuleInfo) -> Option<Slot> {
    let kv = args_kv(&module.args);
    match module.name.as_str() {
        MODULE_NULL_SINK => match kv.get("sink_name").map(String::as_str) {
            Some(SINK_MEMES) => Some(Slot::SinkMemes),
            Some(SINK_VIRTMIC) => Some(Slot::SinkVirtmic),
            _ => None,
        },
        MODULE_LOOPBACK => {
            let source = kv.get("source").map(String::as_str);
            let sink = kv.get("sink").map(String::as_str);
            if source == Some(SOURCE_MEMES_MONITOR) && sink == Some(SINK_VIRTMIC) {
                Some(Slot::LoopbackMemes)
            } else if sink == Some(SINK_VIRTMIC) {
                Some(Slot::LoopbackMic)
            } else if source == Some(SOURCE_MEMES_MONITOR) {
                Some(Slot::LoopbackMonitor)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parses `key=value` tokens from a module argument string. Values may carry
/// surrounding quotes; keys we match on (`sink_name`, `source`, `sink`) never
/// contain spaces, so whitespace splitting is safe.
fn args_kv(args: &str) -> BTreeMap<String, String> {
    args.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string()))
        .collect()
}

/// Whitespace-collapsed form of an args string, for equality checks.
fn normalize_args(args: &str) -> String {
    args.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Schema version of the module state file; bump on incompatible changes
/// (older/unknown versions are treated as empty state).
const STATE_VERSION: u32 = 1;

/// One module recorded as ours in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedModule {
    /// Module index in the server at load time.
    pub index: u32,
    /// Expected module name (guard against pid-style index reuse by an
    /// unrelated module): `module-null-sink` or `module-loopback`.
    pub name: String,
}

/// Persisted registry of the module indices this tool loaded
/// (`$XDG_STATE_HOME/soundboard/modules.json`, see [`default_state_path`]).
///
/// Written after every reconcile/teardown. A missing, corrupt or
/// unknown-version file is treated as empty state (the graph then re-adopts
/// exact-args modules on the next `up`; see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleState {
    /// Schema version, see [`STATE_VERSION`].
    pub version: u32,
    /// Modules we loaded and have not unloaded yet.
    pub modules: Vec<OwnedModule>,
}

impl ModuleState {
    /// Empty state (nothing owned yet).
    #[must_use]
    pub fn empty() -> Self {
        ModuleState {
            version: STATE_VERSION,
            modules: Vec::new(),
        }
    }

    /// True when `m` is ours: its (index, name) pair is recorded, the name is
    /// one of the module types we load, and the module still has our endpoint
    /// shape (defense against an index recycled by a same-named foreign
    /// module).
    fn is_owned(&self, m: &ModuleInfo) -> bool {
        (m.name == MODULE_NULL_SINK || m.name == MODULE_LOOPBACK)
            && classify(m).is_some()
            && self
                .modules
                .iter()
                .any(|o| o.index == m.index && o.name == m.name)
    }

    /// Records `m` as ours (adoption or fresh load); no-op when present.
    fn adopt(&mut self, m: &ModuleInfo) {
        if !self.modules.iter().any(|o| o.index == m.index) {
            self.modules.push(OwnedModule {
                index: m.index,
                name: m.name.clone(),
            });
        }
    }

    /// Drops the record for `index` (module unloaded).
    fn remove(&mut self, index: u32) {
        self.modules.retain(|o| o.index != index);
    }

    /// Prunes records whose module no longer exists on the server (e.g. the
    /// pulse server restarted and indices died with it).
    fn retain_live(&mut self, live_indices: &HashSet<u32>) {
        self.modules.retain(|o| live_indices.contains(&o.index));
    }

    /// Loads the state file; a missing, unreadable, corrupt or
    /// unknown-version file is treated as empty state (never an error: a
    /// stale registry must not brick the graph commands).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::empty();
        };
        match serde_json::from_str::<ModuleState>(&text) {
            Ok(state) if state.version == STATE_VERSION => state,
            _ => Self::empty(),
        }
    }

    /// Writes the state file, creating parent directories.
    ///
    /// # Errors
    /// Fails when the file cannot be written (the caller surfaces it: without
    /// a persisted registry the next `down` cannot find our modules).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create state dir {}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).context("failed to serialize module state")?;
        std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
    }
}

/// Default module state file location:
/// `$XDG_STATE_HOME/soundboard/modules.json` (typically
/// `~/.local/state/soundboard/modules.json`).
///
/// # Errors
/// Fails when the XDG state directory cannot be determined.
pub fn default_state_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::state_dir().ok_or_else(|| {
        anyhow::anyhow!("cannot determine the XDG state directory (set XDG_STATE_HOME or HOME)")
    })?;
    Ok(dir.join("soundboard").join("modules.json"))
}

/// One applied change in a [`Report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// What was done.
    pub action: ChangeAction,
    /// Which slot this concerned.
    pub slot: String,
    /// Module name.
    pub module: String,
    /// Module arguments (target args for `Load`/`Reload`).
    pub args: String,
    /// Module index after the change (`None` for unloads).
    pub index: Option<u32>,
}

/// Kind of applied change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    /// Module was loaded.
    Load,
    /// Module was unloaded.
    Unload,
    /// Module had wrong args: unloaded, then loaded again.
    Reload,
}

/// Outcome of a reconcile/teardown run.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Applied changes, in order.
    pub changes: Vec<Change>,
    /// Non-fatal problems (e.g. mic-mix requested but no real mic found).
    pub warnings: Vec<String>,
}

impl Report {
    /// True when nothing changed and nothing needs attention.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty() && self.warnings.is_empty()
    }

    /// Human-readable multi-line summary ("no changes" when noop).
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_noop() {
            return "no changes".to_string();
        }
        let mut lines = Vec::new();
        for c in &self.changes {
            let verb = match c.action {
                ChangeAction::Load => "loaded",
                ChangeAction::Unload => "unloaded",
                ChangeAction::Reload => "reloaded",
            };
            let index = c
                .index
                .map_or_else(String::new, |i| format!(" (index {i})"));
            lines.push(format!("{} {} [{}]{}", verb, c.module, c.slot, index));
        }
        for w in &self.warnings {
            lines.push(format!("warning: {w}"));
        }
        lines.join("\n")
    }
}

/// A module we want present, with the exact args it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Desired {
    slot: Slot,
    module: &'static str,
    args: String,
}

fn null_sink_args(
    sink_name: &str,
    sink_description: &str,
    source_description: Option<&str>,
) -> String {
    let mut args = format!(
        "sink_name={sink_name} media.class=Audio/Sink sink_properties=device.description={sink_description}"
    );
    if let Some(src_desc) = source_description {
        args.push_str(&format!(" source_properties=device.description={src_desc}"));
    }
    args
}

/// Builds the desired module list for the current config and session state.
fn desired_state(
    backend: &mut dyn PulseBackend,
    cfg: &Config,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Desired>> {
    let mut out = vec![
        Desired {
            slot: Slot::SinkMemes,
            module: MODULE_NULL_SINK,
            args: null_sink_args(SINK_MEMES, "Meme_Soundboard", None),
        },
        Desired {
            slot: Slot::SinkVirtmic,
            module: MODULE_NULL_SINK,
            args: null_sink_args(SINK_VIRTMIC, "Virtual_Mic_sink", Some("Virtual_Mic")),
        },
        Desired {
            slot: Slot::LoopbackMemes,
            module: MODULE_LOOPBACK,
            args: format!("source={SOURCE_MEMES_MONITOR} sink={SINK_VIRTMIC}"),
        },
    ];
    if cfg.mic_mix {
        match real_mic(backend)? {
            Some(mic) => out.push(Desired {
                slot: Slot::LoopbackMic,
                module: MODULE_LOOPBACK,
                args: format!("source={mic} sink={SINK_VIRTMIC}"),
            }),
            None => warnings.push(
                "mic_mix is enabled but no real microphone found (default source is unset or a monitor); mic-mix loopback skipped".to_string(),
            ),
        }
    }
    if cfg.monitoring {
        match backend.default_sink()? {
            Some(sink) => out.push(Desired {
                slot: Slot::LoopbackMonitor,
                module: MODULE_LOOPBACK,
                args: format!("source={SOURCE_MEMES_MONITOR} sink={sink}"),
            }),
            None => warnings.push(
                "monitoring is enabled but no default sink found; monitoring loopback skipped"
                    .to_string(),
            ),
        }
    }
    Ok(out)
}

/// The real microphone: current default source, excluding any `*.monitor`.
fn real_mic(backend: &mut dyn PulseBackend) -> anyhow::Result<Option<String>> {
    match backend.default_source()? {
        Some(name) if name.ends_with(".monitor") => Ok(None),
        other => Ok(other),
    }
}

/// Backend abstraction over libpulse introspection. All methods are
/// synchronous; implementations must not busy-loop. All methods take
/// `&mut self` because pumping the libpulse mainloop is a mutating operation.
pub trait PulseBackend {
    /// Lists all loaded modules.
    fn modules(&mut self) -> anyhow::Result<Vec<ModuleInfo>>;
    /// Loads a module, returns its index.
    fn load(&mut self, name: &str, args: &str) -> anyhow::Result<u32>;
    /// Unloads a module by index.
    fn unload(&mut self, index: u32) -> anyhow::Result<()>;
    /// Name of the current default sink, if any.
    fn default_sink(&mut self) -> anyhow::Result<Option<String>>;
    /// Name of the current default source, if any.
    fn default_source(&mut self) -> anyhow::Result<Option<String>>;
}

/// Human-readable warning for a module shaped like ours that we do not own.
fn foreign_warning(m: &ModuleInfo) -> String {
    let kv = args_kv(&m.args);
    if m.name == MODULE_LOOPBACK {
        if kv.get("sink").map(String::as_str) == Some(SINK_VIRTMIC) {
            return format!("foreign loopback on {SINK_VIRTMIC} left alone: {}", m.args);
        }
        if kv.get("source").map(String::as_str) == Some(SOURCE_MEMES_MONITOR) {
            return format!(
                "foreign loopback from {SOURCE_MEMES_MONITOR} left alone: {}",
                m.args
            );
        }
    }
    format!("foreign {} left alone: {}", m.name, m.args)
}

/// Applies the desired state: loads missing modules, unloads+reloads the ones
/// of ours with wrong args, unloads stale/extra modules of ours. Modules not
/// owned per the state file are never touched: exact-args lookalikes are
/// adopted into the state, everything else is foreign and only warned about.
///
/// Returns the report and the updated state to persist (pruned of dead
/// indices, extended by adoptions/loads, shrunk by unloads).
pub fn reconcile_with(
    backend: &mut dyn PulseBackend,
    cfg: &Config,
    owned: &ModuleState,
) -> anyhow::Result<(Report, ModuleState)> {
    let mut warnings = Vec::new();
    let desired = desired_state(backend, cfg, &mut warnings)?;
    let current = backend.modules()?;

    let mut state = owned.clone();
    state.retain_live(&current.iter().map(|m| m.index).collect());

    let mut ours_by_slot: BTreeMap<Slot, Vec<ModuleInfo>> = BTreeMap::new();
    for m in current {
        let Some(slot) = classify(&m) else {
            continue; // not our shape at all: never ours, never warned about
        };
        if state.is_owned(&m) {
            ours_by_slot.entry(slot).or_default().push(m);
            continue;
        }
        // Shaped like ours but not in the state file: adoptable only with
        // EXACTLY our desired args for the slot; anything else is foreign.
        let adoptable = desired
            .iter()
            .find(|d| d.slot == slot)
            .is_some_and(|d| normalize_args(&m.args) == normalize_args(&d.args));
        if adoptable {
            state.adopt(&m);
            ours_by_slot.entry(slot).or_default().push(m);
        } else {
            warnings.push(foreign_warning(&m));
        }
    }

    let mut report = Report {
        changes: Vec::new(),
        warnings,
    };
    let mut consumed: HashSet<u32> = HashSet::new();

    for d in &desired {
        let existing = ours_by_slot.get(&d.slot).map(Vec::as_slice).unwrap_or(&[]);
        // Exactly one module per slot survives; a matching one is consumed,
        // any other module in the slot (wrong args or duplicate) falls
        // through to the stale cleanup below.
        let matched = existing
            .iter()
            .find(|m| normalize_args(&m.args) == normalize_args(&d.args))
            .map(|m| m.index);
        if let Some(index) = matched {
            consumed.insert(index);
            continue;
        }
        for m in existing {
            backend.unload(m.index)?;
            state.remove(m.index);
            consumed.insert(m.index);
        }
        let index = backend.load(d.module, &d.args)?;
        state.adopt(&ModuleInfo {
            index,
            name: d.module.to_string(),
            args: d.args.clone(),
        });
        let action = if existing.is_empty() {
            ChangeAction::Load
        } else {
            ChangeAction::Reload
        };
        report.changes.push(Change {
            action,
            slot: d.slot.label().to_string(),
            module: d.module.to_string(),
            args: d.args.clone(),
            index: Some(index),
        });
    }

    for modules in ours_by_slot.values() {
        for m in modules {
            if consumed.insert(m.index) {
                backend.unload(m.index)?;
                state.remove(m.index);
                report.changes.push(Change {
                    action: ChangeAction::Unload,
                    slot: classify(m)
                        .map_or_else(|| "<unknown>".to_string(), |s| s.label().to_string()),
                    module: m.name.clone(),
                    args: m.args.clone(),
                    index: None,
                });
            }
        }
    }

    Ok((report, state))
}

/// Unloads every module listed in the state file; everything else — foreign
/// modules and stateless lookalikes — survives (see the module docs for the
/// recovery path after state-file loss).
///
/// Returns the report and the updated state to persist.
pub fn teardown_with(
    backend: &mut dyn PulseBackend,
    owned: &ModuleState,
) -> anyhow::Result<(Report, ModuleState)> {
    let current = backend.modules()?;
    let mut state = owned.clone();
    state.retain_live(&current.iter().map(|m| m.index).collect());
    let mut report = Report::default();
    for m in &current {
        if state.is_owned(m) {
            backend.unload(m.index)?;
            state.remove(m.index);
            report.changes.push(Change {
                action: ChangeAction::Unload,
                slot: classify(m)
                    .map_or_else(|| "<unknown>".to_string(), |s| s.label().to_string()),
                module: m.name.clone(),
                args: m.args.clone(),
                index: None,
            });
        }
    }
    Ok((report, state))
}

/// One doctor check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Check name, human-readable.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// Extra context (what was found, or why it failed/was skipped).
    pub detail: String,
}

/// Result of [`doctor_with`]: a list of checks plus rendering helpers.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Checks, in evaluation order.
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// True when every check passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// Human-readable report: one line per check, then the Zoom hint.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = self
            .checks
            .iter()
            .map(|c| {
                let status = if c.passed { "PASS" } else { "FAIL" };
                let detail = if c.detail.is_empty() {
                    String::new()
                } else {
                    format!(" \u{2014} {}", c.detail)
                };
                format!("[{status}] {}{detail}", c.name)
            })
            .collect();
        lines.push(format!("Hint: {ZOOM_HINT}"));
        lines.push(if self.all_passed() {
            "all checks passed".to_string()
        } else {
            "PROBLEMS FOUND".to_string()
        });
        lines.join("\n")
    }
}

fn check(name: &str, passed: bool, detail: String) -> Check {
    Check {
        name: name.to_string(),
        passed,
        detail,
    }
}

/// Skipped filler for the checks that depend on a reachable server.
fn skipped(name: &str) -> Check {
    check(name, false, "skipped: pulse server unreachable".to_string())
}

/// Checks that exactly one slot entry exists and its args match the desired
/// string; renders a doctor check.
fn slot_check(
    name: &str,
    ours_by_slot: &BTreeMap<Slot, Vec<ModuleInfo>>,
    slot: Slot,
    desired_args: &str,
) -> Check {
    match ours_by_slot.get(&slot) {
        Some(found)
            if found
                .iter()
                .any(|m| normalize_args(&m.args) == normalize_args(desired_args)) =>
        {
            check(name, true, desired_args.to_string())
        }
        Some(found) => check(
            name,
            false,
            format!("present with unexpected args: {}", found[0].args),
        ),
        None => check(name, false, "not found".to_string()),
    }
}

/// Diagnoses the graph state; never fails — problems land in the report.
pub fn doctor_with(backend: &mut dyn PulseBackend, cfg: &Config) -> DoctorReport {
    const NAMES_SERVER: &str = "pulse server reachable";
    const NAMES_SINK_MEMES: &str = "sink 'soundboard' present";
    const NAMES_SINK_VIRTMIC: &str = "sink 'virtmic' present";
    const NAMES_LOOPBACK_MEMES: &str = "loopback soundboard.monitor->virtmic present";
    const NAMES_LOOPBACK_MIC: &str = "mic-mix loopback real-mic->virtmic present";
    const NAMES_LOOPBACK_MONITOR: &str =
        "monitoring loopback soundboard.monitor->real-sink present";
    const NAMES_REAL_MIC: &str = "real microphone found";

    let modules = match backend.modules() {
        Err(e) => {
            let mut checks = vec![check(NAMES_SERVER, false, format!("error: {e:#}"))];
            checks.push(skipped(NAMES_SINK_MEMES));
            checks.push(skipped(NAMES_SINK_VIRTMIC));
            checks.push(skipped(NAMES_LOOPBACK_MEMES));
            checks.push(skipped(NAMES_REAL_MIC));
            if cfg.mic_mix {
                checks.push(skipped(NAMES_LOOPBACK_MIC));
            }
            if cfg.monitoring {
                checks.push(skipped(NAMES_LOOPBACK_MONITOR));
            }
            return DoctorReport { checks };
        }
        Ok(modules) => modules,
    };

    let mut checks = vec![check(NAMES_SERVER, true, "server responded".to_string())];
    let mut ours_by_slot: BTreeMap<Slot, Vec<ModuleInfo>> = BTreeMap::new();
    for module in modules {
        if let Some(slot) = classify(&module) {
            ours_by_slot.entry(slot).or_default().push(module);
        }
    }

    checks.push(slot_check(
        NAMES_SINK_MEMES,
        &ours_by_slot,
        Slot::SinkMemes,
        &null_sink_args(SINK_MEMES, "Meme_Soundboard", None),
    ));
    checks.push(slot_check(
        NAMES_SINK_VIRTMIC,
        &ours_by_slot,
        Slot::SinkVirtmic,
        &null_sink_args(SINK_VIRTMIC, "Virtual_Mic_sink", Some("Virtual_Mic")),
    ));
    checks.push(slot_check(
        NAMES_LOOPBACK_MEMES,
        &ours_by_slot,
        Slot::LoopbackMemes,
        &format!("source={SOURCE_MEMES_MONITOR} sink={SINK_VIRTMIC}"),
    ));

    let real_mic_state = real_mic(backend);
    match &real_mic_state {
        Ok(Some(mic)) => checks.push(check(
            NAMES_REAL_MIC,
            true,
            format!("default source: {mic}"),
        )),
        Ok(None) => checks.push(check(
            NAMES_REAL_MIC,
            false,
            "default source is unset or a monitor".to_string(),
        )),
        Err(e) => checks.push(check(NAMES_REAL_MIC, false, format!("error: {e:#}"))),
    }

    if cfg.mic_mix {
        let found = match &real_mic_state {
            Ok(Some(_)) => ours_by_slot
                .get(&Slot::LoopbackMic)
                .and_then(|ms| ms.first()),
            _ => None,
        };
        checks.push(match found {
            Some(m) => check(NAMES_LOOPBACK_MIC, true, m.args.clone()),
            None if matches!(&real_mic_state, Ok(Some(_))) => {
                check(NAMES_LOOPBACK_MIC, false, "not found".to_string())
            }
            _ => skipped(NAMES_LOOPBACK_MIC),
        });
    }

    if cfg.monitoring {
        match backend.default_sink() {
            Ok(Some(sink)) => {
                let expected = format!("source={SOURCE_MEMES_MONITOR} sink={sink}");
                let found = ours_by_slot
                    .get(&Slot::LoopbackMonitor)
                    .into_iter()
                    .flatten()
                    .any(|m| normalize_args(&m.args) == normalize_args(&expected));
                checks.push(if found {
                    check(NAMES_LOOPBACK_MONITOR, true, expected)
                } else {
                    check(
                        NAMES_LOOPBACK_MONITOR,
                        false,
                        format!("not found (default sink: {sink})"),
                    )
                });
            }
            Ok(None) => checks.push(check(
                NAMES_LOOPBACK_MONITOR,
                false,
                "no default sink".to_string(),
            )),
            Err(e) => checks.push(check(
                NAMES_LOOPBACK_MONITOR,
                false,
                format!("error: {e:#}"),
            )),
        }
    }

    DoctorReport { checks }
}

/// Facade over a [`PulseBackend`] implementing the graph operations. Owns the
/// module state file: loads it before and persists it after every mutation.
pub struct Graph<B: PulseBackend> {
    backend: B,
    state_path: PathBuf,
}

impl<B: PulseBackend> Graph<B> {
    /// Wraps a backend, persisting module ownership at `state_path`
    /// (see [`default_state_path`]).
    #[must_use]
    pub fn new(backend: B, state_path: PathBuf) -> Self {
        Graph {
            backend,
            state_path,
        }
    }

    /// Idempotent desired-state reconcile (see [`reconcile_with`]); the
    /// module state file is reloaded first and rewritten after.
    ///
    /// # Errors
    /// Also fails when the state file cannot be persisted after a successful
    /// reconcile (the next `down` would not find our modules otherwise).
    pub fn reconcile(&mut self, cfg: &Config) -> anyhow::Result<Report> {
        let owned = ModuleState::load(&self.state_path);
        let (report, state) = reconcile_with(&mut self.backend, cfg, &owned)?;
        state.save(&self.state_path)?;
        Ok(report)
    }

    /// Removes all our modules (see [`teardown_with`]); persists the state.
    pub fn teardown(&mut self) -> anyhow::Result<Report> {
        let owned = ModuleState::load(&self.state_path);
        let (report, state) = teardown_with(&mut self.backend, &owned)?;
        state.save(&self.state_path)?;
        Ok(report)
    }

    /// Diagnostics (see [`doctor_with`]); read-only, never adopts or mutates
    /// the state.
    pub fn doctor(&mut self, cfg: &Config) -> DoctorReport {
        doctor_with(&mut self.backend, cfg)
    }
}

/// Synchronously waits for a libpulse operation to finish. Blocks in the
/// mainloop (no busy-looping); bails out if the server connection dies.
fn wait_op<C: ?Sized>(
    mainloop: &mut Mainloop,
    context: &Context,
    op: &Operation<C>,
) -> anyhow::Result<()> {
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => bail!("pulse mainloop quit unexpectedly"),
            IterateResult::Err(e) => bail!("pulse mainloop error: {e}"),
        }
        match context.get_state() {
            ContextState::Failed => bail!("pulse connection failed"),
            ContextState::Terminated => bail!("pulse connection terminated"),
            _ => {}
        }
        if op.get_state() != operation::State::Running {
            return Ok(());
        }
    }
}

fn take_result<T>(cell: &Rc<RefCell<Option<T>>>, what: &str) -> anyhow::Result<T> {
    cell.borrow_mut()
        .take()
        .ok_or_else(|| anyhow::anyhow!("pulse server gave no result for {what}"))
}

/// Real backend over the PulseAudio (PipeWire) pulse-compatible interface.
///
/// Field order matters: `context` is dropped before `mainloop`.
pub struct PulseSession {
    context: Context,
    mainloop: Mainloop,
}

impl PulseSession {
    /// Connects to the local pulse server, blocking until ready.
    pub fn connect(app_name: &str) -> anyhow::Result<Self> {
        let mut mainloop = Mainloop::new().context("failed to create pulse mainloop")?;
        let mut context =
            Context::new(&mainloop, app_name).context("failed to create pulse context")?;
        context
            .connect(None, FlagSet::NOFLAGS, None)
            .map_err(|e| anyhow::anyhow!("failed to initiate pulse connection: {e}"))?;
        loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) => bail!("pulse mainloop quit while connecting"),
                IterateResult::Err(e) => bail!("pulse mainloop error while connecting: {e}"),
            }
            match context.get_state() {
                ContextState::Ready => return Ok(PulseSession { context, mainloop }),
                ContextState::Failed | ContextState::Terminated => {
                    bail!("failed to connect to the pulse server (is pipewire-pulse running?)")
                }
                _ => {}
            }
        }
    }

    fn run_op<C: ?Sized>(&mut self, op: &Operation<C>) -> anyhow::Result<()> {
        wait_op(&mut self.mainloop, &self.context, op)
    }
}

impl PulseBackend for PulseSession {
    fn modules(&mut self) -> anyhow::Result<Vec<ModuleInfo>> {
        let introspect = self.context.introspect();
        let out: Rc<RefCell<Vec<ModuleInfo>>> = Rc::new(RefCell::new(Vec::new()));
        let failed: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let sink = out.clone();
        let fail_flag = failed.clone();
        let op =
            introspect.get_module_info_list(move |item: ListResult<&PulseModuleInfo>| match item {
                ListResult::Item(info) => sink.borrow_mut().push(ModuleInfo {
                    index: info.index,
                    name: info
                        .name
                        .as_ref()
                        .map_or_else(String::new, |c| c.to_string()),
                    args: info
                        .argument
                        .as_ref()
                        .map_or_else(String::new, |c| c.to_string()),
                }),
                ListResult::End => {}
                ListResult::Error => *fail_flag.borrow_mut() = true,
            });
        wait_op(&mut self.mainloop, &self.context, &op)?;
        if *failed.borrow() {
            bail!("pulse server failed to list modules");
        }
        Ok(std::mem::take(&mut *out.borrow_mut()))
    }

    fn load(&mut self, name: &str, args: &str) -> anyhow::Result<u32> {
        let mut introspect = self.context.introspect();
        let out: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));
        let sink = out.clone();
        let op = introspect.load_module(name, args, move |index: u32| {
            *sink.borrow_mut() = Some(index);
        });
        self.run_op(&op)?;
        match take_result(&out, "load_module")? {
            def::INVALID_INDEX => bail!("pulse server refused to load {name}"),
            index => Ok(index),
        }
    }

    fn unload(&mut self, index: u32) -> anyhow::Result<()> {
        let mut introspect = self.context.introspect();
        let out: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let sink = out.clone();
        let op = introspect.unload_module(index, move |success: bool| {
            *sink.borrow_mut() = Some(success);
        });
        self.run_op(&op)?;
        match take_result(&out, "unload_module")? {
            true => Ok(()),
            false => bail!("pulse server refused to unload module #{index}"),
        }
    }

    fn default_sink(&mut self) -> anyhow::Result<Option<String>> {
        self.server_default(|info: &ServerInfo| {
            info.default_sink_name.as_ref().map(|c| c.to_string())
        })
    }

    fn default_source(&mut self) -> anyhow::Result<Option<String>> {
        self.server_default(|info: &ServerInfo| {
            info.default_source_name.as_ref().map(|c| c.to_string())
        })
    }
}

impl PulseSession {
    fn server_default(
        &mut self,
        pick: impl Fn(&ServerInfo) -> Option<String> + 'static,
    ) -> anyhow::Result<Option<String>> {
        let introspect = self.context.introspect();
        let out: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let sink = out.clone();
        let op = introspect.get_server_info(move |info: &ServerInfo| {
            *sink.borrow_mut() = pick(info);
        });
        wait_op(&mut self.mainloop, &self.context, &op)?;
        Ok(out.borrow_mut().take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory backend for pure reconcile tests.
    #[derive(Debug, Default)]
    struct MockBackend {
        modules: Vec<ModuleInfo>,
        next_index: u32,
        default_sink: Option<String>,
        default_source: Option<String>,
        unreachable: bool,
    }

    impl MockBackend {
        fn new(default_sink: Option<&str>, default_source: Option<&str>) -> Self {
            MockBackend {
                default_sink: default_sink.map(str::to_string),
                default_source: default_source.map(str::to_string),
                ..MockBackend::default()
            }
        }

        fn with_module(mut self, name: &str, args: &str) -> Self {
            self.modules.push(ModuleInfo {
                index: self.next_index,
                name: name.to_string(),
                args: args.to_string(),
            });
            self.next_index += 1;
            self
        }
    }

    impl PulseBackend for MockBackend {
        fn modules(&mut self) -> anyhow::Result<Vec<ModuleInfo>> {
            if self.unreachable {
                bail!("connection refused");
            }
            Ok(self.modules.clone())
        }

        fn load(&mut self, name: &str, args: &str) -> anyhow::Result<u32> {
            if self.unreachable {
                bail!("connection refused");
            }
            let index = self.next_index;
            self.next_index += 1;
            self.modules.push(ModuleInfo {
                index,
                name: name.to_string(),
                args: args.to_string(),
            });
            Ok(index)
        }

        fn unload(&mut self, index: u32) -> anyhow::Result<()> {
            if self.unreachable {
                bail!("connection refused");
            }
            self.modules.retain(|m| m.index != index);
            Ok(())
        }

        fn default_sink(&mut self) -> anyhow::Result<Option<String>> {
            Ok(self.default_sink.clone())
        }

        fn default_source(&mut self) -> anyhow::Result<Option<String>> {
            Ok(self.default_source.clone())
        }
    }

    fn memes_sink_args() -> String {
        null_sink_args(SINK_MEMES, "Meme_Soundboard", None)
    }

    fn virtmic_sink_args() -> String {
        null_sink_args(SINK_VIRTMIC, "Virtual_Mic_sink", Some("Virtual_Mic"))
    }

    fn loopback_args(source: &str, sink: &str) -> String {
        format!("source={source} sink={sink}")
    }

    /// Both null-sinks plus the always-on memes loopback; passthroughs off.
    fn clean_sinks_backend() -> MockBackend {
        MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args())
            .with_module(MODULE_NULL_SINK, &virtmic_sink_args())
            .with_module(
                MODULE_LOOPBACK,
                &loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC),
            )
    }

    fn passthroughs_off() -> Config {
        Config {
            mic_mix: false,
            monitoring: false,
            ..Config::default()
        }
    }

    /// State marking every module currently in the backend as ours.
    fn seed_state(backend: &mut MockBackend) -> ModuleState {
        let modules = backend.modules().expect("modules");
        ModuleState {
            version: STATE_VERSION,
            modules: modules
                .into_iter()
                .map(|m| OwnedModule {
                    index: m.index,
                    name: m.name,
                })
                .collect(),
        }
    }

    #[test]
    fn clean_state_reconcile_is_noop() {
        let mut backend = clean_sinks_backend();
        let owned = seed_state(&mut backend);
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert!(report.is_noop(), "expected no changes, got: {report:?}");
        assert_eq!(backend.modules().expect("modules").len(), 3);
        assert_eq!(state, owned);
    }

    #[test]
    fn missing_loopback_is_loaded() {
        let mut backend = MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args())
            .with_module(MODULE_NULL_SINK, &virtmic_sink_args());
        let owned = seed_state(&mut backend);
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Load);
        assert_eq!(report.changes[0].slot, Slot::LoopbackMemes.label());
        let modules = backend.modules().expect("modules");
        let loaded = modules
            .iter()
            .find(|m| m.name == MODULE_LOOPBACK)
            .expect("loaded loopback");
        assert_eq!(
            normalize_args(&loaded.args),
            normalize_args(&loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC))
        );
        // The new module is recorded in the returned state.
        assert!(state.modules.iter().any(|o| o.index == loaded.index));
    }

    #[test]
    fn loopback_with_wrong_args_is_reloaded() {
        // Sinks only + a memes-slot loopback with extra args: the slot holds
        // exactly one wrong-args module of ours, so it must be unloaded and
        // reloaded.
        let mut backend = MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args())
            .with_module(MODULE_NULL_SINK, &virtmic_sink_args())
            .with_module(
                MODULE_LOOPBACK,
                &format!(
                    "{} latency_msec=100",
                    loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC)
                ),
            );
        let owned = seed_state(&mut backend);
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Reload);
        let modules = backend.modules().expect("modules");
        let loopbacks: Vec<_> = modules
            .iter()
            .filter(|m| m.name == MODULE_LOOPBACK)
            .collect();
        assert_eq!(loopbacks.len(), 1);
        assert_eq!(
            normalize_args(&loopbacks[0].args),
            normalize_args(&loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC))
        );
        // The reloaded module replaced the old record in the state.
        assert_eq!(state.modules.len(), 3);
        assert!(
            state
                .modules
                .iter()
                .any(|o| o.index == loopbacks[0].index && o.name == MODULE_LOOPBACK)
        );
    }

    #[test]
    fn mic_mix_loopback_follows_default_mic() {
        let mut backend = clean_sinks_backend();
        let owned = seed_state(&mut backend);
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let (report, _state) = reconcile_with(&mut backend, &cfg, &owned).expect("reconcile");
        assert!(report.warnings.is_empty());
        let modules = backend.modules().expect("modules");
        assert!(modules.iter().any(|m| m.name == MODULE_LOOPBACK
            && normalize_args(&m.args)
                == normalize_args(&loopback_args("alsa_input.real", SINK_VIRTMIC))));
    }

    #[test]
    fn mic_mix_with_monitor_default_source_warns_and_skips() {
        let mut backend = clean_sinks_backend();
        let owned = seed_state(&mut backend);
        backend.default_source = Some("soundboard.monitor".to_string());
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let (report, _state) = reconcile_with(&mut backend, &cfg, &owned).expect("reconcile");
        assert_eq!(report.warnings.len(), 1);
        // Nothing new was created: only the three always-on modules remain.
        assert_eq!(backend.modules().expect("modules").len(), 3);
    }

    #[test]
    fn monitoring_loopback_targets_default_sink() {
        let mut backend = clean_sinks_backend();
        let owned = seed_state(&mut backend);
        let cfg = Config {
            mic_mix: false,
            monitoring: true,
            ..Config::default()
        };
        let (_report, _state) = reconcile_with(&mut backend, &cfg, &owned).expect("reconcile");
        assert!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .any(|m| m.name == MODULE_LOOPBACK
                    && normalize_args(&m.args)
                        == normalize_args(&loopback_args(
                            SOURCE_MEMES_MONITOR,
                            "alsa_output.real"
                        )))
        );
    }

    #[test]
    fn stale_extra_ours_module_is_unloaded() {
        // clean_sinks_backend already carries the exact memes loopback; the
        // extra module below is a state-listed duplicate in the same slot.
        let mut backend = clean_sinks_backend().with_module(
            MODULE_LOOPBACK,
            &loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC),
        );
        let owned = seed_state(&mut backend);
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Unload);
        let loopbacks: Vec<_> = backend
            .modules()
            .expect("modules")
            .iter()
            .filter(|m| m.name == MODULE_LOOPBACK)
            .cloned()
            .collect();
        assert_eq!(loopbacks.len(), 1);
        // Exactly one loopback record remains in the state.
        assert_eq!(
            state
                .modules
                .iter()
                .filter(|o| o.name == MODULE_LOOPBACK)
                .count(),
            1
        );
    }

    #[test]
    fn disabled_mic_mix_unloads_stale_mic_loopback() {
        // The mic loopback IS in the state (we loaded it while mic_mix was
        // on), so turning mic_mix off unloads it.
        let mut backend = clean_sinks_backend().with_module(
            MODULE_LOOPBACK,
            &loopback_args("alsa_input.real", SINK_VIRTMIC),
        );
        let owned = seed_state(&mut backend);
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Unload);
        assert!(
            !backend
                .modules()
                .expect("modules")
                .iter()
                .any(|m| m.name == MODULE_LOOPBACK
                    && normalize_args(&m.args)
                        == normalize_args(&loopback_args("alsa_input.real", SINK_VIRTMIC)))
        );
        assert!(
            !state
                .modules
                .iter()
                .any(|o| o.name == MODULE_LOOPBACK && o.index == 3)
        );
    }

    #[test]
    fn foreign_loopback_is_untouched() {
        let foreign = loopback_args("alsa_input.real", "alsa_output.real");
        let mut backend = clean_sinks_backend().with_module(MODULE_LOOPBACK, &foreign);
        let owned = seed_state(&mut backend);
        let (report, _state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert!(report.is_noop());
        assert!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .any(|m| m.name == MODULE_LOOPBACK && m.args == foreign)
        );
    }

    #[test]
    fn foreign_lookalike_loopback_survives_up_and_down() {
        // A module-loopback on virtmic that we never loaded (absent from the
        // state) with DIFFERENT args: never adopted, never unloaded; `up`
        // warns, `down` leaves it alone.
        let lookalike = format!(
            "{} latency_msec=50",
            loopback_args("alsa_input.real", SINK_VIRTMIC)
        );
        let mut backend = clean_sinks_backend().with_module(MODULE_LOOPBACK, &lookalike);
        let ours = {
            // Seed only the three always-on modules; the lookalike (index 3)
            // stays unowned.
            let all = seed_state(&mut backend);
            ModuleState {
                version: STATE_VERSION,
                modules: all.modules.into_iter().take(3).collect(),
            }
        };
        // up
        let (up_report, up_state) =
            reconcile_with(&mut backend, &passthroughs_off(), &ours).expect("reconcile");
        assert!(
            up_report.warnings.iter().any(|w| {
                w.starts_with("foreign loopback on virtmic left alone") && w.contains(&lookalike)
            }),
            "expected a foreign-loopback warning, got: {up_report:?}"
        );
        assert!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .any(|m| m.args == lookalike),
            "foreign lookalike must survive up"
        );
        // The lookalike (index 3) was never adopted into the state.
        assert!(!up_state.modules.iter().any(|o| o.index == 3));
        // down
        let (down_report, down_state) = teardown_with(&mut backend, &up_state).expect("teardown");
        assert_eq!(down_report.changes.len(), 3);
        assert!(down_state.modules.is_empty());
        let rest = backend.modules().expect("modules");
        assert_eq!(rest.len(), 1);
        assert_eq!(
            rest[0].args, lookalike,
            "foreign lookalike must survive down"
        );
    }

    #[test]
    fn exact_args_loopback_after_state_loss_is_adopted_not_duplicated() {
        // Clean graph, empty state (state file lost): reconcile must adopt
        // all three exact-args modules into a fresh state, change nothing on
        // the server, and the adopted state must then drive teardown.
        let mut backend = clean_sinks_backend();
        let (report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &ModuleState::empty())
                .expect("reconcile");
        assert!(
            report.is_noop(),
            "adoption must be a no-op, got: {report:?}"
        );
        assert!(report.warnings.is_empty());
        assert_eq!(backend.modules().expect("modules").len(), 3);
        assert_eq!(state.modules.len(), 3);
        let (down_report, down_state) = teardown_with(&mut backend, &state).expect("teardown");
        assert_eq!(down_report.changes.len(), 3);
        assert!(down_state.modules.is_empty());
        assert!(backend.modules().expect("modules").is_empty());
    }

    #[test]
    fn stale_state_indices_are_pruned() {
        let mut backend = clean_sinks_backend();
        let mut owned = seed_state(&mut backend);
        // A dead record: the pulse server restarted, index 99 is gone.
        owned.modules.push(OwnedModule {
            index: 99,
            name: MODULE_LOOPBACK.to_string(),
        });
        let (_report, state) =
            reconcile_with(&mut backend, &passthroughs_off(), &owned).expect("reconcile");
        assert!(!state.modules.iter().any(|o| o.index == 99));
        assert_eq!(state.modules.len(), 3);
    }

    #[test]
    fn state_file_missing_corrupt_or_unknown_version_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Missing.
        assert_eq!(
            ModuleState::load(&dir.path().join("absent.json")),
            ModuleState::empty()
        );
        // Corrupt.
        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, "{not json").expect("write");
        assert_eq!(ModuleState::load(&corrupt), ModuleState::empty());
        // Wrong schema version.
        let old = dir.path().join("old.json");
        std::fs::write(&old, r#"{"version": 999, "modules": []}"#).expect("write");
        assert_eq!(ModuleState::load(&old), ModuleState::empty());
        // Valid file round-trips.
        let good = dir.path().join("nested").join("modules.json");
        let state = ModuleState {
            version: STATE_VERSION,
            modules: vec![OwnedModule {
                index: 7,
                name: MODULE_NULL_SINK.to_string(),
            }],
        };
        state.save(&good).expect("save creates parent dirs");
        assert_eq!(ModuleState::load(&good), state);
    }

    #[test]
    fn teardown_removes_exactly_ours() {
        let foreign = loopback_args("alsa_input.real", "alsa_output.real");
        let mut backend = clean_sinks_backend().with_module(MODULE_LOOPBACK, &foreign);
        // Seed only the three always-on modules as ours (that is what the
        // state records in reality); the foreign loopback (index 3) stays
        // unowned even though a buggy seed would list it.
        let owned = ModuleState {
            version: STATE_VERSION,
            modules: seed_state(&mut backend)
                .modules
                .into_iter()
                .take(3)
                .collect(),
        };
        let (report, state) = teardown_with(&mut backend, &owned).expect("teardown");
        assert_eq!(report.changes.len(), 3);
        assert!(state.modules.is_empty());
        let modules = backend.modules().expect("modules");
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].args, foreign);
    }

    #[test]
    fn classify_ignores_quoted_descriptions_and_foreign() {
        let ours = ModuleInfo {
            index: 1,
            name: MODULE_NULL_SINK.to_string(),
            args: "sink_name=virtmic media.class=Audio/Sink sink_properties=device.description=\"Virtual_Mic_sink\""
                .to_string(),
        };
        assert_eq!(classify(&ours), Some(Slot::SinkVirtmic));
        let foreign_loopback = ModuleInfo {
            index: 2,
            name: MODULE_LOOPBACK.to_string(),
            args: "source=x sink=soundboard".to_string(),
        };
        assert_eq!(classify(&foreign_loopback), None);
        let foreign_null = ModuleInfo {
            index: 3,
            name: MODULE_NULL_SINK.to_string(),
            args: "sink_name=other".to_string(),
        };
        assert_eq!(classify(&foreign_null), None);
    }

    #[test]
    fn doctor_all_pass_on_full_state() {
        let mut backend = clean_sinks_backend().with_module(
            MODULE_LOOPBACK,
            &loopback_args("alsa_input.real", SINK_VIRTMIC),
        );
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let report = doctor_with(&mut backend, &cfg);
        assert!(
            report.all_passed(),
            "expected all pass, got:\n{}",
            report.render()
        );
        assert_eq!(report.checks.len(), 6); // server, 2 sinks, loopback, real mic, mic-mix
        assert!(report.render().contains(ZOOM_HINT));
    }

    #[test]
    fn doctor_flags_missing_parts() {
        let mut backend = MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args());
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &cfg);
        assert!(!report.all_passed());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("virtmic") && !c.passed)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("loopback") && !c.passed)
        );
    }

    #[test]
    fn doctor_reports_unreachable_server() {
        let mut backend = MockBackend::new(None, None);
        backend.unreachable = true;
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &cfg);
        assert!(!report.all_passed());
        assert!(report.checks[0].name.contains("server"));
        assert!(!report.checks[0].passed);
        assert!(
            report.checks[1..]
                .iter()
                .all(|c| !c.passed && c.detail.contains("skipped"))
        );
    }

    #[test]
    fn real_mic_excludes_monitors() {
        let mut backend = MockBackend::new(None, Some("alsa_input.real"));
        assert_eq!(
            real_mic(&mut backend).expect("real_mic"),
            Some("alsa_input.real".to_string())
        );
        backend.default_source = Some("soundboard.monitor".to_string());
        assert_eq!(real_mic(&mut backend).expect("real_mic"), None);
        backend.default_source = None;
        assert_eq!(real_mic(&mut backend).expect("real_mic"), None);
    }

    #[test]
    fn graph_facade_delegates_and_persists_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state").join("modules.json");
        let mut graph = Graph::new(clean_sinks_backend(), state_path.clone());
        // State file absent: the first reconcile adopts the exact-args
        // modules and persists them.
        assert!(
            graph
                .reconcile(&passthroughs_off())
                .expect("reconcile")
                .is_noop()
        );
        assert!(state_path.exists(), "state file must be persisted");
        assert!(graph.doctor(&passthroughs_off()).all_passed());
        let torn = graph.teardown().expect("teardown");
        assert_eq!(torn.changes.len(), 3);
        // After teardown the persisted state is empty.
        assert_eq!(ModuleState::load(&state_path), ModuleState::empty());
    }
}
