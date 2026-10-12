//! Pure playback policy: which handle starts, which handles die.
//!
//! Semantics (deliberate, per M2 design):
//!
//! - `interrupt_mode = true` means *per-clip* interrupt: re-clicking
//!   "airhorn" restarts airhorn (its previous playback is stopped), while
//!   *different* clips keep playing and may overlap (applause + airhorn
//!   together is fine).
//! - `interrupt_mode = false` means no interrupts at all: clicking the same
//!   clip twice plays two concurrent copies.
//! - `Stop { name: Some(n) }` kills every active handle of that clip;
//!   `Stop { name: None }` kills everything (stop-all).
//!
//! The scheduler performs no I/O: it only tracks `Handle`s that the caller
//! registers via [`Scheduler::started`] once the player actually spawned the
//! sound, and via [`Scheduler::finished`] when a clip ended on its own.
//! The [`Handle`] type is shared with the player layer, which allocates the
//! concrete ids.
//!
//! Nothing here runs in the v1 CLI yet (single-shot processes interrupt via
//! [`crate::player::PwCatPlayer::stop_external`] instead); the module is
//! fully consumed by the unit tests today and by the M3 GUI afterwards,
//! hence the module-level `allow(dead_code)` until then.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

/// Opaque id of one active playback. Allocated by the player (one per spawned
/// sound); the scheduler tracks them per clip. Construct via
/// [`Handle::from_u64`], inspect via [`Handle::as_u64`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(u64);

impl Handle {
    /// Wraps a raw id (used by the player layer and tests).
    #[must_use]
    pub const fn from_u64(raw: u64) -> Self {
        Handle(raw)
    }

    /// Raw id value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// A request to the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Start playing a clip.
    Play(PlayRequest),
    /// Stop playing (all clips, or one).
    Stop(StopRequest),
}

/// Start request: which clip, and whether re-clicking it interrupts itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    /// Registry name of the clip.
    pub name: String,
    /// Per-clip interrupt mode (see module docs).
    pub interrupt_mode: bool,
}

/// Stop request; `name: None` means stop all clips.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StopRequest {
    /// Clip to stop; `None` = stop everything.
    pub name: Option<String>,
}

/// What the caller must do to satisfy a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Start a new playback; every handle in `stop` (always of the SAME clip)
    /// must be killed before/when starting the new one.
    Start { stop: Vec<Handle> },
    /// Kill the listed handles; nothing new starts.
    Stop(Vec<Handle>),
    /// Nothing to do (e.g. stopping a clip that is not playing).
    Nothing,
}

/// Tracks which clips are playing and owns the interrupt policy.
#[derive(Debug, Default, Clone)]
pub struct Scheduler {
    /// clip name -> active handles, sorted for deterministic reports.
    playing: BTreeMap<String, BTreeSet<Handle>>,
}

impl Scheduler {
    /// Empty scheduler.
    #[must_use]
    pub const fn new() -> Self {
        Scheduler {
            playing: BTreeMap::new(),
        }
    }

    /// Decides what to do for a request and updates the internal state.
    /// For [`Decision::Start`] the caller must afterwards call
    /// [`Scheduler::started`] with the handle the player returned (or
    /// [`Scheduler::finished`] if the spawn failed).
    pub fn decide(&mut self, request: Request) -> Decision {
        match request {
            Request::Play(play) => self.decide_play(play),
            Request::Stop(stop) => self.decide_stop(stop),
        }
    }

    fn decide_play(&mut self, request: PlayRequest) -> Decision {
        let mut stop = Vec::new();
        if request.interrupt_mode
            && let Some(previous) = self.playing.get_mut(&request.name)
        {
            stop = previous.iter().copied().collect();
            previous.clear();
        }
        Decision::Start { stop }
    }

    fn decide_stop(&mut self, request: StopRequest) -> Decision {
        match request.name {
            Some(name) => match self.playing.remove(&name) {
                Some(handles) if !handles.is_empty() => {
                    Decision::Stop(handles.into_iter().collect())
                }
                _ => Decision::Nothing,
            },
            None => {
                let all: Vec<Handle> = self.playing.values().flatten().copied().collect();
                self.playing.clear();
                if all.is_empty() {
                    Decision::Nothing
                } else {
                    Decision::Stop(all)
                }
            }
        }
    }

    /// Stop-everything request (same as `decide(Request::Stop(StopRequest::default()))`).
    pub fn stop_all(&mut self) -> Decision {
        self.decide(Request::Stop(StopRequest { name: None }))
    }

    /// Records that the player successfully started `name` with `handle`.
    pub fn started(&mut self, name: &str, handle: Handle) {
        self.playing
            .entry(name.to_string())
            .or_default()
            .insert(handle);
    }

    /// Records that `handle` ended on its own; returns true when it was
    /// tracked.
    pub fn finished(&mut self, handle: Handle) -> bool {
        self.playing
            .values_mut()
            .any(|handles| handles.remove(&handle))
    }

    /// All (handle, clip name) pairs, sorted by clip then handle.
    #[must_use]
    pub fn active(&self) -> Vec<(Handle, &str)> {
        self.playing
            .iter()
            .flat_map(|(name, handles)| handles.iter().map(move |h| (*h, name.as_str())))
            .collect()
    }

    /// True when nothing is playing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.playing.iter().all(|(_, h)| h.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AIRHORN: &str = "airhorn";
    const APPLAUSE: &str = "applause";

    fn play(name: &str, interrupt: bool) -> Request {
        Request::Play(PlayRequest {
            name: name.to_string(),
            interrupt_mode: interrupt,
        })
    }

    fn stop(name: Option<&str>) -> Request {
        Request::Stop(StopRequest {
            name: name.map(str::to_string),
        })
    }

    #[test]
    fn first_play_starts_without_stops() {
        let mut s = Scheduler::new();
        let d = s.decide(play(AIRHORN, true));
        assert_eq!(d, Decision::Start { stop: vec![] });
        s.started(AIRHORN, Handle::from_u64(1));
        assert_eq!(s.active(), vec![(Handle::from_u64(1), AIRHORN)]);
        assert!(!s.is_empty());
    }

    #[test]
    fn interrupt_mode_rest_restart_stops_same_clip_only() {
        let mut s = Scheduler::new();
        s.decide(play(AIRHORN, true));
        s.started(AIRHORN, Handle::from_u64(1));
        s.decide(play(APPLAUSE, true));
        s.started(APPLAUSE, Handle::from_u64(2));

        let d = s.decide(play(AIRHORN, true));
        assert_eq!(
            d,
            Decision::Start {
                stop: vec![Handle::from_u64(1)]
            }
        );
        // The old handle is already forgotten; the caller registers the new one.
        assert_eq!(s.active(), vec![(Handle::from_u64(2), APPLAUSE)]);
        s.started(AIRHORN, Handle::from_u64(3));
        assert_eq!(
            s.active(),
            vec![
                (Handle::from_u64(3), AIRHORN),
                (Handle::from_u64(2), APPLAUSE)
            ]
        );
    }

    #[test]
    fn no_interrupt_mode_overlaps_same_clip() {
        let mut s = Scheduler::new();
        s.decide(play(AIRHORN, false));
        s.started(AIRHORN, Handle::from_u64(1));
        let d = s.decide(play(AIRHORN, false));
        assert_eq!(d, Decision::Start { stop: vec![] });
        s.started(AIRHORN, Handle::from_u64(2));
        assert_eq!(s.active().len(), 2);
    }

    #[test]
    fn stop_by_name_kills_only_that_clip() {
        let mut s = Scheduler::new();
        s.decide(play(AIRHORN, true));
        s.started(AIRHORN, Handle::from_u64(1));
        s.decide(play(APPLAUSE, true));
        s.started(APPLAUSE, Handle::from_u64(2));

        let d = s.decide(stop(Some(AIRHORN)));
        assert_eq!(d, Decision::Stop(vec![Handle::from_u64(1)]));
        assert_eq!(s.active(), vec![(Handle::from_u64(2), APPLAUSE)]);
    }

    #[test]
    fn stop_unknown_clip_is_nothing() {
        let mut s = Scheduler::new();
        assert_eq!(s.decide(stop(Some("ghost"))), Decision::Nothing);
    }

    #[test]
    fn stop_all_kills_everything_and_resets() {
        let mut s = Scheduler::new();
        s.decide(play(AIRHORN, false));
        s.started(AIRHORN, Handle::from_u64(1));
        s.decide(play(APPLAUSE, false));
        s.started(APPLAUSE, Handle::from_u64(2));

        let d = s.decide(stop(None));
        assert_eq!(
            d,
            Decision::Stop(vec![Handle::from_u64(1), Handle::from_u64(2)])
        );
        assert!(s.is_empty());
        assert_eq!(s.decide(stop(None)), Decision::Nothing);
    }

    #[test]
    fn interrupt_without_registered_handle_still_starts() {
        // Re-click before the player ever registered (e.g. spawn failed).
        let mut s = Scheduler::new();
        assert_eq!(
            s.decide(play(AIRHORN, true)),
            Decision::Start { stop: vec![] }
        );
    }

    #[test]
    fn finished_removes_exactly_that_handle() {
        let mut s = Scheduler::new();
        s.decide(play(AIRHORN, false));
        s.started(AIRHORN, Handle::from_u64(1));
        s.started(AIRHORN, Handle::from_u64(2));
        assert!(s.finished(Handle::from_u64(1)));
        assert_eq!(s.active(), vec![(Handle::from_u64(2), AIRHORN)]);
        assert!(!s.finished(Handle::from_u64(1)));
        assert!(s.finished(Handle::from_u64(2)));
        assert!(s.is_empty());
    }

    #[test]
    fn handle_display_and_raw_roundtrip() {
        let h = Handle::from_u64(42);
        assert_eq!(h.as_u64(), 42);
        assert_eq!(h.to_string(), "#42");
    }
}
