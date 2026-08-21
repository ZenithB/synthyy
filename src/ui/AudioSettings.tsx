// Audio Settings: the CPU/latency tradeoff in the user's hands. Bigger buffer =
// fewer glitches when many tracks play; Native (no 2x oversampling) ~halves CPU.
// Changing the AudioContext means a rebuild, so Apply reloads the page (project
// is autosaved). Mirrors ExportDialog's markup so it looks native.

import React, { useEffect, useState } from 'react'
import { getAudioPrefs, setAudioPrefs, AudioPrefs, LatencyMode } from '../audio/prefs'
import { engine } from '../audio/engine'
import { isDesktop, nativeAudio, nativeRecord, NativeDevice, NativeStats, NativeStreamInfo, NativeLatency } from '../audio/native'
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

// ---- desktop-only: direct hardware access via the Rust core (Phase 0) ----
// Device picker + real buffer sizes + a test tone, with live callback stats
// (xruns, deadline misses, peak load) straight from the audio callback.
const NATIVE_BUFFERS = [64, 128, 256, 512]

function NativeAudioSection() {
  const [devices, setDevices] = useState<NativeDevice[]>([])
  const [device, setDevice] = useState<string | null>(null)
  const [buffer, setBuffer] = useState(128)
  const [stream, setStream] = useState<NativeStreamInfo | null>(null)
  const [stats, setStats] = useState<NativeStats | null>(null)
  const [err, setErr] = useState('')

  useEffect(() => { nativeAudio.devices().then(setDevices).catch(() => setDevices([])) }, [])
  useEffect(() => {
    if (!stream) return
    const t = setInterval(() => nativeAudio.stats().then(setStats).catch(() => {}), 500)
    return () => clearInterval(t)
  }, [stream])
  // leaving the dialog unmounts the section — stop any running tone
  useEffect(() => () => { nativeAudio.stop().catch(() => {}) }, [])

  const start = async () => {
    setErr('')
    try { setStream(await nativeAudio.startTone(device, buffer)) }
    catch (e) { setErr(String(e).slice(0, 120)); setStream(null) }
  }
  const stop = () => { nativeAudio.stop().catch(() => {}); setStream(null); setStats(null) }

  return (
    <>
      <div className="export-divider" />
      <div className="export-row">
        <span className="export-label">Native output</span>
        <select value={device ?? ''} onChange={e => setDevice(e.target.value || null)}>
          <option value="">System default</option>
          {devices.map(d => <option key={d.name} value={d.name}>{d.name}{d.is_default ? ' ★' : ''}</option>)}
        </select>
      </div>
      <div className="export-row">
        <span className="export-label">Native buffer</span>
        <div className="seg">
          {NATIVE_BUFFERS.map(b => (
            <button key={b} className={buffer === b ? 'on' : ''} onClick={() => setBuffer(b)}>{b}</button>
          ))}
        </div>
      </div>
      <div className="export-actions">
        {!stream
          ? <button className="export-btn" onClick={start}><Icon name="bolt" size={13} /> Play test tone</button>
          : <button className="export-btn ghost" onClick={stop}>Stop tone</button>}
      </div>
      {stream && (
        <div className="export-hint">
          {stream.device} @ {(stream.sample_rate / 1000).toFixed(1)} kHz
          {stream.buffer_frames ? ` · ${stream.buffer_frames} frames (${(stream.buffer_frames / stream.sample_rate * 1000).toFixed(1)} ms)` : ' · default buffer'}
          {stats && <> · xruns <b>{stats.xruns}</b> · deadline misses <b>{stats.over_budget}</b> · peak load <b>{Math.round(stats.peak_load * 100)}%</b></>}
        </div>
      )}
      {err && <div className="export-hint" style={{ color: '#e66' }}>{err}</div>}
      <div className="export-hint">Direct hardware access via the Rust audio core — the settings above only affect the browser engine.</div>
      <NativeInputSection />
    </>
  )
}

// ---- desktop-only: recording input + loopback latency calibration (Phase 6) ----
// The input stream is separate from the output stream and is opened on demand
// (here, or by the first track record). "Measure latency" plays a short chirp
// through the engine output and finds it again on the input — connect a cable
// from an output to an input, or let the speakers reach the mic.
const INPUT_KEY = 'sf-native-input'

function NativeInputSection() {
  const [devices, setDevices] = useState<NativeDevice[]>([])
  const [device, setDevice] = useState<string | null>(() => localStorage.getItem(INPUT_KEY) || null)
  const [stream, setStream] = useState<NativeStreamInfo | null>(null)
  const [lat, setLat] = useState<NativeLatency | null>(null)
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState('')

  const refreshLat = () => nativeRecord.latencyGet().then(setLat).catch(() => {})
  useEffect(() => {
    nativeRecord.inputDevices().then(setDevices).catch(() => setDevices([]))
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
          {stream.device} @ {(stream.sample_rate / 1000).toFixed(1)} kHz
          {stream.buffer_frames ? ` · ${stream.buffer_frames} frames` : ' · default buffer'}
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
