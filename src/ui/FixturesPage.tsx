// Dev-only /#fixtures page: renders every golden-fixture project to WAV via
// the existing export path, one visible tab, real clicks (the AudioContext
// gesture requirement is satisfied by pressing the buttons). Downloads land
// in the browser's download folder; commit them under fixtures/golden/.
// See docs/RUST_REWRITE_PLAN.md §8a and src/audio/fixtures.ts.

import React, { useMemo, useState } from 'react'
import { allFixtures, renderFixtureWav, Fixture } from '../audio/fixtures'
import { download } from '../audio/encode'

type Status = 'idle' | 'rendering' | 'done' | 'error'

export function FixturesPage() {
  const fixtures = useMemo(allFixtures, [])
  const [status, setStatus] = useState<Record<string, Status>>({})
  const [errors, setErrors] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState(false)

  const renderOne = async (f: Fixture) => {
    setStatus(s => ({ ...s, [f.name]: 'rendering' }))
    try {
      const wav = await renderFixtureWav(f)
      download(wav, `${f.name}.wav`)
      setStatus(s => ({ ...s, [f.name]: 'done' }))
    } catch (e) {
      console.error(`fixture ${f.name} failed`, e)
      setErrors(er => ({ ...er, [f.name]: String((e as Error)?.message || e).slice(0, 200) }))
      setStatus(s => ({ ...s, [f.name]: 'error' }))
    }
  }

  const renderAll = async () => {
    setBusy(true)
    for (const f of fixtures) await renderOne(f)   // serial: renders share the doc
    setBusy(false)
  }

  const mark = (st: Status | undefined) =>
    st === 'rendering' ? '⏳' : st === 'done' ? '✓' : st === 'error' ? '✗' : ''

  return (
    <div style={{ maxWidth: 640, margin: '40px auto', padding: 16, fontFamily: 'system-ui, sans-serif', color: '#ddd' }}>
      <h1 style={{ fontSize: 20 }}>Golden fixtures</h1>
      <p style={{ opacity: 0.7, fontSize: 13 }}>
        Renders each fixture project to WAV through the browser engine — the reference
        files for the Rust engine's parity tests. Keep this tab visible while rendering
        (hidden tabs throttle timers). Downloads go to your browser's download folder;
        commit them under <code>fixtures/golden/</code>.
      </p>
      <button onClick={renderAll} disabled={busy || !fixtures.length} style={{ padding: '6px 14px', marginBottom: 16 }}>
        Render all ({fixtures.length})
      </button>
      {!fixtures.length && <p style={{ opacity: 0.6 }}>No fixtures found in fixtures/projects/.</p>}
      <ul style={{ listStyle: 'none', padding: 0, display: 'flex', flexDirection: 'column', gap: 6 }}>
        {fixtures.map(f => (
          <li key={f.name} style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <button onClick={() => renderOne(f)} disabled={busy || status[f.name] === 'rendering'} style={{ padding: '3px 10px' }}>
              Render
            </button>
            <code>{f.name}</code>
            <span style={{ opacity: 0.6, fontSize: 12 }}>{f.scope.kind}</span>
            <span>{mark(status[f.name])}</span>
            {status[f.name] === 'error' && <span style={{ color: '#e66', fontSize: 12 }}>{errors[f.name]}</span>}
          </li>
        ))}
      </ul>
    </div>
  )
}
