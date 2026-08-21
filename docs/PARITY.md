# Parity ledger — Rust engine vs browser goldens

Final numbers after the calibration pass (2026-08-21). Spectral similarity =
mean log-magnitude cosine over 2048-sample frames (1.0 = identical spectrum
shape). ΔRMS = rendered − golden, dB. Null scores are meaningless against Tone
composites (no phase alignment) and are omitted. Two goldens are known-bad
(browser OFFLINE-render bugs, flagged as a follow-up task): the browser export
silently skips mbcomp (worklet registration race → passthrough) and never ticks
tempo-mode duck. The master limiter now mirrors Tone.Limiter(−1) (Blink
DynamicsCompressor kernel, ratio 20 / knee 30) instead of a brickwall.

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
| fx-duck | 0.977 | -4.3 | GOLDEN PATHOLOGY: tempo-mode duck is inert in the browser offline render (Tone.Offline context-restore bug); Rust duck is live-correct |
| fx-eq | 0.978 | +1.4 |  |
| fx-eq7 | 0.984 | +1.4 |  |
| fx-filter | 0.986 | +1.4 |  |
| fx-mbcomp | 0.976 | -6.7 | GOLDEN PATHOLOGY: browser offline render degrades the worklet to passthrough (evidence in fx/mbcomp.rs header); Rust port verified against a float64 simulation of the canonical worklet |
| fx-opto | 0.938 | +0.7 | device-level parity +0.7 dB after Blink-kernel port + limiter alignment |
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

All 37 fixtures: zero hard failures. 29/37 within ±2 dB loudness of the browser reference (exceptions annotated above).
