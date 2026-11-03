// Desktop-only (Tauri) guards against the WEBVIEW's browser features leaking
// into the app. WebView2 on Windows enables Edge's accelerator keys and its
// default right-click menu (Back / Refresh / Save as / Print / Inspect), and
// Tauri 2.11 exposes no switch for either (wry has one, Tauri doesn't pass it
// through). Ctrl+R / F5 reloads the page (the engine restarts, playback
// stops), Ctrl+P opens a print dialog, the mouse back button navigates.
// WKWebView on macOS exposes fewer of these, but blocking them is harmless
// there, so the behaviour is identical on both platforms.
//
// Only browser-only keys are blocked. The app's own shortcuts
// (Ctrl/Cmd + K Z Y D A C X V, Space, Tab, digits…) are untouched, and
// text fields keep their native context menu (copy / paste).

/** Ctrl/Cmd + letter combos that only do browser things. */
const BROWSER_MOD_KEYS = new Set(['r', 'p', 'f', 'g', 'u', 's', 'j', 'h', 'o', 'n'])
/** Function keys that only do browser things (find-next, reload, caret
 *  browsing). F12 (devtools) is blocked in production only. */
const BROWSER_FKEYS = new Set(['F3', 'F5', 'F7'])

function isTextField(t: EventTarget | null): boolean {
  const el = t as HTMLElement | null
  return !!el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)
}

/** True when the browser would act on this key and the app never does. */
export function isBrowserOnlyKey(e: Pick<KeyboardEvent, 'key' | 'ctrlKey' | 'metaKey' | 'altKey' | 'shiftKey'>, dev: boolean): boolean {
  const mod = e.ctrlKey || e.metaKey
  const k = e.key.length === 1 ? e.key.toLowerCase() : e.key
  if (mod && BROWSER_MOD_KEYS.has(k)) return true
  if (BROWSER_FKEYS.has(e.key)) return true
  if (e.key === 'BrowserBack' || e.key === 'BrowserForward' || e.key === 'BrowserRefresh') return true
  if (e.altKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) return true // history back/forward
  if (!dev) {
    if (e.key === 'F12') return true
    if (mod && e.shiftKey && (k === 'i' || k === 'j' || k === 'c')) return true // devtools
  }
  return false
}

export function installDesktopGuards() {
  const dev = import.meta.env.DEV
  // capture phase: runs before the browser's default action is decided, and
  // only calls preventDefault — the app's own handlers still see the event
  window.addEventListener('keydown', e => {
    if (isBrowserOnlyKey(e, dev)) e.preventDefault()
  }, { capture: true })
  // mouse back/forward buttons navigate the page in WebView2
  const noNav = (e: MouseEvent) => { if (e.button === 3 || e.button === 4) e.preventDefault() }
  window.addEventListener('mouseup', noNav, { capture: true })
  window.addEventListener('auxclick', noNav, { capture: true })
  // browser context menu: the app's own right-click menus call
  // preventDefault themselves; everything else gets nothing instead of
  // "Refresh / Print / Inspect". Text fields keep copy/paste. Dev builds keep
  // the menu for "Inspect".
  if (!dev) {
    window.addEventListener('contextmenu', e => {
      if (!isTextField(e.target)) e.preventDefault()
    })
  }
}
