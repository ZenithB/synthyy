//! Command lane types: a fixed-size `Cmd` enum crossing the `rtrb` ring into
//! the audio callback, and the off-thread `CommandResolver` that turns string
//! addresses ("trackId|fxId|paramKey") into numeric ones. No `String` ever
//! crosses into the callback (RUST_ARCH.md "Command lane").

use std::collections::HashMap;

use crate::transport::Mode;

/// `target` value addressing the master strip instead of a track.
pub const TARGET_MASTER: u16 = u16::MAX;
/// `slot` value addressing the mixer strip (gain/pan/sends) of a target.
pub const SLOT_MIXER: u8 = u8::MAX;
/// `slot` value addressing the instrument. Fx devices are `SLOT_FX0 + idx`.
pub const SLOT_INST: u8 = 0;
pub const SLOT_FX0: u8 = 1;
/// `param` value addressing a device's output gain (dB) instead of a schema key.
pub const PARAM_OUT: u16 = u16::MAX;

/// Mixer param indices under `SLOT_MIXER`.
pub const MIX_GAIN: u16 = 0;
pub const MIX_PAN: u16 = 1;
pub const MIX_SEND_A: u16 = 2;
pub const MIX_SEND_B: u16 = 3;
/// Bus sends resolve to `MIX_BUS_SEND0 + position in the track's send list`.
pub const MIX_BUS_SEND0: u16 = 16;

/// Fixed-size, heap-free commands for the RT lane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    SetParam {
        target: u16,
        slot: u8,
        param: u16,
        value: f32,
    },
    NoteOn {
        track: u16,
        pitch: u8,
        vel: f32,
    },
    NoteOff {
        track: u16,
        pitch: u8,
    },
    ClipLaunch {
        track: u16,
        clip: u16,
    },
    ClipStop {
        track: u16,
    },
    TransportPlay {
        mode: Mode,
        from_ticks: f64,
    },
    TransportStop,
    SetTempo {
        bpm: f64,
    },
    MetronomeOn,
    MetronomeOff,
}

/// Numeric address of one parameter (see the `SLOT_*`/`PARAM_*` constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdAddr {
    pub target: u16,
    pub slot: u8,
    pub param: u16,
}

/// String → numeric address resolution, built alongside the graph and used
/// only off-thread (the IPC layer). Key grammar mirrors the web engine's
/// param ids:
///   "<trackId>|inst|<pkey>"    instrument param
///   "<trackId>|<fxId>|<pkey>"  fx param ("out" = the device output gain)
///   "<trackId>|mix|gain|pan|sendA|sendB"  mixer strip
///   "<trackId>|send|<busId>"   per-bus send level
///   "master|gain", "master|<fxId>|<pkey>" master strip / master fx
#[derive(Debug, Default)]
pub struct CommandResolver {
    params: HashMap<String, CmdAddr>,
    tracks: HashMap<String, u16>,
    scenes: HashMap<String, u16>,
}

impl CommandResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_param(&mut self, key: String, addr: CmdAddr) {
        self.params.insert(key, addr);
    }

    pub fn insert_track(&mut self, id: String, idx: u16) {
        self.tracks.insert(id, idx);
    }

    pub fn insert_scene(&mut self, id: String, idx: u16) {
        self.scenes.insert(id, idx);
    }

    pub fn param(&self, key: &str) -> Option<CmdAddr> {
        self.params.get(key).copied()
    }

    pub fn track(&self, id: &str) -> Option<u16> {
        self.tracks.get(id).copied()
    }

    pub fn scene(&self, id: &str) -> Option<u16> {
        self.scenes.get(id).copied()
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }
}
