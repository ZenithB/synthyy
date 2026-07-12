// Audio-track input picker + record button, shown on audio-track headers in
// both Session and Arrangement. Pick any of the device's audio inputs, then
// record live audio into a clip (playhead in Arrangement, scene slot in Session).

import { useSyncExternalStore, useState } from 'react'
import { inputDevices, inputsVersion, subscribeInputs, refreshInputDevices, trackInput, setTrackInput } from '../state/inputs'
import { toggleTrackRecord, isTrackRecording, subscribeRec } from './actions'
import { Icon } from './icons'

export function TrackInput({ trackId }: { trackId: string }) {
  useSyncExternalStore(subscribeInputs, inputsVersion)
  useSyncExternalStore(subscribeRec, () => (isTrackRecording(trackId) ? 1 : 0))
  const [primed, setPrimed] = useState(false)
  const devices = inputDevices()
  const sel = trackInput(trackId)
  const recording = isTrackRecording(trackId)

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
    </div>
  )
}
