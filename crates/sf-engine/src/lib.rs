//! Graph, transport, scheduler, mixer — no I/O dependencies so the same graph
//! serves the real-time callback (sf-io) and offline export (sf-render).
//! Phase 1 (RUST_REWRITE_PLAN.md §5): track/bus/master graph with gain, pan,
//! mute/solo, A/B/per-bus sends, master fx + limiter; tick transport with
//! session/arr modes and the arr loop region; a sample-accurate event
//! scheduler with swing/humanize applied at schedule time; an `rtrb` command
//! lane with off-thread string→index resolution; meter/GR/playhead snapshots.
//!
//! Real-time rules (§4): no alloc, no locks, no syscalls in `Engine::process`
//! after the graph is built — graphs are built off-thread and handed over via
//! a ring, old ones collected by `basedrop`.

/// Test binaries run under the alloc-detecting allocator so RT-path tests
/// can wrap `Engine::process` in `assert_no_alloc`.
#[cfg(test)]
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

pub mod command;
pub mod engine;
pub mod graph;
pub mod meters;
pub mod metro;
pub mod midifx;
pub mod midimod;
pub mod modulation;
pub mod scheduler;
pub mod transport;

pub use command::{Cmd, CmdAddr, CommandResolver};
pub use engine::{Engine, EngineControl};
pub use graph::{build_graph, DuckRoute, DuckTarget, EngineGraph, SampleBank};
pub use meters::{EngineSnapshot, MeterBank, TrackMeter};
pub use midifx::{apply_midi_fx, expand_midi, ARP_DIV_TICKS};
pub use midimod::{MidiModShared, MIDI_REBUILD_SEC};
pub use modulation::LFO_DIV_TICKS;
pub use scheduler::{identity_expand, ExpandMidi, NoteEv, SchedParams, XorShift};
pub use transport::{Mode, Transport};
