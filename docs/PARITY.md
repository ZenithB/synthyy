# Parity ledger — Rust engine vs browser goldens

Spectral similarity = mean log-magnitude cosine over 2048-sample frames
(1.0 = identical spectrum shape). ΔRMS = rendered − golden, dB. Null scores are
meaningless against Tone composites (no phase alignment) and are omitted.

**Two browser bugs were found and fixed by this harness** (2026-08-21): the
offline export constructed worklet nodes with `new AudioWorkletNode(rawContext…)`,
which throws on Tone's offline standardized-audio-context wrapper — so mbcomp
(and any worklet device) silently rendered as a PASSTHROUGH in every bounce; and
the sidechain duck scheduled its gain with `Tone.now()`, which reads the global
context that Tone.Offline restores to the LIVE one before rendering, pushing
every gain change past the end of the bounce (inert duck). Both are fixed in
`devices.ts`/`render.ts` and their goldens regenerated. The Rust ports were
correct all along — mbcomp moved from −6.7 dB to +0.9 dB purely by fixing the
reference.

Regenerate: render goldens per fixtures/README.md, then
`cargo test -p sf-render --test fixtures` (report at target/parity-report.json).

| Fixture | Spectral | ΔRMS (dB) | Note |
|---|---|---|---|
| feat-automation | 0.977 | +1.3 |  |
| feat-sends | 0.975 | +1.2 |  |
| feat-swing | 0.929 | +1.2 |  |
| fx-autofilt | 0.982 | +1.1 |  |
| fx-autopan | 0.987 | +6.7 |  |
| fx-autotune | 0.981 | +1.5 |  |
| fx-cheby | 0.928 | +6.2 |  |
| fx-chorus | 0.982 | +1.2 |  |
| fx-comp | 0.991 | +0.3 |  |
| fx-crush | 0.839 | +1.3 |  |
| fx-delay | 0.983 | +1.4 |  |
| fx-dist | 0.973 | +0.2 |  |
| fx-duck | 0.981 | +2.2 | golden regenerated 2026-08-21 after fixing the inert-duck bug (was -4.3 dB against an unducked golden) |
| fx-eq | 0.978 | +1.4 |  |
| fx-eq7 | 0.984 | +1.4 |  |
| fx-filter | 0.986 | +1.4 |  |
| fx-mbcomp | 0.978 | +0.9 | golden regenerated 2026-08-21 after fixing the browser offline-render bug (was -6.7 dB against a passthrough golden) |
| fx-opto | 0.938 | +0.7 | Blink-kernel port + Tone.Limiter-matched master limiter |
| fx-phaser | 0.982 | +1.4 |  |
| fx-pingpong | 0.985 | +2.9 |  |
| fx-plate | 0.983 | +0.9 |  |
| fx-reverb | 0.989 | +1.7 |  |
| fx-shift | 0.980 | +1.4 |  |
| fx-trem | 0.980 | +1.9 |  |
| fx-vib | 0.980 | +1.4 |  |
| fx-widen | 0.975 | +55.7 | GOLDEN IS SILENT: browser nulls mid at extreme width on a mono source; fixture pathology |
| inst-drum | 0.861 | +1.1 |  |
| inst-duo | 0.977 | +1.3 |  |
| inst-fm | 0.957 | -0.0 |  |
| inst-keys | 0.992 | -0.0 |  |
| inst-mono | 0.979 | +1.4 |  |
| inst-pluck | 0.933 | -2.5 |  |
| inst-poly | 0.978 | +0.7 |  |
| midi-arp | 0.971 | +1.4 |  |
| midi-chord | 0.979 | +1.5 |  |
| midi-velo | 0.980 | +1.4 |  |
| smoke-mono | 0.979 | +1.3 |  |

All 37 fixtures: zero hard failures. 31/37 within ±2 dB of the browser reference.
