// MIDI-effect note processing, shared by the live engine and the offline
// renderer so they can't drift. Operates on plain note/params data (no Yjs),
// expanding a clip's notes through the track's Scale/Chord/Arp/Velocity/Random
// device chain before the instrument is triggered.

import { Note, clamp } from '../types'
import { snapToScale, getScale } from '../theory'
import { ARP_DIV_TICKS } from './schema'

export type MidiFxData = { type: string; on: boolean; params: Record<string, number> }

const BAR = 384

/** The ascending pitch set the arp cycles through, before direction is applied.
 *  `pat` picks how each held note generates pitches:
 *    0 Octave — the held notes themselves, stacked across `oct` octaves.
 *    1 5ths   — each held note plus its perfect 5th, stacked across octaves.
 *    2 Scale  — walk the project scale upward from the lowest held note for
 *               `oct` octaves (other held notes ignored — one line up the scale).
 *    3 Select — each held note plus every semitone toggled in `sel` (a 12-bit
 *               mask, bit i = +i semitones), stacked across octaves. */
function arpSequence(held: number[], pat: number, oct: number, sel: number, root: number, scaleId: string): number[] {
  if (pat === 2) {
    const ivs = getScale(scaleId).ivs
    const s0 = snapToScale(held[0], root, scaleId)
    const rel0 = (((s0 - root) % 12) + 12) % 12
    let deg = ivs.indexOf(rel0); if (deg < 0) deg = 0
    const baseRoot = s0 - rel0                 // the scale root in s0's octave
    const seq: number[] = []
    const count = ivs.length * oct
    for (let k = 0; k < count; k++) {
      const idx = deg + k
      seq.push(baseRoot + ivs[idx % ivs.length] + 12 * Math.floor(idx / ivs.length))
    }
    return seq
  }
  const offsets = pat === 1 ? [0, 7]
    : pat === 3 ? Array.from({ length: 12 }, (_, i) => i).filter(i => sel & (1 << i))
    : [0]
  if (!offsets.length) offsets.push(0)         // Select with nothing on → the note itself
  const one: number[] = []
  held.forEach(h => offsets.forEach(o => one.push(h + o)))
  const seq: number[] = []
  for (let o = 0; o < oct; o++) one.forEach(pp => seq.push(pp + o * 12))
  return [...new Set(seq)].sort((a, b) => a - b)
}

export function arpExpand(notes: Note[], p: Record<string, number>, _loopLen: number, root = 0, scaleId = 'major'): Note[] {
  if (notes.length === 0) return notes
  const step = ARP_DIV_TICKS[Math.max(0, Math.min(ARP_DIV_TICKS.length - 1, (p.rate ?? 0) | 0))] || 48
  const mode = (p.mode ?? 0) | 0     // direction: 0 up · 1 down · 2 up-down · 3 random
  const pat = (p.pat ?? 0) | 0       // pitch pattern (see arpSequence)
  const oct = Math.max(1, p.oct ?? 1)
  const gate = p.gate ?? 0.8
  const sel = (p.sel ?? 1) | 0
  const groups = new Map<number, Note[]>()
  notes.forEach(n => { const g = groups.get(n.s) ?? []; g.push(n); groups.set(n.s, g) })
  const out: Note[] = []
  groups.forEach(g => {
    const end = Math.max(...g.map(n => n.s + n.d))
    const held = [...new Set(g.map(n => n.p))].sort((a, b) => a - b)
    let seq = arpSequence(held, pat, oct, sel, root, scaleId)
    if (mode === 1) seq = seq.reverse()
    else if (mode === 2 && seq.length > 2) seq = [...seq, ...seq.slice(1, -1).reverse()]
    let i = 0
    for (let t = g[0].s; t < end; t += step) {
      const pitch = mode === 3 ? seq[Math.floor(Math.random() * seq.length)] : seq[i % seq.length]
      out.push({ p: clamp(pitch, 0, 127), s: t, d: Math.max(6, step * gate), v: g[0].v, pr: g[0].pr })
      i++
    }
  })
  return out
}

/** Run a clip's notes through the track's MIDI-fx chain. */
export function applyMidiFx(
  chain: MidiFxData[] | undefined,
  notes: Note[],
  loopLen: number,
  root: number,
  scaleId: string,
  isDrum: boolean,
): Note[] {
  if (!chain || chain.length === 0) return notes
  let out = notes
  chain.forEach(d => {
    if (!d.on) return
    const p = d.params || {}
    if (d.type === 'scale' && !isDrum) {
      out = out.map(n => ({ ...n, p: snapToScale(n.p, root, scaleId) }))
    } else if (d.type === 'chord' && !isDrum) {
      const ivs = [0, p.i1 ?? 0, p.i2 ?? 0, p.i3 ?? 0].filter((v, i) => i === 0 || v !== 0)
      const next: Note[] = []
      out.forEach(n => ivs.forEach(iv => next.push({ ...n, p: clamp(n.p + iv, 0, 127), v: iv === 0 ? n.v : n.v * 0.85 })))
      out = next
    } else if (d.type === 'velo') {
      const s = p.scale ?? 1, r = p.rand ?? 0
      out = out.map(n => ({ ...n, v: clamp(n.v * s + (Math.random() * 2 - 1) * r, 0.05, 1) }))
    } else if (d.type === 'rand') {
      const ch = p.chance ?? 1, oc = p.octave ?? 0
      out = out.flatMap(n => {
        if (ch < 1 && Math.random() > ch) return []
        let pitch = n.p
        if (oc > 0 && Math.random() < oc) pitch = clamp(pitch + (Math.random() < 0.5 ? 12 : -12), 0, 127)
        return [{ ...n, p: pitch }]
      })
    } else if (d.type === 'arp' && !isDrum) {
      out = arpExpand(out, p, loopLen || BAR, root, scaleId)
    }
  })
  return out
}
