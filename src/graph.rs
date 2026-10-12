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
//! "Our" modules are identified strictly by module name + arguments:
//! null-sinks with `sink_name=soundboard|virtmic`, and loopbacks with
//! `sink=virtmic` or `source=soundboard.monitor`. Anything else — including a
//! user's own loopback that happens to target `sink=soundboard` — is foreign
//! and never unloaded.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
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
/// Contract constant; consumed by the player (M2) and UI (M3).
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

/// Classifies a module into one of our slots; `None` for foreign modules.
/// Foreign modules must never be unloaded (user's own loopbacks survive).
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

/// Applies the desired state: loads missing modules, unloads+reloads the ones
/// with wrong args, unloads stale/extra modules of ours. Foreign modules are
/// never touched.
pub fn reconcile_with(backend: &mut dyn PulseBackend, cfg: &Config) -> anyhow::Result<Report> {
    let mut warnings = Vec::new();
    let desired = desired_state(backend, cfg, &mut warnings)?;
    let current = backend.modules()?;

    let mut ours_by_slot: BTreeMap<Slot, Vec<ModuleInfo>> = BTreeMap::new();
    for m in current {
        if let Some(slot) = classify(&m) {
            ours_by_slot.entry(slot).or_default().push(m);
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
            consumed.insert(m.index);
        }
        let index = backend.load(d.module, &d.args)?;
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

    Ok(report)
}

/// Unloads every module of ours; foreign modules survive.
pub fn teardown_with(backend: &mut dyn PulseBackend) -> anyhow::Result<Report> {
    let current = backend.modules()?;
    let mut report = Report::default();
    for m in current {
        if let Some(slot) = classify(&m) {
            backend.unload(m.index)?;
            report.changes.push(Change {
                action: ChangeAction::Unload,
                slot: slot.label().to_string(),
                module: m.name.clone(),
                args: m.args.clone(),
                index: None,
            });
        }
    }
    Ok(report)
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

/// Facade over a [`PulseBackend`] implementing the graph operations.
pub struct Graph<B: PulseBackend> {
    backend: B,
}

impl<B: PulseBackend> Graph<B> {
    /// Wraps a backend.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Graph { backend }
    }

    /// Idempotent desired-state reconcile (see [`reconcile_with`]).
    pub fn reconcile(&mut self, cfg: &Config) -> anyhow::Result<Report> {
        reconcile_with(&mut self.backend, cfg)
    }

    /// Removes all our modules (see [`teardown_with`]).
    pub fn teardown(&mut self) -> anyhow::Result<Report> {
        teardown_with(&mut self.backend)
    }

    /// Diagnostics (see [`doctor_with`]).
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

    #[test]
    fn clean_state_reconcile_is_noop() {
        let mut backend = clean_sinks_backend();
        let report = reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
        assert!(report.is_noop(), "expected no changes, got: {report:?}");
        assert_eq!(backend.modules().expect("modules").len(), 3);
    }

    #[test]
    fn missing_loopback_is_loaded() {
        let mut backend = MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args())
            .with_module(MODULE_NULL_SINK, &virtmic_sink_args());
        let report = reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Load);
        assert_eq!(report.changes[0].slot, Slot::LoopbackMemes.label());
        let modules = backend.modules().expect("modules");
        assert!(modules.iter().any(|m| m.name == MODULE_LOOPBACK
            && normalize_args(&m.args)
                == normalize_args(&loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC))));
    }

    #[test]
    fn loopback_with_wrong_args_is_reloaded() {
        // Sinks only + a memes-slot loopback with extra args: the slot holds
        // exactly one wrong-args module, so it must be unloaded and reloaded.
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
        let report = reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
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
    }

    #[test]
    fn mic_mix_loopback_follows_default_mic() {
        let mut backend = clean_sinks_backend();
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let report = reconcile_with(&mut backend, &cfg).expect("reconcile");
        assert!(report.warnings.is_empty());
        let modules = backend.modules().expect("modules");
        assert!(modules.iter().any(|m| m.name == MODULE_LOOPBACK
            && normalize_args(&m.args)
                == normalize_args(&loopback_args("alsa_input.real", SINK_VIRTMIC))));
    }

    #[test]
    fn mic_mix_with_monitor_default_source_warns_and_skips() {
        let mut backend = clean_sinks_backend();
        backend.default_source = Some("soundboard.monitor".to_string());
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let report = reconcile_with(&mut backend, &cfg).expect("reconcile");
        assert_eq!(report.warnings.len(), 1);
        // Nothing new was created: only the three always-on modules remain.
        assert_eq!(backend.modules().expect("modules").len(), 3);
    }

    #[test]
    fn monitoring_loopback_targets_default_sink() {
        let mut backend = clean_sinks_backend();
        let cfg = Config {
            mic_mix: false,
            monitoring: true,
            ..Config::default()
        };
        let report = reconcile_with(&mut backend, &cfg).expect("reconcile");
        assert!(report.warnings.is_empty());
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
        // extra module below is a duplicate in the same slot.
        let mut backend = clean_sinks_backend().with_module(
            MODULE_LOOPBACK,
            &loopback_args(SOURCE_MEMES_MONITOR, SINK_VIRTMIC),
        );
        let report = reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].action, ChangeAction::Unload);
        assert_eq!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .filter(|m| m.name == MODULE_LOOPBACK)
                .count(),
            1
        );
    }

    #[test]
    fn disabled_mic_mix_unloads_stale_mic_loopback() {
        let mut backend = clean_sinks_backend().with_module(
            MODULE_LOOPBACK,
            &loopback_args("alsa_input.real", SINK_VIRTMIC),
        );
        let report = reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
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
    }

    #[test]
    fn foreign_loopback_is_untouched() {
        let foreign = loopback_args("alsa_input.real", "alsa_output.real");
        let mut backend = clean_sinks_backend().with_module(MODULE_LOOPBACK, &foreign);
        reconcile_with(&mut backend, &passthroughs_off()).expect("reconcile");
        assert!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .any(|m| m.name == MODULE_LOOPBACK && m.args == foreign)
        );
    }

    #[test]
    fn teardown_removes_exactly_ours() {
        let foreign = loopback_args("alsa_input.real", "alsa_output.real");
        let mut backend = clean_sinks_backend().with_module(MODULE_LOOPBACK, &foreign);
        let report = teardown_with(&mut backend).expect("teardown");
        assert_eq!(report.changes.len(), 3);
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
    fn graph_facade_delegates() {
        let backend = clean_sinks_backend();
        let mut graph = Graph::new(backend);
        assert!(
            graph
                .reconcile(&passthroughs_off())
                .expect("reconcile")
                .is_noop()
        );
        assert!(graph.doctor(&passthroughs_off()).all_passed());
        let torn = graph.teardown().expect("teardown");
        assert_eq!(torn.changes.len(), 3);
    }
}
