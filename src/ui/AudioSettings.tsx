// Audio Settings: the CPU/latency tradeoff in the user's hands. Bigger buffer =
// fewer glitches when many tracks play; Native (no 2x oversampling) ~halves CPU.
// Changing the AudioContext means a rebuild, so Apply reloads the page (project
// is autosaved). Mirrors ExportDialog's markup so it looks native.

import React, { useEffect, useState } from 'react'
import { getAudioPrefs, setAudioPrefs, AudioPrefs, LatencyMode } from '../audio/prefs'
import { engine } from '../audio/engine'
import {
  isDesktop, nativeAudio, nativeRecord, nativeOutputPrefs, setNativeOutputPrefs,
  NativeDevice, NativeHost, NativeStats, NativeStreamInfo, NativeLatency,
} from '../audio/native'
// desktop-only panel: talks to the concrete native engine (stream ownership)
import { nativeEngine } from '../audio/engine-native'
import { setUI, useUI } from '../state/store'
import { Icon } from './icons'

const TIERS: { id: LatencyMode; label: string; hint: string }[] = [
  { id: 'interactive', label: 'Low',      hint: 'Smallest buffer — tightest timing for playing in live.' },
  { id: 'balanced',    label: 'Balanced', hint: 'Medium buffer — a sensible middle ground.' },
  { id: 'playback',    label: 'Stable',   hint: 'Largest buffer — most headroom, best for many tracks/effects. Adds a little latency.' },
]
const RATES: { id: AudioPrefs['sampleRate']; label: string }[] = [
  { id: 'auto', label: 'Auto' }, { id: 44100, label: '44.1k' }, { id: 48000, label: '48k' },
]

export function AudioSettings() {
  const open = useUI(s => s.audioSettingsOpen)
  const [p, setP] = useState<AudioPrefs>(getAudioPrefs)
  // re-sync pending state from disk each time the dialog is opened
  useEffect(() => { if (open) setP(getAudioPrefs()) }, [open])
  if (!open) return null

  const saved = getAudioPrefs()
  const changed = JSON.stringify(p) !== JSON.stringify(saved)
  const close = () => setUI({ audioSettingsOpen: false })
  const apply = () => { setAudioPrefs(p); location.reload() }
  const optimize = () => setP({ ...p, oversample: false, latency: 'playback' })

  const liveRate = engine.sampleRate ? `${Math.round(engine.sampleRate / 100) / 10} kHz` : '—'
  const lat = engine.outputLatencyMs()
  const liveLat = lat ? `~${lat} ms` : '—'

  return (
    <div className="modal-backdrop" onClick={close}>
      <div className="modal export-dialog" onClick={e => e.stopPropagation()}>
        <div className="modal-head">
          <span className="modal-title"><Icon name="tools" size={15} /> Audio settings</span>
          <button className="icon-btn" onClick={close} data-info="Close"><Icon name="close" size={13} /></button>
        </div>

        <div className="export-row">
          <span className="export-label">Buffer size</span>
          <div className="seg">
            {TIERS.map(t => (
              <button key={t.id} className={p.latency === t.id ? 'on' : ''} onClick={() => setP({ ...p, latency: t.id })}>{t.label}</button>
            ))}
          </div>
        </div>
        <div className="export-hint">{TIERS.find(t => t.id === p.latency)?.hint} A bigger buffer is the fix for audio dropping out when track count climbs.</div>

        <div className="export-divider" />

        <div className="export-row">
          <span className="export-label">Quality</span>
          <div className="seg">
            <button className={p.oversample ? 'on' : ''} onClick={() => setP({ ...p, oversample: true })}>2× oversample</button>
            <button className={!p.oversample ? 'on' : ''} onClick={() => setP({ ...p, oversample: false })}>Native</button>
          </div>
        </div>
        <div className="export-hint">2× oversampling keeps FM &amp; distortion alias-free but roughly doubles CPU. <b>Native</b> frees up the most processing for more tracks and effects.</div>

        {!p.oversample && (
          <div className="export-row">
            <span className="export-label">Sample rate</span>
            <div className="seg">
              {RATES.map(r => (
                <button key={String(r.id)} className={p.sampleRate === r.id ? 'on' : ''} onClick={() => setP({ ...p, sampleRate: r.id })}>{r.label}</button>
              ))}
            </div>
          </div>
        )}

        <div className="export-divider" />

        <div className="export-row">
          <span className="export-label">Now running</span>
          <span className="export-unit">{liveRate} · {liveLat} output latency</span>
        </div>

        {isDesktop() && <NativeAudioSection />}

        <div className="export-actions">
          <button className="export-btn" onClick={optimize}><Icon name="bolt" size={13} /> Optimize for fewer glitches</button>
        </div>

        <div className="export-divider" />

        <div className="export-actions">
          <button className="export-btn record" disabled={!changed} onClick={apply}><Icon name="power" size={13} /> Apply &amp; restart audio</button>
          <button className="export-btn ghost" onClick={close}>Cancel</button>
        </div>
        <div className="export-hint">Applying reloads the page to rebuild the audio engine — your project is saved automatically.</div>
      </div>
    </div>
  )
}

// ---- desktop-only: direct hardware access via the Rust core ----
// Driver type (CoreAudio / WASAPI / ASIO) + output device + buffer for the
// native engine, remembered per machine (nativeOutputPrefs). Applied when the
// user presses Apply or closes the dialog (the engine stream is re-opened and
// the project re-pushed). Live callback stats come straight from the audio
// callback: the callback size actually seen, xruns, deadline misses, load and
// the audio thread's priority (MMCSS on Windows).
const NATIVE_BUFFERS = [64, 128, 256, 512]

function NativeAudioSection() {
  const saved = nativeOutputPrefs()
  const [hosts, setHosts] = useState<NativeHost[]>([])
  const [host, setHost] = useState<string | null>(saved.host)
  const [devices, setDevices] = useState<NativeDevice[]>([])
  const [device, setDevice] = useState<string | null>(saved.device)
  const [buffer, setBuffer] = useState<number | null>(saved.buffer)
  const [outFirst, setOutFirst] = useState<number>(saved.outFirst ?? 0)
  const [stream, setStream] = useState<NativeStreamInfo | null>(null)
  const [stats, setStats] = useState<NativeStats | null>(null)
  const [err, setErr] = useState('')
  const [applied, setApplied] = useState('')

  // the driver actually meant by "null" (platform default)
  const hostName = host ?? hosts.find(h => h.is_default)?.name ?? null
  const isWasapi = hostName?.toUpperCase() === 'WASAPI'
  const isAsio = hostName?.toUpperCase() === 'ASIO'
  const hasAsio = hosts.some(h => h.name.toUpperCase() === 'ASIO')

  useEffect(() => { nativeAudio.hosts().then(setHosts).catch(() => setHosts([])) }, [])
  useEffect(() => {
    if (!hostName) return
    nativeAudio.devices(hostName).then(setDevices).catch(() => setDevices([]))
  }, [hostName])
  useEffect(() => {
    const t = setInterval(() => nativeAudio.stats().then(setStats).catch(() => {}), 500)
    return () => clearInterval(t)
  }, [])
  // Releasing the device must hand the stream BACK to the engine — the test
  // tone replaced it, so skipping this leaves the app silent until restart.
  // This is also where the remembered output choice takes effect on close.
  useEffect(() => () => { void releaseDevice() }, [])

  const save = (next: { host?: string | null; device?: string | null; buffer?: number | null; outFirst?: number | null }) => {
    const p = { host, device, buffer, outFirst, ...next }
    setNativeOutputPrefs(p)
    setApplied('')
  }
  const pickHost = (h: string | null) => {
    // device names belong to a driver type — reset to its default
    setHost(h); setDevice(null); setOutFirst(0); save({ host: h, device: null, outFirst: 0 })
  }
  // a new device starts on outputs 1–2 (its pairs differ)
  const pickDevice = (d: string | null) => { setDevice(d); setOutFirst(0); save({ device: d, outFirst: 0 }) }
  const pickOutFirst = (f: number) => { setOutFirst(f); save({ outFirst: f }) }
  // output pairs of the selected (or default) device: 1–2, 3–4, …
  const outChannels = (devices.find(d => d.name === device) ?? devices.find(d => d.is_default))?.channels ?? 0
  const outPairs = Array.from({ length: Math.floor(outChannels / 2) }, (_, i) => i * 2)
  const pickBuffer = (b: number | null) => { setBuffer(b); save({ buffer: b }) }

  const start = async () => {
    setErr('')
    try { setStream(await nativeAudio.startTone(device, buffer ?? 128, hostName, outFirst)) }
    catch (e) { setErr(String(e).slice(0, 160)); setStream(null) }
  }
  const releaseDevice = async () => {
    try { await nativeAudio.stop() } catch { /* ok */ }
    try { await nativeEngine.restartStream() } catch { /* engine not started yet */ }
  }
  const stop = () => { void releaseDevice(); setStream(null) }
  const apply = async () => {
    setErr('')
    setStream(null)
    try {
      await nativeAudio.stop().catch(() => {})
      await nativeEngine.restartStream()
      setApplied('Playback now uses these settings.')
    } catch (e) { setErr(String(e).slice(0, 160)) }
  }

  const rate = stream?.sample_rate ?? nativeEngine.sampleRate
  const ms = (frames: number) => rate ? ` (${(frames / rate * 1000).toFixed(1)} ms)` : ''
  const seen = stats?.max_callback_frames
    ? (stats.min_callback_frames && stats.min_callback_frames !== stats.max_callback_frames
        ? `${stats.min_callback_frames}–${stats.max_callback_frames} frames${ms(stats.max_callback_frames)}`
        : `${stats.max_callback_frames} frames${ms(stats.max_callback_frames)}`)
    : null
  const prio = stats?.rt_priority === 'mmcss' ? 'real-time (MMCSS)'
    : stats?.rt_priority === 'mmcss-failed' ? 'NORMAL — MMCSS registration failed'
    : null

  return (
    <>
      <div className="export-divider" />
      {hosts.length > 1 && (
        <div className="export-row">
          <span className="export-label">Driver</span>
          <div className="seg">
            {hosts.map(h => (
              <button key={h.name} className={hostName === h.name ? 'on' : ''}
                onClick={() => pickHost(h.name)}>{h.name}</button>
            ))}
          </div>
        </div>
      )}
      <div className="export-row">
        <span className="export-label">Native output</span>
        <select value={device ?? ''} onChange={e => pickDevice(e.target.value || null)}>
          <option value="">{isAsio ? 'First ASIO driver' : 'System default'}</option>
          {devices.map(d => <option key={d.name} value={d.name}>{d.name}{d.is_default ? ' ★' : ''}</option>)}
        </select>
      </div>
      {outPairs.length > 1 && (
        <div className="export-row">
          <span className="export-label">Outputs</span>
          <select value={outFirst} onChange={e => pickOutFirst(Number(e.target.value))}
            data-info="Which pair of the interface's outputs carries the mix (e.g. 3–4 for a headphone output)">
            {outPairs.map(f => <option key={f} value={f}>{f + 1}–{f + 2}</option>)}
          </select>
        </div>
      )}
      <div className="export-row">
        <span className="export-label">Native buffer</span>
        <div className="seg">
          <button className={buffer === null ? 'on' : ''} onClick={() => pickBuffer(null)}
            data-info="Follow the Latency setting above (Low 128 · Balanced 256 · Stable 512)">Auto</button>
          {NATIVE_BUFFERS.map(b => (
            <button key={b} className={buffer === b ? 'on' : ''} onClick={() => pickBuffer(b)}>{b}</button>
          ))}
        </div>
      </div>
      {isWasapi && (
        <div className="export-hint">
          WASAPI: Windows sets the buffer itself (usually about 10 ms), so the buffer choice has little effect.
          {hasAsio ? ' Choose ASIO for low latency with an audio interface.' : ' This build has no ASIO support (see docs/WINDOWS.md).'}
        </div>
      )}
      {isAsio && (
        <div className="export-hint">
          ASIO: the recording input uses the same driver. A buffer size the driver doesn’t accept falls back to the size set in its control panel.
        </div>
      )}
      <div className="export-actions">
        <button className="export-btn" onClick={apply}>Apply to playback</button>
        {!stream
          ? <button className="export-btn ghost" onClick={start}><Icon name="bolt" size={13} /> Play test tone</button>
          : <button className="export-btn ghost" onClick={stop}>Stop tone</button>}
      </div>
      {stream && (
        <div className="export-hint">
          Test tone: {stream.device}{stream.host ? ` [${stream.host}]` : ''} @ {(stream.sample_rate / 1000).toFixed(1)} kHz
          {stream.channels ? ` · ${stream.channels} ch ${stream.sample_format ?? ''}` : ''}
        </div>
      )}
      {stats && stats.callbacks > 0 && (
        <div className="export-hint">
          {seen && <>Callback <b>{seen}</b> · </>}
          xruns <b>{stats.xruns}</b> · deadline misses <b>{stats.over_budget}</b> · peak load <b>{Math.round(stats.peak_load * 100)}%</b>
          {prio && <> · priority <b style={stats.rt_priority === 'mmcss-failed' ? { color: '#e66' } : undefined}>{prio}</b></>}
        </div>
      )}
      {applied && <div className="export-hint">{applied}</div>}
      {err && <div className="export-hint" style={{ color: '#e66' }}>{err}</div>}
      <div className="export-hint">Direct hardware access via the Rust audio core. Output settings take effect when you press Apply or close this dialog; the settings above only affect the browser engine.</div>
      <NativeInputSection host={hostName} />
    </>
  )
}

// ---- desktop-only: recording input + loopback latency calibration (Phase 6) ----
// The input stream is separate from the output stream and is opened on demand
// (here, or by the first track record). "Measure latency" plays a short chirp
// through the engine output and finds it again on the input — connect a cable
// from an output to an input, or let the speakers reach the mic.
const INPUT_KEY = 'sf-native-input'

function NativeInputSection({ host }: { host: string | null }) {
  const [devices, setDevices] = useState<NativeDevice[]>([])
  const [device, setDevice] = useState<string | null>(() => localStorage.getItem(INPUT_KEY) || null)
  const [stream, setStream] = useState<NativeStreamInfo | null>(null)
  const [lat, setLat] = useState<NativeLatency | null>(null)
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState('')

  const refreshLat = () => nativeRecord.latencyGet().then(setLat).catch(() => {})
  // inputs list follows the output's driver type (ASIO runs both on one driver)
  useEffect(() => {
    nativeRecord.inputDevices(host).then(setDevices).catch(() => setDevices([]))
  }, [host])
  useEffect(() => {
    nativeRecord.inputInfo().then(setStream).catch(() => {})
    void refreshLat()
  }, [])

  const open = async (name: string | null) => {
    setMsg('')
    try {
      setStream(await nativeRecord.inputStart(name, null))
      void refreshLat()
    } catch (e) { setMsg(String(e).slice(0, 120)); setStream(null) }
  }
  const pick = (name: string | null) => {
    setDevice(name)
    if (name) localStorage.setItem(INPUT_KEY, name); else localStorage.removeItem(INPUT_KEY)
    void open(name)
  }
  const measure = async () => {
    setBusy(true); setMsg('')
    try {
      if (!stream) await open(device)
      const ms = await nativeRecord.latencyMeasure()
      setMsg(`Measured round trip: ${ms.toFixed(1)} ms`)
      void refreshLat()
    } catch (e) { setMsg(String(e).slice(0, 140)) }
    setBusy(false)
  }
  const reset = async () => { setLat(await nativeRecord.latencySet(null).catch(() => null)); setMsg('') }

  return (
    <>
      <div className="export-divider" />
      <div className="export-row">
        <span className="export-label">Native input</span>
        <select value={device ?? ''} onChange={e => pick(e.target.value || null)}>
          <option value="">System default</option>
          {devices.map(d => <option key={d.name} value={d.name}>{d.name}{d.is_default ? ' ★' : ''}</option>)}
        </select>
      </div>
      <div className="export-actions">
        {!stream && <button className="export-btn" onClick={() => open(device)}>Open input</button>}
        <button className="export-btn" disabled={busy} onClick={measure}><Icon name="bolt" size={13} /> {busy ? 'Measuring…' : 'Measure latency'}</button>
        {lat?.measured_roundtrip_ms != null && <button className="export-btn ghost" onClick={reset}>Reset</button>}
      </div>
      {stream && (
        <div className="export-hint">
          {stream.device}{stream.host ? ` [${stream.host}]` : ''} @ {(stream.sample_rate / 1000).toFixed(1)} kHz
          {stream.channels ? ` · ${stream.channels} ch` : ''}
          {stream.buffer_frames ? ` · ${stream.buffer_frames} frames` : ' · driver-managed buffer'}
        </div>
      )}
      {lat && (
        <div className="export-hint">
          Record compensation <b>{lat.compensation_ms.toFixed(1)} ms</b>
          {lat.measured_roundtrip_ms != null
            ? <> · measured round trip {lat.measured_roundtrip_ms.toFixed(1)} ms</>
            : <> · unmeasured (buffer estimate {lat.estimate_ms.toFixed(1)} ms + stream timestamps)</>}
        </div>
      )}
      {msg && <div className="export-hint" style={msg.startsWith('Measured') ? undefined : { color: '#e66' }}>{msg}</div>}
      <div className="export-hint">Measure with a cable from an output to an input (or speakers near the mic): a short chirp is played and timed on the way back. Recorded takes are shifted by the compensation automatically.</div>
    </>
  )
}
