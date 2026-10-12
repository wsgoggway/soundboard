//! Audio graph management: an idempotent desired-state reconcile over TWO
//! layers of the PipeWire session.
//!
//! The desired graph (contract: `docs/pipewire-recipe.md`, ADR 0004):
//!
//! Pulse layer (via `pipewire-pulse`, libpulse introspection):
//! - null-sink `soundboard` (description "Meme_Soundboard") — clips play here;
//! - loopback `soundboard.monitor -> <real_sink>` (only when
//!   `config.monitoring`).
//!
//! Native layer (via `pw-cli` / `pw-link` shell-outs):
//! - native virtual source node `virtmic`
//!   (`media.class=Audio/Source/Virtual`, description "Virtual_Mic") — THIS
//!   is what Zoom lists as a microphone (research-verified: Zoom filters
//!   monitor sources, so the old pulse null-sink monitor `virtmic.monitor`
//!   never showed up);
//! - links `soundboard:monitor_FL/FR -> virtmic:input_FL/FR` (always; the
//!   adapter's soft-mixer mixes multiple feeds into one input port);
//! - links `<real_mic>:capture_FL/FR -> virtmic:input_FL/FR` (only when
//!   `config.mic_mix`).
//!
//! `real_mic` is the current pulse default source, excluding any `*.monitor`;
//! its PipeWire node carries the same name (alsa sources keep
//! `node.name == pulse name`). If it is missing, the mic links are skipped
//! with a warning. `real_sink` is the current default sink. Both are resolved
//! at reconcile time, so `soundboard up` re-targets the graph when the user
//! changes default devices.
//!
//! NEVER used on purpose (documented broken variants, see the recipe): the
//! pulse-level `module-null-sink media.class=Audio/Source/Virtual` (registers
//! no pulse sink and loopbacks fall back to the default sink) and
//! `pw-loopback` (mis-binds both halves). The NATIVE `pw-cli create-node
//! adapter` path is the verified replacement.
//!
//! Ownership, pulse side, is persistent, not heuristic: every module index we
//! load is recorded in a versioned state file
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
//!   alone and listed in `Report.warnings` ("foreign loopback on
//!   soundboard.monitor left alone: ...").
//! - Teardown (down) unloads only state-listed modules; it does not adopt.
//!   Consequence: after losing the state file while the graph is up, `down`
//!   leaves the orphaned modules running — run `up` once (it adopts/reloads
//!   them back into the state), then `down`.
//!
//! Ownership, native side, is BY NAME, not by state: the node `virtmic` and
//! its port pairs are our identity (same as the pulse sink name). The state
//! file still records created node ids (`native_nodes`, schema version 2) for
//! bookkeeping and staleness pruning, but reconcile/teardown re-discover the
//! node by `node.name` and links by port pairs — recovery after state-file
//! loss needs no persisted native state. Links are matched stateless by
//! directed port pairs (out `node:port` -> in `node:port`) as reported by
//! `pw-link -l`: links carry no owner attribute, so the pair IS the identity.
//! A link is ours iff it touches the `virtmic` node (it is our node — every
//! link on it is ours); links between any other nodes are never touched.
//! If MULTIPLE `virtmic` nodes are found, extras are destroyed keeping the
//! lowest id (with a warning); there is a small race window where two `up`
//! invocations could both create a node — the next `up` reconciles the
//! duplicates away (documented limitation, ADR 0004).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use anyhow::{Context as _, bail};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::{
    ModuleInfo as PulseModuleInfo, ServerInfo, SourceInfo as PaSourceInfo,
};
use libpulse_binding::context::{Context, FlagSet, State as ContextState};
use libpulse_binding::def;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::operation::{self, Operation};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Sink the clips are played into.
pub const SINK_MEMES: &str = "soundboard";
/// Monitor source of [`SINK_MEMES`] (pulse alias of the PipeWire monitor
/// ports); feeds the virtual mic and the optional monitoring loopback.
pub const SOURCE_MEMES_MONITOR: &str = "soundboard.monitor";
/// Native PipeWire node name of the virtual microphone; the pulse layer
/// exposes the same node as capture source [`SOURCE_VIRTMIC`].
pub const NODE_VIRTMIC: &str = "virtmic";
/// Pulse source name apps capture from — the native virtual mic node itself
/// (NOT a `*.monitor`; that is the whole point of the native node, ADR 0004).
pub const SOURCE_VIRTMIC: &str = NODE_VIRTMIC;
/// `node.description` of the virtual mic node — what conferencing apps show
/// in their device list.
pub const VIRTMIC_DESCRIPTION: &str = "Virtual_Mic";
/// Output ports of the [`SINK_MEMES`] monitor (recipe-verified port names).
pub const MEMES_MONITOR_PORTS: [&str; 2] = ["monitor_FL", "monitor_FR"];
/// Input ports of the [`NODE_VIRTMIC`] node (recipe-verified port names).
pub const VIRTMIC_INPUT_PORTS: [&str; 2] = ["input_FL", "input_FR"];
/// Output (capture) ports of the real microphone node (recipe-verified port
/// names).
pub const MIC_CAPTURE_PORTS: [&str; 2] = ["capture_FL", "capture_FR"];
/// Human-readable hint appended to `doctor` output (Russian for the user;
/// node names stay as the server reports them).
pub const ZOOM_HINT: &str = "Zoom: Настройки \u{2192} Звук \u{2192} Микрофон = \"Virtual_Mic\"";

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

/// A pulse source as seen through introspection (subset used by doctor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulseSourceInfo {
    /// Source index in the server.
    pub index: u32,
    /// Source name, e.g. `virtmic` or `alsa_input...monitor`.
    pub name: String,
    /// Index of the monitored sink when this source is a monitor
    /// (`None` for a real source).
    pub monitor_of: Option<u32>,
}

/// Logical slot a pulse module of ours occupies; the unit of desired-state
/// matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    SinkMemes,
    LoopbackMonitor,
}

impl Slot {
    /// Human-readable label for reports.
    fn label(self) -> &'static str {
        match self {
            Slot::SinkMemes => "sink soundboard",
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
            _ => None,
        },
        MODULE_LOOPBACK => {
            let source = kv.get("source").map(String::as_str);
            if source == Some(SOURCE_MEMES_MONITOR) {
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

/// Schema version of the state file; bump on incompatible changes
/// (older/unknown versions are treated as empty state — a version-1 file from
/// the pre-native design is therefore dropped whole, and the next `up`
/// re-adopts whatever is still on the server).
const STATE_VERSION: u32 = 2;

/// One pulse module recorded as ours in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedModule {
    /// Module index in the server at load time.
    pub index: u32,
    /// Expected module name (guard against pid-style index reuse by an
    /// unrelated module): `module-null-sink` or `module-loopback`.
    pub name: String,
}

/// One native PipeWire node recorded as ours in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedNativeNode {
    /// Node id in the server at creation/adoption time.
    pub id: u32,
    /// Expected `node.name` (guard against id reuse): currently only
    /// [`NODE_VIRTMIC`].
    pub name: String,
}

/// Persisted registry of the pulse module indices and native node ids this
/// tool created (`$XDG_STATE_HOME/soundboard/modules.json`, see
/// [`default_state_path`]).
///
/// Written after every reconcile/teardown. A missing, corrupt or
/// unknown-version file is treated as empty state (the graph then re-adopts
/// exact-args modules / the name-matched native node on the next `up`; see
/// the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleState {
    /// Schema version, see [`STATE_VERSION`].
    pub version: u32,
    /// Pulse modules we loaded and have not unloaded yet.
    pub modules: Vec<OwnedModule>,
    /// Native nodes we created (bookkeeping; matching is by `node.name`,
    /// not by this list).
    #[serde(default)]
    pub native_nodes: Vec<OwnedNativeNode>,
}

impl ModuleState {
    /// Empty state (nothing owned yet).
    #[must_use]
    pub fn empty() -> Self {
        ModuleState {
            version: STATE_VERSION,
            modules: Vec::new(),
            native_nodes: Vec::new(),
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

    /// Records the native node as ours (no-op when present).
    fn adopt_native(&mut self, id: u32, name: &str) {
        if !self.native_nodes.iter().any(|n| n.id == id) {
            self.native_nodes.push(OwnedNativeNode {
                id,
                name: name.to_string(),
            });
        }
    }

    /// Drops the record for a destroyed native node.
    fn remove_native(&mut self, id: u32) {
        self.native_nodes.retain(|n| n.id != id);
    }

    /// Prunes native records whose node id no longer exists.
    fn retain_native_live(&mut self, live_ids: &HashSet<u32>) {
        self.native_nodes.retain(|n| live_ids.contains(&n.id));
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
            std::fs::create_dir_all(parent).with_context(|| {
                format!("не удалось создать каталог состояния {}", parent.display())
            })?;
        }
        let text =
            serde_json::to_string_pretty(self).context("failed to serialize module state")?;
        std::fs::write(path, text)
            .with_context(|| format!("не удалось записать {}", path.display()))
    }
}

/// Default state file location:
/// `$XDG_STATE_HOME/soundboard/modules.json` (typically
/// `~/.local/state/soundboard/modules.json`).
///
/// # Errors
/// Fails when the XDG state directory cannot be determined.
pub fn default_state_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::state_dir().ok_or_else(|| {
        anyhow::anyhow!(
            "не удалось определить каталог состояния XDG (задайте XDG_STATE_HOME или HOME)"
        )
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
    /// Module name or tool (`pw-cli` / `pw-link`) that applied the change.
    pub module: String,
    /// Module arguments (target args for `Load`/`Reload`), the SPA-JSON
    /// payload (node create) or the port pair (link create/destroy).
    pub args: String,
    /// Module/node index after the change (`None` for unloads/destroys and
    /// for links).
    pub index: Option<u32>,
}

/// Kind of applied change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    /// Pulse module was loaded.
    Load,
    /// Pulse module was unloaded.
    Unload,
    /// Pulse module had wrong args: unloaded, then loaded again.
    Reload,
    /// Native node or link was created.
    Create,
    /// Native node or link was destroyed.
    Destroy,
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

    /// Human-readable multi-line summary ("изменений нет" when noop).
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_noop() {
            return "изменений нет".to_string();
        }
        let mut lines = Vec::new();
        for c in &self.changes {
            let verb = match c.action {
                ChangeAction::Load => "загружен",
                ChangeAction::Unload => "выгружен",
                ChangeAction::Reload => "перезагружен",
                ChangeAction::Create => "создан",
                ChangeAction::Destroy => "удалён",
            };
            let index = c
                .index
                .map_or_else(String::new, |i| format!(" (индекс {i})"));
            lines.push(format!("{verb} {} [{}]{}", c.module, c.slot, index));
        }
        for w in &self.warnings {
            lines.push(format!("предупреждение: {w}"));
        }
        lines.join("\n")
    }
}

/// A pulse module we want present, with the exact args it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Desired {
    slot: Slot,
    module: &'static str,
    args: String,
}

fn null_sink_args(sink_name: &str, sink_description: &str) -> String {
    format!(
        "sink_name={sink_name} media.class=Audio/Sink sink_properties=device.description={sink_description}"
    )
}

/// Builds the desired pulse module list for the current config and session
/// state.
fn desired_pulse_modules(
    backend: &mut dyn PulseBackend,
    cfg: &Config,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Desired>> {
    let mut out = vec![Desired {
        slot: Slot::SinkMemes,
        module: MODULE_NULL_SINK,
        args: null_sink_args(SINK_MEMES, "Meme_Soundboard"),
    }];
    if cfg.monitoring {
        match backend.default_sink()? {
            Some(sink) => out.push(Desired {
                slot: Slot::LoopbackMonitor,
                module: MODULE_LOOPBACK,
                args: format!("source={SOURCE_MEMES_MONITOR} sink={sink}"),
            }),
            None => warnings.push(
                "monitoring включён, но синк по умолчанию не найден; monitoring loopback пропущен"
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
    /// Lists all sources (used by the Zoom-visibility doctor check).
    fn sources(&mut self) -> anyhow::Result<Vec<PulseSourceInfo>>;
    /// Loads a module, returns its index.
    fn load(&mut self, name: &str, args: &str) -> anyhow::Result<u32>;
    /// Unloads a module by index.
    fn unload(&mut self, index: u32) -> anyhow::Result<()>;
    /// Name of the current default sink, if any.
    fn default_sink(&mut self) -> anyhow::Result<Option<String>>;
    /// Name of the current default source, if any.
    fn default_source(&mut self) -> anyhow::Result<Option<String>>;
}

/// One PipeWire object as reported by `pw-cli ls Node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeNodeInfo {
    /// Node id in the server.
    pub id: u32,
    /// `node.name` property, when present.
    pub name: Option<String>,
}

/// One PipeWire port as reported by `pw-cli ls Port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePortInfo {
    /// Port id in the server.
    pub id: u32,
    /// Id of the owning node (`node.id`).
    pub node_id: u32,
    /// Port name (`port.name`), e.g. `input_FL`.
    pub name: String,
    /// Port direction (`port.direction`): `in` or `out`.
    pub direction: String,
}

/// One directed link `out_node:out_port -> in_node:in_port` as reported by
/// `pw-link -l` (name-based; nodes without a name are reported by id string).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NativeLinkInfo {
    /// Output (source) node name or id string.
    pub out_node: String,
    /// Output port name.
    pub out_port: String,
    /// Input (sink) node name or id string.
    pub in_node: String,
    /// Input port name.
    pub in_port: String,
}

impl NativeLinkInfo {
    /// Builds a link info from `node:port` reference strings.
    fn new(out_node: &str, out_port: &str, in_node: &str, in_port: &str) -> Self {
        NativeLinkInfo {
            out_node: out_node.to_string(),
            out_port: out_port.to_string(),
            in_node: in_node.to_string(),
            in_port: in_port.to_string(),
        }
    }

    /// True when the link touches `node` on either end — such links belong to
    /// our node and are the only ones we ever destroy.
    fn touches(&self, node: &str) -> bool {
        self.out_node == node || self.in_node == node
    }
}

/// Backend abstraction over the native PipeWire tooling (`pw-cli` /
/// `pw-link`). All methods are synchronous process spawns.
pub trait NativeBackend {
    /// Lists all nodes (`pw-cli ls Node`).
    fn nodes(&mut self) -> anyhow::Result<Vec<NativeNodeInfo>>;
    /// Lists all ports (`pw-cli ls Port`).
    fn ports(&mut self) -> anyhow::Result<Vec<NativePortInfo>>;
    /// Lists all directed links (`pw-link -l`).
    fn links(&mut self) -> anyhow::Result<Vec<NativeLinkInfo>>;
    /// Creates a node (`pw-cli create-node ...`); on success pw-cli prints
    /// nothing — the caller re-lists nodes to find the new id.
    fn create_node(&mut self, args: &[String]) -> anyhow::Result<()>;
    /// Destroys a node by id (`pw-cli destroy <id>`).
    fn destroy_node(&mut self, id: u32) -> anyhow::Result<()>;
    /// Links two ports (`pw-link out:port in:port`).
    fn create_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()>;
    /// Unlinks two ports (`pw-link -d out:port in:port`).
    fn destroy_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()>;
}

/// The exact `pw-cli create-node` argument vector for the virtual mic node —
/// the live-verified recipe, built by [`virtmic_node_payload`].
fn virtmic_node_args() -> Vec<String> {
    vec![
        "create-node".to_string(),
        "adapter".to_string(),
        virtmic_node_payload(),
    ]
}

/// The SPA-JSON properties payload for the virtual mic node (single argv
/// token, braced form as verified live). Syntax traps baked in:
/// NO quotes around values; `audio.position` MUST be the array
/// `[ FL, FR ]` (a bare comma breaks SPA-JSON); `monitor.channel-volumes`
/// must NOT be set (it corrupts the description/channel-map).
#[must_use]
pub fn virtmic_node_payload() -> String {
    format!(
        "{{ factory.name=support.null-audio-sink node.name={NODE_VIRTMIC} \
         media.class=Audio/Source/Virtual node.description={VIRTMIC_DESCRIPTION} \
         audio.position=[ FL, FR ] object.linger=true }}"
    )
}

/// Runs a command, captures stdout, fails with stderr context on non-zero
/// exit. argv arrays only — never a shell string.
fn run_capture(bin: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("не удалось запустить {}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {} завершился с ошибкой: {}",
            bin.display(),
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs a command and checks the exit status; output is ignored (pw-cli
/// create-node prints nothing on success — silence is success, no retry).
fn run_quiet(bin: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("не удалось запустить {}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {} завершился с ошибкой: {}",
            bin.display(),
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(())
}

/// Real backend over the `pw-cli` / `pw-link` binaries. Binary paths are
/// injectable for tests ([`PwTooling::with_paths`]); the system constructor
/// resolves them from `PATH`.
pub struct PwTooling {
    pw_cli: PathBuf,
    pw_link: PathBuf,
}

impl PwTooling {
    /// Tooling resolving `pw-cli` / `pw-link` from `PATH`.
    #[must_use]
    pub fn system() -> Self {
        PwTooling {
            pw_cli: PathBuf::from("pw-cli"),
            pw_link: PathBuf::from("pw-link"),
        }
    }

    /// Tooling with explicit binary paths (tests, non-PATH installs).
    // Not used by the bin target yet; kept injectable per the testing rules
    // (unit tests stub the binaries via these paths).
    #[allow(dead_code)]
    pub fn with_paths(pw_cli: PathBuf, pw_link: PathBuf) -> Self {
        PwTooling { pw_cli, pw_link }
    }
}

impl NativeBackend for PwTooling {
    fn nodes(&mut self) -> anyhow::Result<Vec<NativeNodeInfo>> {
        let text = run_capture(&self.pw_cli, &["ls", "Node"])?;
        Ok(parse_nodes(&text))
    }

    fn ports(&mut self) -> anyhow::Result<Vec<NativePortInfo>> {
        let text = run_capture(&self.pw_cli, &["ls", "Port"])?;
        Ok(parse_ports(&text))
    }

    fn links(&mut self) -> anyhow::Result<Vec<NativeLinkInfo>> {
        let text = run_capture(&self.pw_link, &["-l"])?;
        Ok(parse_pwlink_list(&text))
    }

    fn create_node(&mut self, args: &[String]) -> anyhow::Result<()> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_quiet(&self.pw_cli, &refs)
    }

    fn destroy_node(&mut self, id: u32) -> anyhow::Result<()> {
        run_quiet(&self.pw_cli, &["destroy", &id.to_string()])
    }

    fn create_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()> {
        run_quiet(
            &self.pw_link,
            &[
                &format!("{}:{}", out.0, out.1),
                &format!("{}:{}", input.0, input.1),
            ],
        )
    }

    fn destroy_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()> {
        run_quiet(
            &self.pw_link,
            &[
                "-d",
                &format!("{}:{}", out.0, out.1),
                &format!("{}:{}", input.0, input.1),
            ],
        )
    }
}

/// Splits a `node:port` reference at the first colon (PipeWire node names
/// never contain colons; ids are digits).
fn parse_port_spec(spec: &str) -> Option<(String, String)> {
    let (node, port) = spec.split_once(':')?;
    if node.is_empty() || port.is_empty() {
        return None;
    }
    Some((node.to_string(), port.to_string()))
}

/// Parses `pw-cli ls`-style output into (id, properties) records. Object
/// headers look like `\tid 62, type PipeWire:Interface:Node/3`; property
/// lines look like `key = "value"` (quotes stripped when present). Content
/// before the first header is ignored.
fn parse_pw_objects(text: &str) -> Vec<(u32, BTreeMap<String, String>)> {
    let mut out: Vec<(u32, BTreeMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("id ") {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(id) = digits.parse::<u32>() {
                out.push((id, BTreeMap::new()));
                continue;
            }
        }
        let Some((key, value)) = trimmed.split_once(" = ") else {
            continue;
        };
        if let Some((_, props)) = out.last_mut() {
            let value = value.trim().trim_matches('"').to_string();
            props.insert(key.trim().to_string(), value);
        }
    }
    out
}

/// Parses `pw-cli ls Node` output into node records.
#[must_use]
pub fn parse_nodes(text: &str) -> Vec<NativeNodeInfo> {
    parse_pw_objects(text)
        .into_iter()
        .map(|(id, props)| NativeNodeInfo {
            id,
            name: props.get("node.name").cloned(),
        })
        .collect()
}

/// Parses `pw-cli ls Port` output into port records; entries without a
/// usable `node.id` or `port.name` are skipped.
#[must_use]
pub fn parse_ports(text: &str) -> Vec<NativePortInfo> {
    parse_pw_objects(text)
        .into_iter()
        .filter_map(|(id, props)| {
            let node_id = props.get("node.id")?.parse::<u32>().ok()?;
            let name = props.get("port.name")?.clone();
            let direction = props.get("port.direction").cloned().unwrap_or_default();
            Some(NativePortInfo {
                id,
                node_id,
                name,
                direction,
            })
        })
        .collect()
}

/// Parses `pw-link -l` output. The listing is grouped by port: a top-level
/// line is a port reference, arrow lines are its peers — `|-> peer` means the
/// group port is the OUTPUT side, `|<- peer` means it is the INPUT side. The
/// same physical link is listed under both endpoints, so callers must treat
/// the result as a multiset (reconcile deduplicates by pair).
#[must_use]
pub fn parse_pwlink_list(text: &str) -> Vec<NativeLinkInfo> {
    let mut out = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(peer) = trimmed.strip_prefix("|->")
            && let Some((out_node, out_port)) = current.clone()
            && let Some((in_node, in_port)) = parse_port_spec(peer.trim())
        {
            out.push(NativeLinkInfo::new(
                &out_node, &out_port, &in_node, &in_port,
            ));
        } else if let Some(peer) = trimmed.strip_prefix("|<-")
            && let Some((in_node, in_port)) = current.clone()
            && let Some((out_node, out_port)) = parse_port_spec(peer.trim())
        {
            out.push(NativeLinkInfo::new(
                &out_node, &out_port, &in_node, &in_port,
            ));
        } else if !trimmed.is_empty() {
            current = parse_port_spec(trimmed);
        }
    }
    out
}

/// Directed port pair our feeds are identified by.
type PortPair = (String, String, String, String);

/// Desired link pairs for the current config: soundboard monitor -> virtmic
/// (always) plus real mic -> virtmic when `mic` is `Some` (mic_mix on and a
/// real mic was found).
fn desired_link_pairs(mic: Option<&str>) -> Vec<PortPair> {
    let mut out = Vec::new();
    for (out_port, in_port) in MEMES_MONITOR_PORTS.iter().zip(VIRTMIC_INPUT_PORTS) {
        out.push((
            SINK_MEMES.to_string(),
            (*out_port).to_string(),
            NODE_VIRTMIC.to_string(),
            in_port.to_string(),
        ));
    }
    if let Some(mic) = mic {
        for (out_port, in_port) in MIC_CAPTURE_PORTS.iter().zip(VIRTMIC_INPUT_PORTS) {
            out.push((
                mic.to_string(),
                (*out_port).to_string(),
                NODE_VIRTMIC.to_string(),
                in_port.to_string(),
            ));
        }
    }
    out
}

/// Human-readable slot label for a link pair.
fn link_slot(pair: &PortPair) -> String {
    format!("связь {}:{}->{}:{}", pair.0, pair.1, pair.2, pair.3)
}

/// One desired link change applied against the server.
#[derive(Debug)]
struct LinkChange {
    pair: PortPair,
    destroy: bool,
}

/// Computes the link diff between desired pairs and the links the server
/// currently reports: missing desired pairs must be created, our pairs that
/// must not exist (any link touching the virtmic node) must be destroyed.
/// Foreign links (neither end on virtmic) never appear here.
fn link_diff(desired: &[PortPair], existing: &[NativeLinkInfo]) -> Vec<LinkChange> {
    let desired_set: HashSet<&PortPair> = desired.iter().collect();
    let mut existing_unique: Vec<&NativeLinkInfo> = Vec::new();
    let mut seen: HashSet<&NativeLinkInfo> = HashSet::new();
    for link in existing {
        if seen.insert(link) {
            existing_unique.push(link);
        }
    }
    let mut out = Vec::new();
    for pair in desired {
        let as_info = NativeLinkInfo {
            out_node: pair.0.clone(),
            out_port: pair.1.clone(),
            in_node: pair.2.clone(),
            in_port: pair.3.clone(),
        };
        if !existing_unique.contains(&&as_info) {
            out.push(LinkChange {
                pair: pair.clone(),
                destroy: false,
            });
        }
    }
    for link in existing_unique {
        let pair = (
            link.out_node.clone(),
            link.out_port.clone(),
            link.in_node.clone(),
            link.in_port.clone(),
        );
        if link.touches(NODE_VIRTMIC) && !desired_set.contains(&pair) {
            out.push(LinkChange {
                pair,
                destroy: true,
            });
        }
    }
    out
}

/// Discovers the `virtmic` node by `node.name`; creates it when absent,
/// destroys duplicates keeping the LOWEST id. Returns the kept id and a
/// FRESH node list (including a newly created node).
fn ensure_virtmic_node(
    native: &mut dyn NativeBackend,
    report: &mut Report,
) -> anyhow::Result<(u32, Vec<NativeNodeInfo>)> {
    let nodes = native.nodes()?;
    let mut ids: Vec<u32> = nodes
        .iter()
        .filter(|n| n.name.as_deref() == Some(NODE_VIRTMIC))
        .map(|n| n.id)
        .collect();
    ids.sort_unstable();
    if ids.is_empty() {
        native.create_node(&virtmic_node_args())?;
        let nodes = native.nodes()?;
        let created: Vec<u32> = nodes
            .iter()
            .filter(|n| n.name.as_deref() == Some(NODE_VIRTMIC))
            .map(|n| n.id)
            .collect();
        let Some(&id) = created.iter().min() else {
            bail!("pw-cli create-node отработал, но узел {NODE_VIRTMIC} не появился");
        };
        report.changes.push(Change {
            action: ChangeAction::Create,
            slot: format!("узел {NODE_VIRTMIC}"),
            module: "pw-cli adapter".to_string(),
            args: virtmic_node_payload(),
            index: Some(id),
        });
        return Ok((id, nodes));
    }
    let keep = ids[0];
    let mut destroyed_extras = Vec::new();
    for &extra in &ids[1..] {
        native.destroy_node(extra)?;
        destroyed_extras.push(extra);
        report.changes.push(Change {
            action: ChangeAction::Destroy,
            slot: format!("узел {NODE_VIRTMIC} (дубликат)"),
            module: "pw-cli".to_string(),
            args: format!("id {extra}"),
            index: None,
        });
    }
    if !destroyed_extras.is_empty() {
        let list: Vec<String> = destroyed_extras.iter().map(|i| i.to_string()).collect();
        report.warnings.push(format!(
            "обнаружено несколько узлов {NODE_VIRTMIC} — дубликаты (id {}) удалены, оставлен id {keep}",
            list.join(", ")
        ));
    }
    Ok((keep, nodes))
}

/// Applies the native part of the desired state: virtmic node + feed links.
/// `mic` is the resolved real-mic name (pulse default source) when mic_mix is
/// on and a mic exists. Never touches links whose ports are not on our nodes.
fn native_reconcile(
    native: &mut dyn NativeBackend,
    mic: Option<&str>,
    state: &mut ModuleState,
    report: &mut Report,
) -> anyhow::Result<()> {
    let (virtmic_id, nodes) = ensure_virtmic_node(native, report)?;
    let live_ids: HashSet<u32> = nodes.iter().map(|n| n.id).collect();
    state.retain_native_live(&live_ids);
    state.adopt_native(virtmic_id, NODE_VIRTMIC);

    // Desired pairs; skipped (with a single warning each) when the soundboard
    // sink or the mic node is missing — the port check below is the second
    // guard (a present node with unexpected port names).
    let node_id = |name: &str| -> Option<u32> {
        nodes
            .iter()
            .find(|n| n.name.as_deref() == Some(name))
            .map(|n| n.id)
    };
    let mut desired = match node_id(SINK_MEMES) {
        Some(_) => desired_link_pairs(match mic {
            Some(name) if node_id(name).is_some() => Some(name),
            Some(name) => {
                report.warnings.push(format!(
                    "узел микрофона {name} не найден среди PipeWire-узлов — mic-связи пропущены"
                ));
                None
            }
            None => None,
        }),
        None => {
            report
                .warnings
                .push("синк soundboard не найден — связи звука не созданы".to_string());
            Vec::new()
        }
    };

    // Only create pairs whose ports actually exist (port names are recipe
    // facts; a deviating node must warn, not produce broken pw-link calls).
    let ports = native.ports()?;
    let mut ports_by_node: BTreeMap<u32, HashSet<String>> = BTreeMap::new();
    for p in ports {
        ports_by_node.entry(p.node_id).or_default().insert(p.name);
    }
    let port_of = |node: &str, port: &str| -> bool {
        node_id(node).is_some_and(|id| ports_by_node.get(&id).is_some_and(|set| set.contains(port)))
    };
    desired.retain(|pair| {
        let ok = port_of(&pair.0, &pair.1) && port_of(&pair.2, &pair.3);
        if !ok {
            report.warnings.push(format!(
                "порты для связи {}:{}->{}:{} не найдены — связь пропущена",
                pair.0, pair.1, pair.2, pair.3
            ));
        }
        ok
    });

    let existing = native.links()?;
    for change in link_diff(&desired, &existing) {
        if change.destroy {
            native.destroy_link(
                (&change.pair.0, &change.pair.1),
                (&change.pair.2, &change.pair.3),
            )?;
        } else {
            native.create_link(
                (&change.pair.0, &change.pair.1),
                (&change.pair.2, &change.pair.3),
            )?;
        }
        report.changes.push(Change {
            action: if change.destroy {
                ChangeAction::Destroy
            } else {
                ChangeAction::Create
            },
            slot: link_slot(&change.pair),
            module: "pw-link".to_string(),
            args: format!(
                "{}:{} -> {}:{}",
                change.pair.0, change.pair.1, change.pair.2, change.pair.3
            ),
            index: None,
        });
    }
    Ok(())
}

/// Human-readable warning for a module shaped like ours that we do not own.
fn foreign_warning(m: &ModuleInfo) -> String {
    let kv = args_kv(&m.args);
    if m.name == MODULE_LOOPBACK
        && kv.get("source").map(String::as_str) == Some(SOURCE_MEMES_MONITOR)
    {
        return format!(
            "чужой loopback от {SOURCE_MEMES_MONITOR} оставлен без изменений: {}",
            m.args
        );
    }
    format!("чужой {} оставлен без изменений: {}", m.name, m.args)
}

/// Applies the desired state on both layers: pulse modules (loads missing,
/// unloads+reloads the ones of ours with wrong args, unloads stale/extra
/// modules of ours) and the native node + links. Pulse modules not owned per
/// the state file are never touched: exact-args lookalikes are adopted into
/// the state, everything else is foreign and only warned about. The native
/// side matches by node name / port pairs and never touches foreign links.
///
/// Returns the report and the updated state to persist (pruned of dead
/// records, extended by adoptions/loads/creates, shrunk by unloads/destroys).
pub fn reconcile_with(
    backend: &mut dyn PulseBackend,
    native: &mut dyn NativeBackend,
    cfg: &Config,
    owned: &ModuleState,
) -> anyhow::Result<(Report, ModuleState)> {
    let mut warnings = Vec::new();
    let desired = desired_pulse_modules(backend, cfg, &mut warnings)?;
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

    // Native layer runs after the pulse layer: the soundboard sink must
    // exist before links onto its monitor ports are created.
    let mic = if cfg.mic_mix {
        match real_mic(backend)? {
            Some(mic) => Some(mic),
            None => {
                report.warnings.push(
                    "mic_mix включён, но реальный микрофон не найден (источник по умолчанию не задан или монитор); mic-связи пропущены"
                        .to_string(),
                );
                None
            }
        }
    } else {
        None
    };
    native_reconcile(native, mic.as_deref(), &mut state, &mut report)?;

    Ok((report, state))
}

/// Removes everything of ours from both layers: every pulse module listed in
/// the state file (foreign modules and stateless lookalikes survive), every
/// link touching the virtmic node and the virtmic node itself (matched by
/// name, not by state — recovery after state-file loss works without the
/// persisted native records).
///
/// Returns the report and the updated state to persist.
pub fn teardown_with(
    backend: &mut dyn PulseBackend,
    native: &mut dyn NativeBackend,
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

    let nodes = native.nodes()?;
    let live_ids: HashSet<u32> = nodes.iter().map(|n| n.id).collect();
    state.retain_native_live(&live_ids);
    let mut virtmic_ids: Vec<u32> = nodes
        .iter()
        .filter(|n| n.name.as_deref() == Some(NODE_VIRTMIC))
        .map(|n| n.id)
        .collect();
    virtmic_ids.sort_unstable();
    if !virtmic_ids.is_empty() {
        // Links first, then the node: a dangling link on a dying node would
        // just be cleaned up by the server anyway, but an explicit order
        // keeps the report honest.
        let links = native.links()?;
        let mut seen: HashSet<&NativeLinkInfo> = HashSet::new();
        for link in &links {
            if !seen.insert(link) || !link.touches(NODE_VIRTMIC) {
                continue;
            }
            native.destroy_link(
                (&link.out_node, &link.out_port),
                (&link.in_node, &link.in_port),
            )?;
            let pair = (
                link.out_node.clone(),
                link.out_port.clone(),
                link.in_node.clone(),
                link.in_port.clone(),
            );
            report.changes.push(Change {
                action: ChangeAction::Destroy,
                slot: link_slot(&pair),
                module: "pw-link".to_string(),
                args: format!(
                    "{}:{} -> {}:{}",
                    link.out_node, link.out_port, link.in_node, link.in_port
                ),
                index: None,
            });
        }
        for id in virtmic_ids {
            native.destroy_node(id)?;
            state.remove_native(id);
            report.changes.push(Change {
                action: ChangeAction::Destroy,
                slot: format!("узел {NODE_VIRTMIC}"),
                module: "pw-cli".to_string(),
                args: format!("id {id}"),
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
                let status = if c.passed { "УСПЕХ" } else { "СБОЙ" };
                let detail = if c.detail.is_empty() {
                    String::new()
                } else {
                    format!(" \u{2014} {}", c.detail)
                };
                format!("[{status}] {}{detail}", c.name)
            })
            .collect();
        lines.push(format!("Подсказка: {ZOOM_HINT}"));
        lines.push(if self.all_passed() {
            "все проверки пройдены".to_string()
        } else {
            "ОБНАРУЖЕНЫ ПРОБЛЕМЫ".to_string()
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

/// Skipped filler for the checks that depend on a reachable pulse server.
fn skipped(name: &str) -> Check {
    check(
        name,
        false,
        "пропущено: pulse-сервер недоступен".to_string(),
    )
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
            format!("присутствует с неожидаемыми аргументами: {}", found[0].args),
        ),
        None => check(name, false, "не найден".to_string()),
    }
}

/// Doctor check: the native virtmic node exists EXACTLY once.
fn doctor_virtmic_node(native: &mut dyn NativeBackend, name: &str) -> anyhow::Result<Check> {
    let nodes = native.nodes()?;
    let mut ids: Vec<u32> = nodes
        .iter()
        .filter(|n| n.name.as_deref() == Some(NODE_VIRTMIC))
        .map(|n| n.id)
        .collect();
    ids.sort_unstable();
    Ok(match ids.as_slice() {
        [] => check(name, false, "не найден".to_string()),
        [id] => check(name, true, format!("id {id}")),
        many => check(
            name,
            false,
            format!("найдено {} узлов: id {:?}", many.len(), many),
        ),
    })
}

/// Doctor check: the pulse source `virtmic` exists and is NOT a monitor —
/// the Zoom-visibility check (Zoom filters monitor sources from its mic
/// list).
fn doctor_virtmic_source(name: &str, pulse_sources: Option<&[PulseSourceInfo]>) -> Check {
    let Some(sources) = pulse_sources else {
        return skipped(name);
    };
    let found = sources.iter().find(|s| s.name == SOURCE_VIRTMIC);
    match found {
        None => check(
            name,
            false,
            "источник virtmic не найден (узел не создан?)".to_string(),
        ),
        Some(source) if source.monitor_of.is_some() => check(
            name,
            false,
            "virtmic является монитором синка — Zoom такие источники фильтрует".to_string(),
        ),
        Some(source) => check(name, true, format!("индекс {}, не монитор", source.index)),
    }
}

/// Doctor check over link pairs: every `wanted` pair must be present in the
/// reported link list.
fn doctor_links(
    native: &mut dyn NativeBackend,
    name: &str,
    wanted: &[(String, String, String, String)],
) -> anyhow::Result<Check> {
    let links = native.links()?;
    let present: HashSet<(&str, &str, &str, &str)> = links
        .iter()
        .map(|l| {
            (
                l.out_node.as_str(),
                l.out_port.as_str(),
                l.in_node.as_str(),
                l.in_port.as_str(),
            )
        })
        .collect();
    let missing: Vec<String> = wanted
        .iter()
        .filter(|p| !present.contains(&(p.0.as_str(), p.1.as_str(), p.2.as_str(), p.3.as_str())))
        .map(|p| format!("{}:{}->{}:{}", p.0, p.1, p.2, p.3))
        .collect();
    Ok(if missing.is_empty() {
        let detail: Vec<String> = wanted
            .iter()
            .map(|p| format!("{}:{}->{}:{}", p.0, p.1, p.2, p.3))
            .collect();
        check(name, true, detail.join(", "))
    } else {
        check(name, false, format!("отсутствуют: {}", missing.join(", ")))
    })
}

/// Native doctor check wrapper: node presence, never fails.
fn native_node_check(native: &mut dyn NativeBackend, name: &str) -> Check {
    doctor_virtmic_node(native, name)
        .unwrap_or_else(|e| check(name, false, format!("ошибка: {e:#}")))
}

/// Native doctor check wrapper: the always-on meme links, never fails.
fn memes_links_check(native: &mut dyn NativeBackend, name: &str) -> Check {
    let wanted: Vec<(String, String, String, String)> = MEMES_MONITOR_PORTS
        .iter()
        .zip(VIRTMIC_INPUT_PORTS)
        .map(|(out, input)| {
            (
                SINK_MEMES.to_string(),
                (*out).to_string(),
                NODE_VIRTMIC.to_string(),
                input.to_string(),
            )
        })
        .collect();
    doctor_links(native, name, &wanted)
        .unwrap_or_else(|e| check(name, false, format!("ошибка: {e:#}")))
}

/// Diagnoses the graph state on both layers; never fails — problems land in
/// the report. Pulse-side checks are skipped when the pulse server is
/// unreachable; native-side checks are evaluated independently (they shell
/// out to pw-cli/pw-link).
pub fn doctor_with(
    backend: &mut dyn PulseBackend,
    native: &mut dyn NativeBackend,
    cfg: &Config,
) -> DoctorReport {
    const NAMES_SERVER: &str = "pulse-сервер доступен";
    const NAMES_SINK_MEMES: &str = "синк soundboard присутствует";
    const NAMES_VIRTMIC_SOURCE: &str =
        "источник virtmic присутствует и не является монитором (виден Zoom)";
    const NAMES_VIRTMIC_NODE: &str = "нативный узел virtmic присутствует (ровно один)";
    const NAMES_MEMES_LINKS: &str = "связи soundboard:monitor_{FL,FR}->virtmic присутствуют";
    const NAMES_REAL_MIC: &str = "реальный микрофон найден";
    const NAMES_MIC_LINKS: &str = "mic-связи real-mic:capture_{FL,FR}->virtmic присутствуют";
    const NAMES_LOOPBACK_MONITOR: &str =
        "monitoring loopback soundboard.monitor->real-sink присутствует";

    // Native-side checks: evaluated independently of the pulse server.
    let native_node_check = native_node_check(native, NAMES_VIRTMIC_NODE);
    let memes_links_check = memes_links_check(native, NAMES_MEMES_LINKS);

    let modules = match backend.modules() {
        Err(e) => {
            let mut checks = vec![check(NAMES_SERVER, false, format!("ошибка: {e:#}"))];
            checks.push(skipped(NAMES_SINK_MEMES));
            checks.push(skipped(NAMES_VIRTMIC_SOURCE));
            // The native layer does not go through pulse: still evaluated.
            checks.push(native_node_check);
            checks.push(memes_links_check);
            checks.push(skipped(NAMES_REAL_MIC));
            if cfg.mic_mix {
                checks.push(skipped(NAMES_MIC_LINKS));
            }
            if cfg.monitoring {
                checks.push(skipped(NAMES_LOOPBACK_MONITOR));
            }
            return DoctorReport { checks };
        }
        Ok(modules) => modules,
    };

    let mut checks = vec![check(NAMES_SERVER, true, "сервер ответил".to_string())];
    let mut ours_by_slot: BTreeMap<Slot, Vec<ModuleInfo>> = BTreeMap::new();
    for module in &modules {
        if let Some(slot) = classify(module) {
            ours_by_slot.entry(slot).or_default().push(module.clone());
        }
    }

    checks.push(slot_check(
        NAMES_SINK_MEMES,
        &ours_by_slot,
        Slot::SinkMemes,
        &null_sink_args(SINK_MEMES, "Meme_Soundboard"),
    ));

    // Zoom-visibility: source virtmic must exist and not be a monitor.
    let sources = backend.sources();
    checks.push(match &sources {
        Ok(sources) => doctor_virtmic_source(NAMES_VIRTMIC_SOURCE, Some(sources)),
        Err(e) => check(NAMES_VIRTMIC_SOURCE, false, format!("ошибка: {e:#}")),
    });

    checks.push(native_node_check);
    checks.push(memes_links_check);

    let real_mic_state = real_mic(backend);
    match &real_mic_state {
        Ok(Some(mic)) => checks.push(check(
            NAMES_REAL_MIC,
            true,
            format!("источник по умолчанию: {mic}"),
        )),
        Ok(None) => checks.push(check(
            NAMES_REAL_MIC,
            false,
            "источник по умолчанию не задан или является монитором".to_string(),
        )),
        Err(e) => checks.push(check(NAMES_REAL_MIC, false, format!("ошибка: {e:#}"))),
    }

    if cfg.mic_mix {
        match &real_mic_state {
            Ok(Some(mic)) => {
                let mic_wanted: Vec<(String, String, String, String)> = MIC_CAPTURE_PORTS
                    .iter()
                    .zip(VIRTMIC_INPUT_PORTS)
                    .map(|(out, input)| {
                        (
                            mic.clone(),
                            (*out).to_string(),
                            NODE_VIRTMIC.to_string(),
                            input.to_string(),
                        )
                    })
                    .collect();
                checks.push(
                    doctor_links(native, NAMES_MIC_LINKS, &mic_wanted).unwrap_or_else(|e| {
                        check(NAMES_MIC_LINKS, false, format!("ошибка: {e:#}"))
                    }),
                );
            }
            Ok(None) => checks.push(skipped(NAMES_MIC_LINKS)),
            Err(_) => checks.push(skipped(NAMES_MIC_LINKS)),
        }
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
                        format!("не найден (синк по умолчанию: {sink})"),
                    )
                });
            }
            Ok(None) => checks.push(check(
                NAMES_LOOPBACK_MONITOR,
                false,
                "нет синка по умолчанию".to_string(),
            )),
            Err(e) => checks.push(check(
                NAMES_LOOPBACK_MONITOR,
                false,
                format!("ошибка: {e:#}"),
            )),
        }
    }

    DoctorReport { checks }
}

/// Facade over the two backends implementing the graph operations. Owns the
/// state file: loads it before and persists it after every mutation.
pub struct Graph<P: PulseBackend, N: NativeBackend> {
    backend: P,
    native: N,
    state_path: PathBuf,
}

impl<P: PulseBackend, N: NativeBackend> Graph<P, N> {
    /// Wraps the backends, persisting ownership at `state_path`
    /// (see [`default_state_path`]).
    #[must_use]
    pub fn new(backend: P, native: N, state_path: PathBuf) -> Self {
        Graph {
            backend,
            native,
            state_path,
        }
    }

    /// Idempotent desired-state reconcile (see [`reconcile_with`]); the
    /// state file is reloaded first and rewritten after.
    ///
    /// # Errors
    /// Also fails when the state file cannot be persisted after a successful
    /// reconcile (the next `down` would not find our modules otherwise).
    pub fn reconcile(&mut self, cfg: &Config) -> anyhow::Result<Report> {
        let owned = ModuleState::load(&self.state_path);
        let (report, state) = reconcile_with(&mut self.backend, &mut self.native, cfg, &owned)?;
        state.save(&self.state_path)?;
        Ok(report)
    }

    /// Removes everything of ours (see [`teardown_with`]); persists the state.
    pub fn teardown(&mut self) -> anyhow::Result<Report> {
        let owned = ModuleState::load(&self.state_path);
        let (report, state) = teardown_with(&mut self.backend, &mut self.native, &owned)?;
        state.save(&self.state_path)?;
        Ok(report)
    }

    /// Diagnostics (see [`doctor_with`]); read-only, never adopts or mutates
    /// the state.
    pub fn doctor(&mut self, cfg: &Config) -> DoctorReport {
        doctor_with(&mut self.backend, &mut self.native, cfg)
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
            IterateResult::Quit(_) => bail!("pulse-мэйнлуп неожиданно завершился"),
            IterateResult::Err(e) => bail!("ошибка pulse-мэйнлупа: {e}"),
        }
        match context.get_state() {
            ContextState::Failed => bail!("pulse-соединение потеряно"),
            ContextState::Terminated => bail!("pulse-соединение завершено"),
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
        .ok_or_else(|| anyhow::anyhow!("pulse-сервер не дал результат для {what}"))
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
            .map_err(|e| anyhow::anyhow!("не удалось начать pulse-соединение: {e}"))?;
        loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) => bail!("pulse-мэйнлуп завершился при подключении"),
                IterateResult::Err(e) => bail!("ошибка pulse-мэйнлупа при подключении: {e}"),
            }
            match context.get_state() {
                ContextState::Ready => return Ok(PulseSession { context, mainloop }),
                ContextState::Failed | ContextState::Terminated => {
                    bail!("не удалось подключиться к pulse-серверу (запущен ли pipewire-pulse?)")
                }
                _ => {}
            }
        }
    }

    fn run_op<C: ?Sized>(&mut self, op: &Operation<C>) -> anyhow::Result<()> {
        wait_op(&mut self.mainloop, &self.context, op)
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
            bail!("pulse-сервер не смог перечислить модули");
        }
        Ok(std::mem::take(&mut *out.borrow_mut()))
    }

    fn sources(&mut self) -> anyhow::Result<Vec<PulseSourceInfo>> {
        let introspect = self.context.introspect();
        let out: Rc<RefCell<Vec<PulseSourceInfo>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = out.clone();
        let op = introspect.get_source_info_list(move |item: ListResult<&PaSourceInfo>| {
            if let ListResult::Item(info) = item {
                sink.borrow_mut().push(PulseSourceInfo {
                    index: info.index,
                    name: info
                        .name
                        .as_ref()
                        .map_or_else(String::new, |c| c.to_string()),
                    monitor_of: info
                        .monitor_of_sink
                        .filter(|sink_index| *sink_index != def::INVALID_INDEX),
                });
            }
        });
        wait_op(&mut self.mainloop, &self.context, &op)?;
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
            def::INVALID_INDEX => bail!("pulse-сервер отказался загружать {name}"),
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
            false => bail!("pulse-сервер отказался выгружать модуль #{index}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory pulse backend for pure reconcile tests.
    #[derive(Debug, Default)]
    struct MockBackend {
        modules: Vec<ModuleInfo>,
        sources: Vec<PulseSourceInfo>,
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

        fn with_source(mut self, name: &str, monitor_of: Option<u32>) -> Self {
            self.sources.push(PulseSourceInfo {
                index: self.next_index,
                name: name.to_string(),
                monitor_of,
            });
            self.next_index += 1;
            self
        }
    }

    impl PulseBackend for MockBackend {
        fn modules(&mut self) -> anyhow::Result<Vec<ModuleInfo>> {
            if self.unreachable {
                bail!("в соединении отказано");
            }
            Ok(self.modules.clone())
        }

        fn sources(&mut self) -> anyhow::Result<Vec<PulseSourceInfo>> {
            if self.unreachable {
                bail!("в соединении отказано");
            }
            Ok(self.sources.clone())
        }

        fn load(&mut self, name: &str, args: &str) -> anyhow::Result<u32> {
            if self.unreachable {
                bail!("в соединении отказано");
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
                bail!("в соединении отказано");
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

    /// Node/port fixture shared by the native mocks: soundboard (id 100),
    /// virtmic (id 200), real mic (id 300) with the recipe-verified port
    /// names.
    fn fixture_nodes() -> Vec<NativeNodeInfo> {
        vec![
            NativeNodeInfo {
                id: 100,
                name: Some(SINK_MEMES.to_string()),
            },
            NativeNodeInfo {
                id: 200,
                name: Some(NODE_VIRTMIC.to_string()),
            },
            NativeNodeInfo {
                id: 300,
                name: Some("alsa_input.real".to_string()),
            },
        ]
    }

    fn fixture_ports() -> Vec<NativePortInfo> {
        let mut out = Vec::new();
        let mut id = 1;
        for (node_id, names, direction) in [
            (100_u32, MEMES_MONITOR_PORTS.to_vec(), "out"),
            (200, VIRTMIC_INPUT_PORTS.to_vec(), "in"),
            (300, MIC_CAPTURE_PORTS.to_vec(), "out"),
        ] {
            for name in names {
                out.push(NativePortInfo {
                    id,
                    node_id,
                    name: name.to_string(),
                    direction: direction.to_string(),
                });
                id += 1;
            }
        }
        out
    }

    /// In-memory native backend: keeps nodes/links, simulates pw-cli/pw-link
    /// semantics (create_node adds a named virtmic node; links dedupe by
    /// pair).
    #[derive(Debug, Default)]
    struct MockNative {
        nodes: Vec<NativeNodeInfo>,
        ports: Vec<NativePortInfo>,
        links: Vec<NativeLinkInfo>,
        next_node_id: u32,
        create_node_should_fail: bool,
        last_create_args: Option<Vec<String>>,
    }

    impl MockNative {
        fn with_virtmic() -> Self {
            MockNative {
                nodes: fixture_nodes(),
                ports: fixture_ports(),
                next_node_id: 1000,
                ..MockNative::default()
            }
        }

        /// Graph before the first `up`: the soundboard sink node exists (the
        /// pulse layer created it) but no virtmic node.
        fn with_soundboard() -> Self {
            MockNative {
                nodes: fixture_nodes().into_iter().take(1).collect(),
                ports: fixture_ports().into_iter().take(2).collect(),
                next_node_id: 1000,
                ..MockNative::default()
            }
        }

        fn with_link(
            mut self,
            out_node: &str,
            out_port: &str,
            in_node: &str,
            in_port: &str,
        ) -> Self {
            self.links
                .push(NativeLinkInfo::new(out_node, out_port, in_node, in_port));
            self
        }

        fn with_extra_node(mut self, id: u32, name: Option<&str>) -> Self {
            self.nodes.push(NativeNodeInfo {
                id,
                name: name.map(str::to_string),
            });
            self
        }

        /// Adds the virtmic input ports for `node_id` (the duplicate-keep test
        /// keeps a node whose id differs from the fixture's canonical 200).
        fn with_input_ports(mut self, node_id: u32) -> Self {
            for (i, name) in VIRTMIC_INPUT_PORTS.iter().enumerate() {
                self.ports.push(NativePortInfo {
                    id: 900 + i as u32,
                    node_id,
                    name: (*name).to_string(),
                    direction: "in".to_string(),
                });
            }
            self
        }

        fn virtmic_ids(&self) -> Vec<u32> {
            let mut ids: Vec<u32> = self
                .nodes
                .iter()
                .filter(|n| n.name.as_deref() == Some(NODE_VIRTMIC))
                .map(|n| n.id)
                .collect();
            ids.sort_unstable();
            ids
        }
    }

    impl NativeBackend for MockNative {
        fn nodes(&mut self) -> anyhow::Result<Vec<NativeNodeInfo>> {
            Ok(self.nodes.clone())
        }

        fn ports(&mut self) -> anyhow::Result<Vec<NativePortInfo>> {
            Ok(self.ports.clone())
        }

        fn links(&mut self) -> anyhow::Result<Vec<NativeLinkInfo>> {
            Ok(self.links.clone())
        }

        fn create_node(&mut self, args: &[String]) -> anyhow::Result<()> {
            self.last_create_args = Some(args.to_vec());
            if self.create_node_should_fail {
                bail!("pw-cli create-node failed");
            }
            self.next_node_id += 1;
            let id = self.next_node_id;
            self.nodes.push(NativeNodeInfo {
                id,
                name: Some(NODE_VIRTMIC.to_string()),
            });
            // The adapter node comes with its input ports (like the real
            // one), so the mock passes the port-existence validation.
            for (i, name) in VIRTMIC_INPUT_PORTS.iter().enumerate() {
                self.ports.push(NativePortInfo {
                    id: 500 + i as u32,
                    node_id: id,
                    name: (*name).to_string(),
                    direction: "in".to_string(),
                });
            }
            Ok(())
        }

        fn destroy_node(&mut self, id: u32) -> anyhow::Result<()> {
            self.nodes.retain(|n| n.id != id);
            Ok(())
        }

        fn create_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()> {
            self.links
                .push(NativeLinkInfo::new(out.0, out.1, input.0, input.1));
            Ok(())
        }

        fn destroy_link(&mut self, out: (&str, &str), input: (&str, &str)) -> anyhow::Result<()> {
            self.links.retain(|l| {
                !(l.out_node == out.0
                    && l.out_port == out.1
                    && l.in_node == input.0
                    && l.in_port == input.1)
            });
            Ok(())
        }
    }

    fn memes_sink_args() -> String {
        null_sink_args(SINK_MEMES, "Meme_Soundboard")
    }

    fn loopback_args(source: &str, sink: &str) -> String {
        format!("source={source} sink={sink}")
    }

    /// Pulse backend with only the memes sink; passthroughs off.
    fn clean_pulse_backend() -> MockBackend {
        MockBackend::new(Some("alsa_output.real"), Some("alsa_input.real"))
            .with_module(MODULE_NULL_SINK, &memes_sink_args())
    }

    fn passthroughs_off() -> Config {
        Config {
            mic_mix: false,
            monitoring: false,
            ..Config::default()
        }
    }

    /// State marking every pulse module currently in the backend plus the
    /// virtmic node as ours.
    fn seed_state(backend: &mut MockBackend, native: &MockNative) -> ModuleState {
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
            native_nodes: native
                .virtmic_ids()
                .into_iter()
                .map(|id| OwnedNativeNode {
                    id,
                    name: NODE_VIRTMIC.to_string(),
                })
                .collect(),
        }
    }

    /// Full clean graph for passthroughs-off: pulse memes sink + native
    /// virtmic node + both meme links.
    fn clean_native() -> MockNative {
        MockNative::with_virtmic()
            .with_link(SINK_MEMES, "monitor_FL", NODE_VIRTMIC, "input_FL")
            .with_link(SINK_MEMES, "monitor_FR", NODE_VIRTMIC, "input_FR")
    }

    #[test]
    fn virtmic_node_args_match_verified_recipe() {
        // The exact live-verified recipe string (research slice); the argv
        // after `pw-cli` is: create-node, adapter, one braced payload.
        let args = virtmic_node_args();
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "create-node");
        assert_eq!(args[1], "adapter");
        assert_eq!(
            args[2],
            "{ factory.name=support.null-audio-sink node.name=virtmic \
             media.class=Audio/Source/Virtual node.description=Virtual_Mic \
             audio.position=[ FL, FR ] object.linger=true }"
        );
        // Syntax traps: no quotes around values; array form for
        // audio.position; no monitor.channel-volumes anywhere.
        assert!(!args[2].contains('"'));
        assert!(args[2].contains("audio.position=[ FL, FR ]"));
        assert!(!args[2].contains("monitor.channel-volumes"));
    }

    /// Real `pw-cli ls Node` output shape (captured live 2026-10-12).
    const NODES_SAMPLE: &str = "\
	id 29, type PipeWire:Interface:Node/3
 		object.serial = \"29\"
 		factory.id = \"11\"
 		node.name = \"Dummy-Driver\"
	id 62, type PipeWire:Interface:Node/3
 		object.serial = \"409\"
 		node.description = \"Virtual_Mic\"
 		node.name = \"virtmic\"
 		media.class = \"Audio/Source/Virtual\"
	id 107, type PipeWire:Interface:Node/3
 		object.serial = \"363\"
 		node.description = \"Meme_Soundboard\"
 		node.name = \"soundboard\"
	id 118, type PipeWire:Interface:Node/3
 		object.serial = \"412\"
 		node.description = \"Virtual_Mic\"
 		node.name = \"virtmic\"";

    #[test]
    fn parse_nodes_extracts_ids_and_names() {
        let nodes = parse_nodes(NODES_SAMPLE);
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0].id, 29);
        assert_eq!(nodes[0].name.as_deref(), Some("Dummy-Driver"));
        assert_eq!(
            nodes
                .iter()
                .filter(|n| n.name.as_deref() == Some("virtmic"))
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![62, 118]
        );
        assert_eq!(
            nodes
                .iter()
                .find(|n| n.name.as_deref() == Some("soundboard"))
                .map(|n| n.id),
            Some(107)
        );
    }

    #[test]
    fn parse_nodes_handles_missing_name() {
        let nodes = parse_nodes("	id 42, type PipeWire:Interface:Node/3\n 		object.serial = \"7\"");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, 42);
        assert_eq!(nodes[0].name, None);
    }

    /// Real `pw-cli ls Port` output shape (captured live 2026-10-12).
    const PORTS_SAMPLE: &str = "\
	id 63, type PipeWire:Interface:Port/3
 		object.serial = \"63\"
 		format.dsp = \"32 bit float mono audio\"
 		node.id = \"107\"
 		audio.channel = \"FL\"
 		port.id = \"0\"
 		port.name = \"monitor_FL\"
 		port.direction = \"out\"
 		port.monitor = \"true\"
	id 64, type PipeWire:Interface:Port/3
 		format.dsp = \"32 bit float mono audio\"
 		node.id = \"107\"
 		audio.channel = \"FR\"
 		port.id = \"1\"
 		port.name = \"monitor_FR\"
 		port.direction = \"out\"
 		port.monitor = \"true\"
	id 65, type PipeWire:Interface:Port/3
 		format.dsp = \"32 bit float mono audio\"
 		node.id = \"62\"
 		audio.channel = \"FL\"
 		port.id = \"0\"
 		port.name = \"input_FL\"
 		port.direction = \"in\"";

    #[test]
    fn parse_ports_maps_node_ids_and_names() {
        let ports = parse_ports(PORTS_SAMPLE);
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].node_id, 107);
        assert_eq!(ports[0].name, "monitor_FL");
        assert_eq!(ports[0].direction, "out");
        assert_eq!(ports[1].name, "monitor_FR");
        assert_eq!(ports[2].node_id, 62);
        assert_eq!(ports[2].name, "input_FL");
        assert_eq!(ports[2].direction, "in");
    }

    /// Real `pw-link -l` output shape (captured live 2026-10-12, trimmed).
    const PWLINK_SAMPLE: &str = "\
alsa_input.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Mic__source:capture_FL
  |-> ZOOM VoiceEngine:input_FL
  |-> virtmic:input_FL
soundboard:monitor_FL
  |-> input.loopback-2503-15:input_FL
  |-> virtmic:input_FL
input.loopback-2503-15:input_FL
  |<- soundboard:monitor_FL
virtmic:input_FL
  |<- soundboard:monitor_FL
  |<- alsa_input.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Mic__source:capture_FL";

    #[test]
    fn parse_pwlink_list_builds_directed_pairs() {
        let links = parse_pwlink_list(PWLINK_SAMPLE);
        assert_eq!(links.len(), 7);
        // Both arrow directions must yield the same directed pair (the
        // listing shows each link under both endpoints).
        let memes = NativeLinkInfo::new("soundboard", "monitor_FL", "virtmic", "input_FL");
        assert_eq!(
            links.iter().filter(|l| *l == &memes).count(),
            2,
            "memes link must be listed from both endpoints"
        );
        let mic = NativeLinkInfo::new(
            "alsa_input.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Mic__source",
            "capture_FL",
            "virtmic",
            "input_FL",
        );
        assert_eq!(links.iter().filter(|l| *l == &mic).count(), 2);
        assert!(links.contains(&NativeLinkInfo::new(
            "soundboard",
            "monitor_FL",
            "input.loopback-2503-15",
            "input_FL"
        )));
    }

    #[test]
    fn link_diff_creates_missing_and_destroys_our_extras() {
        let desired = desired_link_pairs(None);
        // Missing FL link: one create. An extra mic link (mic_mix off) and a
        // foreign mic->alsa-sink link: only the virtmic-touching one is
        // destroyed.
        let existing = vec![
            NativeLinkInfo::new(SINK_MEMES, "monitor_FR", NODE_VIRTMIC, "input_FR"),
            NativeLinkInfo::new("alsa_input.real", "capture_FL", NODE_VIRTMIC, "input_FL"),
            NativeLinkInfo::new(
                "alsa_input.real",
                "capture_FL",
                "alsa_output.real",
                "playback_FL",
            ),
        ];
        let diff = link_diff(&desired, &existing);
        let creates: Vec<_> = diff.iter().filter(|c| !c.destroy).collect();
        let destroys: Vec<_> = diff.iter().filter(|c| c.destroy).collect();
        assert_eq!(creates.len(), 1, "missing FL memes link must be created");
        assert_eq!(
            creates[0].pair,
            (
                SINK_MEMES.to_string(),
                "monitor_FL".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FL".to_string()
            )
        );
        assert_eq!(destroys.len(), 1, "only the extra mic->virtmic link");
        assert_eq!(
            destroys[0].pair,
            (
                "alsa_input.real".to_string(),
                "capture_FL".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FL".to_string()
            )
        );
    }

    #[test]
    fn link_diff_is_noop_when_graph_matches_desire() {
        let desired = desired_link_pairs(Some("alsa_input.real"));
        let existing = vec![
            NativeLinkInfo::new(SINK_MEMES, "monitor_FL", NODE_VIRTMIC, "input_FL"),
            NativeLinkInfo::new(SINK_MEMES, "monitor_FR", NODE_VIRTMIC, "input_FR"),
            NativeLinkInfo::new("alsa_input.real", "capture_FL", NODE_VIRTMIC, "input_FL"),
            NativeLinkInfo::new("alsa_input.real", "capture_FR", NODE_VIRTMIC, "input_FR"),
        ];
        // Duplicates (pw-link -l lists each link from both endpoints) must
        // not confuse the diff.
        let mut with_dups = existing.clone();
        with_dups.extend(existing.iter().cloned());
        let diff = link_diff(&desired, &with_dups);
        assert!(diff.is_empty(), "expected no changes, got {diff:?}");
    }

    #[test]
    fn desired_link_pairs_cover_fl_fr_and_mic() {
        let pairs = desired_link_pairs(Some("mic.x"));
        assert_eq!(pairs.len(), 4);
        assert_eq!(
            pairs[0],
            (
                SINK_MEMES.to_string(),
                "monitor_FL".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FL".to_string()
            )
        );
        assert_eq!(
            pairs[1],
            (
                SINK_MEMES.to_string(),
                "monitor_FR".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FR".to_string()
            )
        );
        assert_eq!(
            pairs[2],
            (
                "mic.x".to_string(),
                "capture_FL".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FL".to_string()
            )
        );
        assert_eq!(
            pairs[3],
            (
                "mic.x".to_string(),
                "capture_FR".to_string(),
                NODE_VIRTMIC.to_string(),
                "input_FR".to_string()
            )
        );
        assert_eq!(desired_link_pairs(None).len(), 2);
    }

    #[test]
    fn fresh_up_creates_node_and_links() {
        let mut backend = clean_pulse_backend();
        let mut native = MockNative::with_soundboard();
        let (report, state) = reconcile_with(
            &mut backend,
            &mut native,
            &passthroughs_off(),
            &ModuleState::empty(),
        )
        .expect("reconcile");
        // Pulse: sink pre-exists and is ADOPTED (no Load change); native:
        // node created, both links created.
        assert_eq!(report.changes.len(), 3, "report: {report:?}");
        assert!(report.warnings.is_empty());
        assert_eq!(native.virtmic_ids().len(), 1);
        assert_eq!(native.links.len(), 2);
        assert!(state.native_nodes.len() == 1);
        // The created node got the verified payload.
        let args = native.last_create_args.as_ref().expect("create called");
        assert_eq!(args, &virtmic_node_args());
    }

    #[test]
    fn clean_state_reconcile_is_noop() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let owned = seed_state(&mut backend, &native);
        let (report, state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert!(report.is_noop(), "expected no changes, got: {report:?}");
        assert_eq!(backend.modules().expect("modules").len(), 1);
        assert_eq!(native.links.len(), 2);
        assert_eq!(state, owned);
    }

    #[test]
    fn second_up_is_idempotent() {
        let mut backend = clean_pulse_backend();
        let mut native = MockNative::with_soundboard();
        let cfg = passthroughs_off();
        let (first, state) =
            reconcile_with(&mut backend, &mut native, &cfg, &ModuleState::empty()).expect("first");
        assert!(!first.is_noop());
        let (second, state2) =
            reconcile_with(&mut backend, &mut native, &cfg, &state).expect("second");
        assert!(second.is_noop(), "second up must be a no-op: {second:?}");
        assert_eq!(state2, state);
        assert_eq!(native.virtmic_ids().len(), 1);
    }

    #[test]
    fn mic_links_created_when_mic_mix_on() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let owned = seed_state(&mut backend, &native);
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let (report, _state) =
            reconcile_with(&mut backend, &mut native, &cfg, &owned).expect("reconcile");
        assert!(report.warnings.is_empty(), "{report:?}");
        assert_eq!(report.changes.len(), 2);
        for c in &report.changes {
            assert_eq!(c.action, ChangeAction::Create);
            assert!(c.slot.contains("alsa_input.real"));
        }
        let mic_links: Vec<_> = native
            .links
            .iter()
            .filter(|l| l.out_node == "alsa_input.real" && l.in_node == NODE_VIRTMIC)
            .collect();
        assert_eq!(mic_links.len(), 2);
    }

    #[test]
    fn mic_links_destroyed_when_mic_mix_off() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native()
            .with_link("alsa_input.real", "capture_FL", NODE_VIRTMIC, "input_FL")
            .with_link("alsa_input.real", "capture_FR", NODE_VIRTMIC, "input_FR");
        let owned = seed_state(&mut backend, &native);
        let (report, _state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert_eq!(report.changes.len(), 2);
        for c in &report.changes {
            assert_eq!(c.action, ChangeAction::Destroy);
        }
        assert!(
            !native
                .links
                .iter()
                .any(|l| l.out_node == "alsa_input.real" && l.in_node == NODE_VIRTMIC)
        );
        assert_eq!(native.links.len(), 2, "memes links must survive");
    }

    #[test]
    fn mic_mix_with_monitor_default_source_warns_and_skips() {
        let mut backend = clean_pulse_backend();
        backend.default_source = Some("soundboard.monitor".to_string());
        let mut native = clean_native();
        let owned = seed_state(&mut backend, &native);
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let (report, _state) =
            reconcile_with(&mut backend, &mut native, &cfg, &owned).expect("reconcile");
        assert!(report.changes.is_empty(), "no changes expected: {report:?}");
        assert_eq!(report.warnings.len(), 1);
        assert!(!native.links.iter().any(|l| l.out_node == "alsa_input.real"));
    }

    #[test]
    fn foreign_link_is_untouched() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native().with_link(
            "alsa_input.real",
            "capture_FL",
            "alsa_output.real",
            "playback_FL",
        );
        let owned = seed_state(&mut backend, &native);
        let (report, _state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert!(report.is_noop(), "foreign link must be ignored: {report:?}");
        assert!(
            native.links.iter().any(|l| l.in_node == "alsa_output.real"),
            "foreign link must survive"
        );
    }

    #[test]
    fn duplicate_virtmic_nodes_keep_lowest() {
        let mut backend = clean_pulse_backend();
        // The duplicate (id 50) sits BELOW the canonical one (id 200):
        // links point at virtmic by NAME, and the kept node 50 gets its own
        // input ports so the desired links stay valid.
        let mut native = clean_native()
            .with_extra_node(50, Some(NODE_VIRTMIC))
            .with_input_ports(50);
        let owned = seed_state(&mut backend, &native);
        let (report, _state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert_eq!(native.virtmic_ids(), vec![50]);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("несколько узлов virtmic") && w.contains("200")),
            "expected duplicate warning naming the destroyed id, got: {report:?}"
        );
        assert_eq!(
            report
                .changes
                .iter()
                .filter(|c| c.action == ChangeAction::Destroy)
                .count(),
            1
        );
        // Links are intact (they reference the node by name).
        assert_eq!(native.links.len(), 2);
    }

    #[test]
    fn stale_native_state_entry_is_pruned() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let mut owned = seed_state(&mut backend, &native);
        // A dead record: pipewire restarted, node id 999 is gone.
        owned.native_nodes.push(OwnedNativeNode {
            id: 999,
            name: NODE_VIRTMIC.to_string(),
        });
        let (_report, state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert!(!state.native_nodes.iter().any(|n| n.id == 999));
        assert_eq!(state.native_nodes.len(), 1);
    }

    #[test]
    fn monitoring_loopback_targets_default_sink() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let owned = seed_state(&mut backend, &native);
        let cfg = Config {
            mic_mix: false,
            monitoring: true,
            ..Config::default()
        };
        let (_report, _state) =
            reconcile_with(&mut backend, &mut native, &cfg, &owned).expect("reconcile");
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
        let mut backend = clean_pulse_backend().with_module(MODULE_NULL_SINK, &memes_sink_args());
        let mut native = clean_native();
        let owned = seed_state(&mut backend, &native);
        let (report, state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert_eq!(
            report
                .changes
                .iter()
                .filter(|c| c.action == ChangeAction::Unload)
                .count(),
            1
        );
        assert_eq!(
            backend
                .modules()
                .expect("modules")
                .iter()
                .filter(|m| m.name == MODULE_NULL_SINK)
                .count(),
            1
        );
        assert_eq!(
            state
                .modules
                .iter()
                .filter(|o| o.name == MODULE_NULL_SINK)
                .count(),
            1
        );
    }

    #[test]
    fn foreign_lookalike_loopback_survives_up_and_down() {
        // A module-loopback from soundboard.monitor that we never loaded
        // (absent from the state) with DIFFERENT args: never adopted, never
        // unloaded; `up` warns, `down` leaves it alone.
        let lookalike = format!(
            "{} latency_msec=50",
            loopback_args(SOURCE_MEMES_MONITOR, "alsa_output.real")
        );
        let mut backend = clean_pulse_backend().with_module(MODULE_LOOPBACK, &lookalike);
        let mut native = clean_native();
        // Seed only the memes sink (index 0) and the virtmic node as ours;
        // the lookalike (index 1) stays unowned.
        let ours = ModuleState {
            version: STATE_VERSION,
            modules: vec![OwnedModule {
                index: 0,
                name: MODULE_NULL_SINK.to_string(),
            }],
            native_nodes: vec![OwnedNativeNode {
                id: 200,
                name: NODE_VIRTMIC.to_string(),
            }],
        };
        // up
        let (up_report, up_state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &ours)
                .expect("reconcile");
        assert!(
            up_report.warnings.iter().any(|w| {
                w.starts_with("чужой loopback от soundboard.monitor оставлен без изменений")
                    && w.contains(&lookalike)
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
        assert!(!up_state.modules.iter().any(|o| o.index == 1));
        // down
        let (down_report, down_state) =
            teardown_with(&mut backend, &mut native, &up_state).expect("teardown");
        // 1 pulse module + 2 links + 1 node.
        assert_eq!(down_report.changes.len(), 4);
        assert!(down_state.modules.is_empty());
        assert!(down_state.native_nodes.is_empty());
        let rest = backend.modules().expect("modules");
        assert_eq!(rest.len(), 1);
        assert_eq!(
            rest[0].args, lookalike,
            "foreign lookalike must survive down"
        );
        // The native node was torn down by name even though the state was
        // fresh from reconcile (name matching, not state matching).
        assert!(native.virtmic_ids().is_empty());
        assert!(native.links.is_empty());
    }

    #[test]
    fn exact_args_modules_and_native_node_after_state_loss_are_adopted_not_duplicated() {
        // Clean graph, empty state (state file lost): reconcile must adopt
        // the exact-args module AND the name-matched native node into a
        // fresh state, change nothing on the server, and the adopted state
        // must then drive teardown.
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let (report, state) = reconcile_with(
            &mut backend,
            &mut native,
            &passthroughs_off(),
            &ModuleState::empty(),
        )
        .expect("reconcile");
        assert!(
            report.is_noop(),
            "adoption must be a no-op, got: {report:?}"
        );
        assert!(report.warnings.is_empty());
        assert_eq!(backend.modules().expect("modules").len(), 1);
        assert_eq!(state.modules.len(), 1);
        assert_eq!(state.native_nodes.len(), 1);
        let (down_report, down_state) =
            teardown_with(&mut backend, &mut native, &state).expect("teardown");
        assert_eq!(down_report.changes.len(), 4); // module + 2 links + node
        assert!(down_state.modules.is_empty());
        assert!(down_state.native_nodes.is_empty());
        assert!(backend.modules().expect("modules").is_empty());
        assert!(native.virtmic_ids().is_empty());
        assert!(native.links.is_empty());
    }

    #[test]
    fn stale_state_indices_are_pruned() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let mut owned = seed_state(&mut backend, &native);
        // A dead record: the pulse server restarted, index 99 is gone.
        owned.modules.push(OwnedModule {
            index: 99,
            name: MODULE_LOOPBACK.to_string(),
        });
        let (_report, state) =
            reconcile_with(&mut backend, &mut native, &passthroughs_off(), &owned)
                .expect("reconcile");
        assert!(!state.modules.iter().any(|o| o.index == 99));
        assert_eq!(state.modules.len(), 1);
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
        // Wrong schema version: the pre-native v1 file must be dropped whole.
        let old = dir.path().join("old.json");
        std::fs::write(&old, r#"{"version": 1, "modules": []}"#).expect("write");
        assert_eq!(ModuleState::load(&old), ModuleState::empty());
        std::fs::write(&old, r#"{"version": 999, "modules": []}"#).expect("write");
        assert_eq!(ModuleState::load(&old), ModuleState::empty());
        // Valid file (with native nodes) round-trips.
        let good = dir.path().join("nested").join("modules.json");
        let state = ModuleState {
            version: STATE_VERSION,
            modules: vec![OwnedModule {
                index: 7,
                name: MODULE_NULL_SINK.to_string(),
            }],
            native_nodes: vec![OwnedNativeNode {
                id: 42,
                name: NODE_VIRTMIC.to_string(),
            }],
        };
        state.save(&good).expect("save creates parent dirs");
        assert_eq!(ModuleState::load(&good), state);
    }

    #[test]
    fn teardown_removes_links_node_and_exactly_our_modules() {
        let foreign = loopback_args("alsa_input.real", "alsa_output.real");
        let mut backend = clean_pulse_backend().with_module(MODULE_LOOPBACK, &foreign);
        // Seed only the memes sink as ours; the foreign loopback (index 1)
        // stays unowned even though a buggy seed would list it.
        let mut native = clean_native().with_link(
            "alsa_input.real",
            "capture_FL",
            "alsa_output.real",
            "playback_FL",
        );
        let owned = ModuleState {
            version: STATE_VERSION,
            modules: vec![OwnedModule {
                index: 0,
                name: MODULE_NULL_SINK.to_string(),
            }],
            native_nodes: seed_state(&mut backend, &native).native_nodes,
        };
        let (report, state) = teardown_with(&mut backend, &mut native, &owned).expect("teardown");
        // 1 module unload + 2 link destroys + 1 node destroy.
        assert_eq!(report.changes.len(), 4);
        assert!(state.modules.is_empty());
        assert!(state.native_nodes.is_empty());
        // Pulse: only the foreign module remains; native: only the foreign
        // link remains.
        let modules = backend.modules().expect("modules");
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].args, foreign);
        assert_eq!(native.links.len(), 1);
        assert!(native.virtmic_ids().is_empty());
    }

    #[test]
    fn teardown_without_native_node_is_still_clean() {
        let mut backend = clean_pulse_backend();
        // Graph partially up: no virtmic node at all (destroyed externally).
        let mut native = MockNative::default();
        let owned = seed_state(&mut backend, &native);
        let (report, state) = teardown_with(&mut backend, &mut native, &owned).expect("teardown");
        assert_eq!(report.changes.len(), 1); // only the pulse module
        assert!(state.modules.is_empty());
        assert!(state.native_nodes.is_empty());
    }

    #[test]
    fn classify_ignores_quoted_descriptions_and_foreign() {
        let ours = ModuleInfo {
            index: 1,
            name: MODULE_NULL_SINK.to_string(),
            args: "sink_name=soundboard media.class=Audio/Sink sink_properties=device.description=\"Meme_Soundboard\""
                .to_string(),
        };
        assert_eq!(classify(&ours), Some(Slot::SinkMemes));
        let monitor_loopback = ModuleInfo {
            index: 2,
            name: MODULE_LOOPBACK.to_string(),
            args: "source=soundboard.monitor sink=alsa_output.real".to_string(),
        };
        assert_eq!(classify(&monitor_loopback), Some(Slot::LoopbackMonitor));
        // The OLD-design modules (virtmic sink / loopback into virtmic) are
        // no longer our shape: they are never touched.
        let old_virtmic_sink = ModuleInfo {
            index: 3,
            name: MODULE_NULL_SINK.to_string(),
            args: "sink_name=virtmic media.class=Audio/Sink".to_string(),
        };
        assert_eq!(classify(&old_virtmic_sink), None);
        let foreign_loopback = ModuleInfo {
            index: 4,
            name: MODULE_LOOPBACK.to_string(),
            args: "source=x sink=soundboard".to_string(),
        };
        assert_eq!(classify(&foreign_loopback), None);
        let foreign_null = ModuleInfo {
            index: 5,
            name: MODULE_NULL_SINK.to_string(),
            args: "sink_name=other".to_string(),
        };
        assert_eq!(classify(&foreign_null), None);
    }

    #[test]
    fn doctor_all_pass_on_full_state() {
        let mut backend = clean_pulse_backend().with_source(SOURCE_VIRTMIC, None);
        let mut native = clean_native()
            .with_link("alsa_input.real", "capture_FL", NODE_VIRTMIC, "input_FL")
            .with_link("alsa_input.real", "capture_FR", NODE_VIRTMIC, "input_FR");
        let cfg = Config {
            mic_mix: true,
            monitoring: false,
            ..Config::default()
        };
        let report = doctor_with(&mut backend, &mut native, &cfg);
        assert!(
            report.all_passed(),
            "expected all pass, got:\n{}",
            report.render()
        );
        // server, sink, source-not-monitor, native node, memes links,
        // real mic, mic links
        assert_eq!(report.checks.len(), 7);
        assert!(report.render().contains(ZOOM_HINT));
    }

    #[test]
    fn doctor_flags_missing_parts() {
        let mut backend = clean_pulse_backend().with_source(SOURCE_VIRTMIC, None);
        // Native side empty: no node, no links.
        let mut native = MockNative {
            ports: fixture_ports(),
            ..MockNative::default()
        };
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &mut native, &cfg);
        assert!(!report.all_passed());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("нативный узел") && !c.passed)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name.contains("связи") && !c.passed)
        );
    }

    #[test]
    fn doctor_fails_when_virtmic_source_is_a_monitor() {
        // The regression this doctor check exists for: a monitor source named
        // virtmic (old broken design) — Zoom would never list it.
        let mut backend = clean_pulse_backend().with_source(SOURCE_VIRTMIC, Some(5));
        let mut native = clean_native();
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &mut native, &cfg);
        let zoom_check = report
            .checks
            .iter()
            .find(|c| c.name.contains("не является монитором"))
            .expect("zoom check present");
        assert!(!zoom_check.passed);
        assert!(zoom_check.detail.contains("монитором"));
    }

    #[test]
    fn doctor_fails_when_virtmic_source_absent() {
        let mut backend = clean_pulse_backend();
        let mut native = clean_native();
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &mut native, &cfg);
        let zoom_check = report
            .checks
            .iter()
            .find(|c| c.name.contains("не является монитором"))
            .expect("zoom check present");
        assert!(!zoom_check.passed);
    }

    #[test]
    fn doctor_reports_unreachable_server() {
        let mut backend = MockBackend::new(None, None);
        backend.unreachable = true;
        let mut native = clean_native();
        let cfg = passthroughs_off();
        let report = doctor_with(&mut backend, &mut native, &cfg);
        assert!(!report.all_passed());
        assert!(report.checks[0].name.contains("сервер"));
        assert!(!report.checks[0].passed);
        // Pulse-side checks are skipped...
        for c in &report.checks[1..] {
            if c.name.contains("синк")
                || c.name.contains("источник virtmic")
                || c.name.contains("реальный микрофон")
            {
                assert!(
                    !c.passed && c.detail.contains("пропущено"),
                    "pulse-side check must be skipped: {c:?}"
                );
            }
        }
        // ...but the native node check is evaluated on its own and passes.
        let node_check = report
            .checks
            .iter()
            .find(|c| c.name.contains("нативный узел"))
            .expect("native node check present");
        assert!(node_check.passed, "{node_check:?}");
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
        let backend = clean_pulse_backend().with_source(SOURCE_VIRTMIC, None);
        let mut graph = Graph::new(backend, clean_native(), state_path.clone());
        // State file absent: the first reconcile adopts the exact-args module
        // and the name-matched node, and persists both.
        assert!(
            graph
                .reconcile(&passthroughs_off())
                .expect("reconcile")
                .is_noop()
        );
        assert!(state_path.exists(), "state file must be persisted");
        let persisted = ModuleState::load(&state_path);
        assert_eq!(persisted.modules.len(), 1);
        assert_eq!(persisted.native_nodes.len(), 1);
        assert!(graph.doctor(&passthroughs_off()).all_passed());
        let torn = graph.teardown().expect("teardown");
        assert_eq!(torn.changes.len(), 4); // module + 2 links + node
        // After teardown the persisted state is empty.
        assert_eq!(ModuleState::load(&state_path), ModuleState::empty());
    }
}
