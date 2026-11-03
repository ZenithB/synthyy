// Shared audio encoders used by both the offline WAV/MP3 export and the live
// "Record Output" capture. Each takes a 44.1kHz AudioBuffer and returns a Blob.
// WAV is 16-bit PCM; MP3 is CBR via lamejs (64/128/256 kbps). Mono downmixes L+R.

import { Mp3Encoder } from '@breezystack/lamejs'
import { isDesktop, nativeIo } from './native'
import { toast } from '../state/store'

export type Channels = 'stereo' | 'mono'
export type AudioFormat = 'wav' | 'mp3'

export const OUT_SR = 44100

/** Resample any buffer to 44.1kHz (the engine renders at 88.2kHz). */
export async function resample(buf: AudioBuffer, outSr = OUT_SR): Promise<AudioBuffer> {
  const len = Math.ceil(buf.duration * outSr)
  const ctx = new OfflineAudioContext(Math.min(2, buf.numberOfChannels), len, outSr)
  const src = ctx.createBufferSource()
  src.buffer = buf
  src.connect(ctx.destination)
  src.start()
  return ctx.startRendering()
}

/** Resolve a buffer to 1 (mono downmix) or 2 channels of Float32 data. */
function channelData(buf: AudioBuffer, channels: Channels): Float32Array[] {
  const L = buf.getChannelData(0)
  const R = buf.numberOfChannels > 1 ? buf.getChannelData(1) : L
  if (channels === 'mono') {
    const m = new Float32Array(buf.length)
    for (let i = 0; i < buf.length; i++) m[i] = (L[i] + R[i]) * 0.5
    return [m]
  }
  return [L, R]
}

function floatToInt16(f: Float32Array): Int16Array {
  const o = new Int16Array(f.length)
  for (let i = 0; i < f.length; i++) {
    const s = Math.max(-1, Math.min(1, f[i]))
    o[i] = s < 0 ? s * 0x8000 : s * 0x7fff
  }
  return o
}

export function encodeWav(buf: AudioBuffer, channels: Channels): Blob {
  const data = channelData(buf, channels)
  const numCh = data.length
  const len = buf.length
  const blockAlign = numCh * 2 // 16-bit
  const dataSize = len * blockAlign
  const ab = new ArrayBuffer(44 + dataSize)
  const dv = new DataView(ab)
  const ws = (off: number, s: string) => { for (let i = 0; i < s.length; i++) dv.setUint8(off + i, s.charCodeAt(i)) }
  ws(0, 'RIFF'); dv.setUint32(4, 36 + dataSize, true); ws(8, 'WAVE')
  ws(12, 'fmt '); dv.setUint32(16, 16, true); dv.setUint16(20, 1, true)
  dv.setUint16(22, numCh, true); dv.setUint32(24, OUT_SR, true)
  dv.setUint32(28, OUT_SR * blockAlign, true); dv.setUint16(32, blockAlign, true); dv.setUint16(34, 16, true)
  ws(36, 'data'); dv.setUint32(40, dataSize, true)
  let off = 44
  for (let i = 0; i < len; i++) {
    for (let c = 0; c < numCh; c++) {
      const s = Math.max(-1, Math.min(1, data[c][i]))
      dv.setInt16(off, s < 0 ? s * 0x8000 : s * 0x7fff, true)
      off += 2
    }
  }
  return new Blob([ab], { type: 'audio/wav' })
}

export function encodeMp3(buf: AudioBuffer, channels: Channels, kbps: number): Blob {
  const data = channelData(buf, channels)
  const numCh = data.length
  const enc = new Mp3Encoder(numCh, OUT_SR, kbps)
  const left = floatToInt16(data[0])
  const right = numCh > 1 ? floatToInt16(data[1]) : null
  const blockSize = 1152
  const out: Uint8Array[] = []
  for (let i = 0; i < left.length; i += blockSize) {
    const lc = left.subarray(i, i + blockSize)
    const chunk = right ? enc.encodeBuffer(lc, right.subarray(i, i + blockSize)) : enc.encodeBuffer(lc)
    if (chunk.length > 0) out.push(new Uint8Array(chunk))
  }
  const end = enc.flush()
  if (end.length > 0) out.push(new Uint8Array(end))
  return new Blob(out as BlobPart[], { type: 'audio/mpeg' })
}

/** Encode a 44.1kHz buffer to the chosen format. */
export function encodeAudio(buf: AudioBuffer, format: AudioFormat, channels: Channels, kbps = 256): Blob {
  return format === 'mp3' ? encodeMp3(buf, channels, kbps) : encodeWav(buf, channels)
}

export function extFor(format: AudioFormat): string {
  return format === 'mp3' ? 'mp3' : 'wav'
}

/** Save an export. Desktop: written straight to the Downloads folder by
 *  the native side (no browser download UI — on Windows WebView2 would show
 *  Edge's download panel and ask to "allow multiple downloads" for stems).
 *  Browser: the usual <a download>. */
export function download(blob: Blob, name: string) {
  if (isDesktop()) {
    nativeIo.saveDownload(name, blob)
      .then(path => toast(`Saved to Downloads: ${path.split(/[\\/]/).pop()}`))
      .catch(e => toast(`Couldn’t save ${name}: ${String(e).slice(0, 120)}`))
    return
  }
  const a = document.createElement('a')
  a.href = URL.createObjectURL(blob)
  a.download = name
  a.click()
  setTimeout(() => URL.revokeObjectURL(a.href), 10000)
}
