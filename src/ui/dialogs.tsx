// In-app confirm / prompt.
//
// The desktop shell's webview (WKWebView via wry) implements NONE of the
// JavaScript dialog delegates — `runJavaScriptAlertPanel`,
// `runJavaScriptConfirmPanel`, `runJavaScriptTextInputPanel` — so the native
// `confirm()` silently returns false and `prompt()` returns null. Every action
// gated behind one of them (New project, delete track, rename marker, save
// preset…) became a dead button in the desktop app.
//
// These replacements are promise-based and render as normal modals, so they
// behave identically in the browser and on the desktop, and they look like the
// rest of the app instead of an OS alert. Call sites `await` them.

import React, { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { createPortal } from 'react-dom'
import { Icon } from './icons'

type Req =
  | { kind: 'confirm'; message: string; danger: boolean; resolve: (v: boolean) => void }
  | { kind: 'prompt'; message: string; def: string; resolve: (v: string | null) => void }

let current: Req | null = null
let version = 0
const listeners = new Set<() => void>()
const emit = () => { version++; listeners.forEach(l => l()) }
const subscribe = (fn: () => void) => { listeners.add(fn); return () => { listeners.delete(fn) } }

// One dialog at a time; a second request while one is open queues behind it.
const queue: Req[] = []
function push(req: Req) {
  if (current) queue.push(req)
  else { current = req; emit() }
}
function settle() {
  current = queue.shift() ?? null
  emit()
}

/** Ask for confirmation. Resolves true when the user confirms. */
export function confirmDialog(message: string, opts: { danger?: boolean } = {}): Promise<boolean> {
  return new Promise(resolve => push({ kind: 'confirm', message, danger: !!opts.danger, resolve }))
}

/** Ask for a line of text. Resolves null when cancelled (like window.prompt). */
export function promptDialog(message: string, def = ''): Promise<string | null> {
  return new Promise(resolve => push({ kind: 'prompt', message, def, resolve }))
}

/** Mounted once by App — renders whichever request is active. */
export function Dialogs() {
  useSyncExternalStore(subscribe, () => version)
  const req = current
  const [text, setText] = useState('')
  const inputRef = useRef<HTMLInputElement>(null)

  // reset the field for each new prompt and focus it
  useEffect(() => {
    if (req?.kind === 'prompt') {
      setText(req.def)
      const t = setTimeout(() => { inputRef.current?.focus(); inputRef.current?.select() }, 0)
      return () => clearTimeout(t)
    }
  }, [req])

  if (!req) return null

  const done = (v: boolean | string | null) => {
    ;(req.resolve as (x: any) => void)(v)
    settle()
  }
  const cancel = () => done(req.kind === 'confirm' ? false : null)

  return createPortal(
    <div className="modal-backdrop" onPointerDown={e => { if (e.target === e.currentTarget) cancel() }}>
      <div className="modal" style={{ width: 400 }}>
        <div className="modal-head">
          <span>{req.kind === 'confirm' ? 'Confirm' : 'Enter a name'}</span>
          <button className="icon-btn" onClick={cancel}><Icon name="close" /></button>
        </div>
        <div className="dialog-body">
        <p className="dialog-msg">{req.message}</p>
        {req.kind === 'prompt' && (
          <input
            ref={inputRef}
            className="dialog-input"
            value={text}
            onChange={e => setText(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter') { e.preventDefault(); done(text) }
              if (e.key === 'Escape') { e.preventDefault(); cancel() }
            }}
          />
        )}
        <div className="dialog-actions">
          <button className="export-btn ghost" onClick={cancel}>Cancel</button>
          <button
            className={`export-btn${req.kind === 'confirm' && req.danger ? ' record' : ''}`}
            onClick={() => done(req.kind === 'confirm' ? true : text)}
          >
            {req.kind === 'confirm' ? 'OK' : 'Save'}
          </button>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  )
}
