// Shared enumeration of a track's automatable parameters, used by both the
// per-clip automation lane (piano roll) and the arrangement automation lanes.
// paramId format "dest|fxId|pkey" matches the engine's resolveTarget addressing.

import { trackById, masterFx, meta } from '../state/doc'
import { instSchema, fxSchema, mixSpec, LFO_PARAMS, ParamSpec, normFromSpec } from './schema'
import * as Y from 'yjs'

export type AutoTarget = { key: string; label: string }

const SEND_SPEC = (label: string): ParamSpec => ({ key: 'send', label, min: 0, max: 1, def: 0, fmt: (v: number) => `${Math.round(v * 100)}%` })

// The master bus automates its volume + its master-fx params (no pan / sends / inst).
function masterAutoTargets(): AutoTarget[] {
  const out: AutoTarget[] = [{ key: 'mix||gain', label: 'Volume' }]
  masterFx.forEach((f: Y.Map<any>) => {
    fxSchema(f.get('type')).params.filter(p => !p.steps).forEach(p =>
      out.push({ key: `fx|${f.get('id')}|${p.key}`, label: `${fxSchema(f.get('type')).label} · ${p.label}` }))
  })
  return out
}

export function autoTargets(trackId: string | null): AutoTarget[] {
  if (!trackId) return []
  if (trackId === 'master') return masterAutoTargets()
  const t = trackById(trackId); if (!t) return []
  const out: AutoTarget[] = []
  out.push({ key: 'mix||gain', label: 'Volume' }, { key: 'mix||pan', label: 'Pan' })
  out.push({ key: 'send||A', label: 'Send A' }, { key: 'send||B', label: 'Send B' })
  // the Feedback bus's locked self-send (the ↺ loop level) is automatable
  if (t.get('send') === 'F') out.push({ key: 'send||F', label: 'Feedback send' })
  if (t.get('kind') !== 'drum' && t.get('kind') !== 'bus' && t.get('kind') !== 'audio') {
    instSchema(t.get('inst').get('type')).params.filter(p => !p.steps).forEach(p =>
      out.push({ key: `inst||${p.key}`, label: `Inst · ${p.label}` }))
  }
  ;(t.get('fx') as any).forEach((f: any) => {
    fxSchema(f.get('type')).params.filter((p: any) => !p.steps).forEach((p: any) =>
      out.push({ key: `fx|${f.get('id')}|${p.key}`, label: `${fxSchema(f.get('type')).label} · ${p.label}` }))
  })
  // Each LFO's rate (Hz) is automatable too (only bites on a free-running LFO).
  ;(t.get('lfos') as Y.Array<Y.Map<any>> | undefined)?.forEach((l: Y.Map<any>, i: number) =>
    out.push({ key: `lfo|${l.get('id')}|hz`, label: `LFO ${i + 1} · Rate` }))
  return out
}

/** Resolve a paramId to its ParamSpec + current (manual) value, for value mapping. */
export function paramSpecAndValue(trackId: string, key: string): { spec: ParamSpec; value: number } | null {
  const [dest, fxId, pkey] = key.split('|')
  if (trackId === 'master') {
    if (dest === 'mix' && pkey === 'gain') { const spec = mixSpec('gain'); return spec ? { spec, value: (meta.get('masterGain') as number) ?? 0 } : null }
    const f = masterFx.toArray().find((m: Y.Map<any>) => m.get('id') === fxId)
    if (!f) return null
    const spec = fxSchema(f.get('type')).params.find(p => p.key === pkey); if (!spec) return null
    return { spec, value: ((f.get('params') as Y.Map<number>).get(pkey)) ?? spec.def }
  }
  const t = trackById(trackId); if (!t) return null
  if (dest === 'mix') {
    const spec = mixSpec(pkey); if (!spec) return null
    return { spec, value: (t.get(pkey) as number) ?? spec.def }
  }
  if (dest === 'send') {
    if (pkey === 'F') {
      // Feedback bus self-send — range/default mirror the ↺ Feedback knob
      return {
        spec: { ...SEND_SPEC('Feedback send'), max: 0.95, def: 0.3 },
        value: ((t.get('sends') as Y.Map<number> | undefined)?.get(trackId) as number) ?? 0.3,
      }
    }
    return { spec: SEND_SPEC(pkey === 'B' ? 'Send B' : 'Send A'), value: (t.get(pkey === 'B' ? 'sendB' : 'sendA') as number) ?? 0 }
  }
  if (dest === 'inst') {
    const spec = instSchema(t.get('inst').get('type')).params.find(p => p.key === pkey); if (!spec) return null
    return { spec, value: ((t.get('inst').get('params') as any).get(pkey) as number) ?? spec.def }
  }
  if (dest === 'lfo') {
    // another LFO's rate (Hz) — matches the engine's LFO_PARAMS[2] (log) spec
    const lfos = t.get('lfos') as Y.Array<Y.Map<any>> | undefined
    let l: Y.Map<any> | undefined
    if (lfos) for (let i = 0; i < lfos.length; i++) if (lfos.get(i).get('id') === fxId) { l = lfos.get(i); break }
    if (!l) return null
    return { spec: LFO_PARAMS[2], value: (l.get('hz') as number) ?? LFO_PARAMS[2].def }
  }
  // fx
  const fx = (t.get('fx') as any).toArray().find((f: any) => f.get('id') === fxId)
  if (!fx) return null
  const spec = fxSchema(fx.get('type')).params.find((p: any) => p.key === pkey); if (!spec) return null
  return { spec, value: ((fx.get('params') as any).get(pkey) as number) ?? spec.def }
}

/** Normalize a raw param value into [0,1] along its spec curve (log for freq/exp). */
export function normOf(spec: ParamSpec, value: number): number {
  return normFromSpec(spec, value)
}
