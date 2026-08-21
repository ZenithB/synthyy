// Engine facade selector — the ONLY module UI code imports the engine from.
//
// In the Tauri desktop shell (window.__TAURI__ present) the native engine is
// served: an IPC client to the Rust core (engine-native.ts). Everywhere else
// the browser engine (Tone.js) runs. UI code never forks on the target.
//
// Typing the export as EngineApi is the completeness check for the contract:
// any UI use of a member missing from the interface fails `npm run build`.

import type { EngineApi } from './engine-api'
import { engine as toneEngine } from './engine-tone'
import { nativeEngine } from './engine-native'
import { isDesktop } from './native'

export const engine: EngineApi = isDesktop() ? nativeEngine : toneEngine
// Debug handle follows the selected engine (engine-tone sets it eagerly for
// the browser; on desktop the native facade must win).
;(window as any).__engine = engine
