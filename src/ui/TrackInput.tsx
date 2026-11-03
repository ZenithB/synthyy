// Audio-track input picker + record button, shown on audio-track headers in
// both Session and Arrangement. Pick any of the device's audio inputs, then
// record live audio into a clip (playhead in Arrangement, scene slot in Session).

import { useSyncExternalStore, useState } from 'react'
import {
  inputDevices, inputsVersion, subscribeInputs, refreshInputDevices, trackInput, setTrackInput,
  trackInputChannels, setTrackInputChannels, inputChannelOptions,
} from '../state/inputs'
import { isDesktop } from '../audio/native'
import { toggleTrackRecord, isTrackRecording, subscribeRec } from './actions'
import { Icon } from './icons'

export function TrackInput({ trackId }: { trackId: string }) {
  useSyncExternalStore(subscribeInputs, inputsVersion)
  useSyncExternalStore(subscribeRec, () => (isTrackRecording(trackId) ? 1 : 0))
  const [primed, setPrimed] = useState(false)
  const devices = inputDevices()
  const sel = trackInput(trackId)
  const recording = isTrackRecording(trackId)
  // desktop: which of the device's inputs (USB interfaces have several)
  const dev = devices.find(d => d.deviceId === sel) ?? devices.find(d => d.label.endsWith('★'))
  const nIn = isDesktop() ? dev?.channels ?? 0 : 0
  const ch = trackInputChannels(trackId)
  const chValue = ch ? `${ch.first}:${ch.count}` : (nIn >= 2 ? '0:2' : '0:1')

  // populate labels (needs mic permission) the first time the user opens the list
  const prime = () => { if (!primed) { setPrimed(true); void refreshInputDevices(true) } }

  return (
    <div className="track-input" onClick={e => e.stopPropagation()} data-info="Audio input for recording. Pick a device, then hit record to capture into a clip.">
      <button
        className={`tbtn track-rec ${recording ? 'on' : ''}`}
        data-info={recording ? 'Stop recording' : 'Record from the selected input'}
        onClick={() => { prime(); toggleTrackRecord(trackId) }}
      ><Icon name="rec" size={9} /></button>
      <select className="input-select" value={sel} onPointerDown={prime}
        onChange={e => setTrackInput(trackId, e.target.value)}
        data-info="Choose which of the computer's audio inputs to record from">
        <option value="">Default input</option>
        {devices.map(d => <option key={d.deviceId} value={d.deviceId}>{d.label}</option>)}
      </select>
      {nIn > 1 && (
        <select className="input-select input-ch" value={chValue}
          onChange={e => {
            const [first, count] = e.target.value.split(':').map(Number)
            setTrackInputChannels(trackId, { first, count: count === 1 ? 1 : 2 })
          }}
          data-info="Which of the interface's inputs to record: a pair (stereo) or one input (mono)">
          {inputChannelOptions(nIn).map(o => <option key={o.value} value={o.value}>{o.label}</option>)}
        </select>
      )}
    </div>
  )
}
