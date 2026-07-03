// Shared constants & plain types used across state, audio and UI.

export const PPQ = 96                 // ticks per quarter note
export const BAR = PPQ * 4            // 4/4 fixed
export const STEP16 = PPQ / 4         // a 16th note in ticks

// A note inside a clip. p = MIDI pitch (or drum pad index 0-7 on drum tracks),
// s = start in ticks, d = duration in ticks, v = velocity 0..1, pr = probability 0..1
export type Note = { p: number; s: number; d: number; v: number; pr: number }

export type ClipRef =
  | { kind: 'session'; trackId: string; sceneId: string }
  | { kind: 'arr'; id: string }

export type TrackKind = 'synth' | 'drum' | 'audio' | 'bus'

// Palette: olive-gold primary, terracotta + teal secondaries, warm neutrals.
export const CLIP_COLORS = [
  '#FF9C87', '#D38878', '#AF513E', '#922710', '#FFF287', '#D3C978',
  '#AFA23E', '#928310', '#559DA0', '#497D80', '#26676A', '#8E8C84',
]

export const USER_COLORS = [
  '#FF9C87', '#FFF287', '#D3C978', '#AFA23E', '#559DA0', '#7FB6B9',
  '#D38878', '#E8E2C8', '#92C2C4', '#C2B86A',
]

export const DRUM_PADS = ['Kick', 'Snare', 'Clap', 'Cl Hat', 'Op Hat', 'Lo Tom', 'Perc', 'Crash']
export const NUM_PADS = DRUM_PADS.length

export const GRID_OPTIONS: { label: string; ticks: number }[] = [
  { label: '1/4', ticks: PPQ },
  { label: '1/8', ticks: PPQ / 2 },
  { label: '1/8T', ticks: PPQ / 3 },
  { label: '1/16', ticks: PPQ / 4 },
  { label: '1/16T', ticks: PPQ / 6 },
  { label: '1/32', ticks: PPQ / 8 },
]

export const LAUNCH_Q_OPTIONS = [
  { label: 'None', bars: 0 },
  { label: '1 Bar', bars: 1 },
  { label: '2 Bars', bars: 2 },
  { label: '4 Bars', bars: 4 },
]

export const ADJECTIVES = ['Funky', 'Cosmic', 'Mellow', 'Neon', 'Dusty', 'Velvet', 'Turbo', 'Lazy', 'Golden', 'Wavy', 'Hyper', 'Silky']
export const ANIMALS = ['Walrus', 'Fox', 'Panda', 'Otter', 'Falcon', 'Gecko', 'Yak', 'Moth', 'Tiger', 'Heron', 'Llama', 'Newt']

export function randomName() {
  const a = ADJECTIVES[Math.floor(Math.random() * ADJECTIVES.length)]
  const b = ANIMALS[Math.floor(Math.random() * ANIMALS.length)]
  return `${a} ${b}`
}

export function clamp(v: number, lo: number, hi: number) {
  return Math.min(hi, Math.max(lo, v))
}

/** Ticks → "bar.beat.sixteenth". Pass the current meter's bar/beat tick sizes
 *  (defaults keep the legacy 4/4 behaviour). */
export function ticksToBBS(ticks: number, barT = BAR, beatT = PPQ) {
  const bar = Math.floor(ticks / barT) + 1
  const beat = Math.floor((ticks % barT) / beatT) + 1
  const six = Math.floor((ticks % beatT) / STEP16) + 1
  return `${bar}.${beat}.${six}`
}
