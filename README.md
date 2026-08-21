# Synthyy

Standalone desktop DAW with a native Rust audio engine (Tauri 2 shell). Forked from Synthtagram (the browser app, kept separate). Start here: `docs/DESKTOP.md`, `docs/RUST_REWRITE_PLAN.md`, `docs/RUST_ARCH.md`.

---

_Original Synthtagram README follows._

# 🎛️ Synthtagram

**Make a song together.** A lightweight, Ableton-inspired DAW that lives in your browser — with Google-Docs-style real-time collaboration. Launch clips, sculpt synths, arrange a track, and share a link so friends can edit the same song *live*, note by note.

**▶ Play it now: https://synthtagram.vercel.app**

No install, no account, no server. Your project autosaves locally; sharing creates a peer-to-peer room over WebRTC.

---

## What's inside

### A real (small) DAW
- **Session View** — Ableton's signature clip grid: tracks as columns, scenes as rows, quantized clip launching so nothing ever comes in off-beat. Launch scenes with keys `1–9`.
- **Arrangement View** — a linear timeline. Drag clips around (Alt = copy), resize, set a loop region, and send clips/scenes over from the Session grid when the jam is ready to become a song.
- **Piano roll** — canvas editor with draw mode (`B`), marquee select, drag/resize/copy, per-note **velocity** and **probability** lanes, scale highlighting, snap-to-scale, fold-to-scale, and full **right-click menus**: copy/cut/paste/duplicate a selection, one-click transforms, and per-pen defaults (note length, velocity, chance) when drawing.
- **Adjustable workspace** — drag the divider to resize the editor panel, and zoom the whole interface in/out from the top bar (handy on small or huge screens); less-used controls tuck into a tidy overflow menu.
- **Mixer** — per-track fader, pan, mute, solo, arm, live meters, master limiter.
- **7 instrument modules** — Analog Poly (with fat/supersaw waves + stereo spread), FM Synth, Mono Bass, Pluck, Dream Keys, Duo Thick (detuned dual-voice with vibrato), and an 8-pad synthesized Drum Kit (no samples — every drum is synthesized live, with per-pad tune/decay/level).
- **18 effect modules** — EQ3, Filter, tempo-synced Echo & Ping Pong, Reverb, Chorus, Drive, Heat (Chebyshev), Bitcrusher, Compressor, Phaser, Auto Filter, Auto Pan, Tremolo, Vibrato, Stereo Widener, Frequency Shifter, and **Auto-Tune** (live pitch correction that snaps to the project key/scale — or chromatic — with adjustable retune speed). Reorder, bypass, stack freely per track.
- **Per-device metering** — every instrument and effect has its own level meter plus a pre-meter **output knob (−30/+30 dB)** for gain-staging device by device down the chain.
- **LFOs** (Ableton-style) — add LFOs to any track and map each to any knob (instrument or effect). 7 shapes (sine, triangle, saw up/down, square, sample & hold, random), tempo-synced rate *or* free Hz, depth + phase. The LFO modulates the parameter *around its manual value* and snaps back when unmapped — modulation runs locally per client so it never spams the network, with a deterministic S&H/random so collaborators hear the same wobble.
- **Automation** — draw parameter envelopes per clip (volume, pan, cutoff, any effect knob) right in the piano roll's bottom lane; they play back layered under any LFO on the same parameter.
- **Send / Return buses** — two shared return tracks (Reverb + Delay) with per-track A/B send knobs, so the whole mix shares one lush space instead of one reverb per track.
- **Macro racks** — 8 macro knobs per track, each mapping to many parameters; one knob morphs a whole sound, plus a Vary button for happy accidents.
- **Live MIDI effects** — Scale, Chord, Arpeggiator, Velocity and Random devices that process notes before the instrument (hold one note → arpeggio; force everything in key).
- **Sidechain pump** — a tempo-synced ducking effect for instant modern movement.
- **Sampler** — record from your mic or drop in an audio file, played back pitched across the keyboard (samples stored locally per browser).
- **Clip follow actions** — after N bars a clip can auto-trigger the next / a random / any clip, or stop — turning the Session grid into a generative, self-arranging engine.
- **Drum step sequencer** — a 16-step × 8-pad grid for drum clips, the fastest way to build a beat (toggle to the piano roll anytime).
- **Master analyzers** — a spectrum analyzer, oscilloscope and tuner on the master output (click to cycle).
- **Personal library** — save your own instrument presets and favorite any sound; they live in *My Sounds* and *Favorites* in the browser.
- **Audio clips** — import audio files (drag-and-drop or the browser button), **mono or stereo**, as clips on audio tracks. Launch/loop them in the grid, place them on the timeline, and manipulate: gain, repitch, reverse, loop, fade in/out, with a waveform view. They run through the track's full effect chain, sends and mixer.
- **Fully stereo signal path** — panning, stereo effects (Ping Pong, Widener, Auto Pan), and stereo samples are preserved end-to-end through tracks, returns, the master and the WAV export.
- **Sound packs** — a searchable browser with **~50 instrument presets** (supersaw anthems to gamelan bells), **12 drum kits** (808 Boom, Trap 808 Long, Techno Bunker, Garage 2-Step, Ambient Glass…), **40 MIDI loops** across drums/bass/chords/melody/arps (click to audition, drag onto the grid), plus a full demo song.
- **Chord progression generator** — 16 classic progressions (Axis pop, Doo-Wop, Royal Road, 12-bar blues, Andalusian, jazz ii–V–I, neo-soul vamps…) rendered **into your project's key** on drop, with roman-numeral labels and one-click audition.
- **Export** — render your arrangement, loop region, or a scene to **WAV (16-bit) or MP3 (64/128/256 kbps)**, **stereo or mono**, entirely in the browser. The offline render faithfully mirrors the live signal path — instrument & effect chains, per-device output gains, **send/return buses**, LFOs, sidechain ducking, MIDI-fx and audio clips all bounce exactly as you hear them. Export per-track **stems** (each track + each return bus, so a stem set reconstructs the mix). Save/import project files.
- **Record Output** — capture a live take of *exactly what you hear* — including knob tweaks, performance, and Auto-Tune — to WAV/MP3, then download. (Two complementary paths: the offline render is instant and deterministic; Record Output captures live modulation in real time.)

### Collaboration ("Google Docs, but it slaps")
- One click creates a share link. Everyone with the link edits the **same project in real time** — clips, notes, knobs, everything, with per-note granularity (CRDTs via Yjs, so concurrent edits merge instead of conflicting).
- **Presence**: colored avatars, "who's editing this clip" dots, and ghost playheads of your collaborators on the timeline.
- **Per-user undo** — `Ctrl+Z` only undoes *your* edits, never your friend's (just like Google Docs).
- Built-in **chat** that syncs with the project.
- **Serverless**: peers discover each other via public relays (Trystero) and then talk directly over WebRTC. Same-browser tabs sync instantly via BroadcastChannel. Everyone keeps a full local copy (IndexedDB autosave), so the project survives everyone leaving.
- Playback is intentionally local-per-person (the standard for collaborative DAWs) — the *music data* syncs live, your transport is your own.

### Audio engine
- Built on **Tone.js / Web Audio**, with the whole graph running at **88.2 kHz — 2× internal oversampling** — so FM synthesis and nonlinear effects stay clean; the browser resamples to your hardware on output. The waveshaping Drive stage additionally oversamples 4×.
- **Low-latency live playing** — keyboard/MIDI notes trigger immediately (bypassing the scheduler lookahead used for clip playback), so the instrument responds like an instrument, not a video game. The status bar shows your live output latency.
- **Audio settings** (⋯ menu) — tune the CPU/latency tradeoff: increase the **buffer size** for more headroom (fewer dropouts as track count climbs), or switch from 2× oversampling to **Native** rate to roughly halve CPU. One-click "Optimize for fewer glitches" applies both.
- WAV export renders offline at 88.2 kHz, then resamples to 44.1 kHz / 16-bit.

---

## Why these design choices? (a short study of Ableton Live)

Synthtagram is a homage. Before building, we researched what made Live the most influential DAW of its generation, and copied the *ideas*, not the pixels:

1. **The Session grid comes first.** Live's category-defining move was non-linear clip launching — "if I look at a timeline that is going to hit an end, that scares me" (Gerhard Behles). The empty state here is a grid inviting you to fill slots, not a timeline demanding a finished song.
2. **Quantized launching makes the grid a playable instrument.** Any clip triggered mid-bar waits for the boundary. This one rule means you cannot sound bad while jamming — it's the soul of the thing.
3. **One window, zero modal dialogs.** Browser left, grid center, clip/device detail in a bottom panel, everything editable while audio keeps running. Live has kept this layout for 20+ years because "knowledge you gain with the program doesn't change."
4. **Info View.** Live documents every control in a hover panel. So do we — the bar at the bottom explains whatever you point at.
5. **Drag-and-drop as the universal verb.** Presets, kits and loops drag from the browser onto tracks, slots, and the timeline. Click auditions before you commit.
6. **Two views, one data model.** The infamous Live friction is getting your Session jam into the Arrangement, so clips and scenes here have one-click "send to arrangement."
7. **Few, opinionated devices.** Live's "benign dictatorship": a handful of modules with immediate knobs beats an empty rack of infinite options.

## Wishlist features (what forum users wish their DAW did)

We mined long-standing community requests and complaints and built the feasible ones in from day one:

| Community wish | Synthtagram |
|---|---|
| Real-time collaboration | ✅ The core feature — live multiplayer projects |
| Autosave (Live has none!) | ✅ Continuous, automatic, local-first |
| Undo history you can see | ✅ History panel of labeled edits — click to rewind |
| Per-note probability (pre-Live-11 wish) | ✅ "Chance" lane in the piano roll |
| Scale awareness / snap-to-scale (pre-Live-12 wish) | ✅ Global key & scale, highlighting, snap, fold |
| Capture MIDI you forgot to record | ✅ `CAP` button — last 30 s of playing becomes a clip |
| Command palette / searchable actions | ✅ `Ctrl/Cmd+K` |
| MIDI transform tools one click away | ✅ Chordify (diatonic), arp, strum, humanize, quantize %, legato, reverse, velocity ramps |
| Stem export without bouncing manually | ✅ One-click per-track stems |
| Mixer visible in Arrangement | ✅ Mute strips on lanes; full mixer lives one Tab away |
| Tap tempo, editable launch quantize, swing | ✅ All in the top bar |
| Light theme that isn't an afterthought | ✅ One click |

(See [FEATURES.md](FEATURES.md) for the full map with sources.)

---

## Running locally

```bash
npm install
npm run dev        # http://localhost:5173
npm run build      # production build to dist/
```

Hosted on [Vercel](https://vercel.com) — pushes to `main` auto-deploy to https://synthtagram.vercel.app (Vite `base: '/'`).

## Architecture (1 minute version)

```
src/
  state/   Yjs document (the shared project), mutations, undo, P2P + tab sync
  audio/   Tone.js engine: schema-driven instrument/effect factories,
           transport & clip scheduling, offline WAV renderer, live input
  ui/      React: Session grid, Arrangement timeline, canvas piano roll,
           device rack, browser, palette, panels
  packs.ts presets / kits / MIDI loops / demo song (pure data)
```

Every edit goes through a labeled Yjs transaction → it's undoable, it appears in the history panel, and it broadcasts to peers. The audio engine *observes* the document, so a collaborator twisting a knob retunes your local audio within a frame.

## Limitations

- **Offline export quirks** (use **Record Output** for an exact bounce of these): **Auto-Tune** isn't reproduced in the offline render — its pitch detector needs a live analyser, which doesn't run in an `OfflineAudioContext`. The **Pluck** instrument renders as a close synth approximation offline (its Karplus-Strong core can't render in `OfflineAudioContext`). Both sound exactly right in Record Output, which captures the live engine.
- P2P discovery uses public relays; on very locked-down networks peers may not connect (tabs on the same machine always sync).
- **MIDI controllers need Chrome or Edge.** The Web MIDI API only exists in Chromium browsers, so a hardware controller won't be detected in Safari or Firefox — but *audio and everything else work in every modern browser* (Safari 15+, Firefox included); MIDI is not required for sound. To avoid a surprise permission prompt, the app never asks for MIDI access on load — click **Enable MIDI** in the status bar to opt in (after that it reconnects silently). If the audio engine ever fails to start, a banner says so with a Retry.
- Intended for desktop/laptop usage. Will work on phones and tablets, but not optimised.

---

Built with [Tone.js](https://tonejs.github.io/), [Yjs](https://yjs.dev/), [Trystero](https://github.com/dmotz/trystero), React and Vite. 🤖 Built with [Claude Code](https://claude.com/claude-code).
