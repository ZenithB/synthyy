// Shared canvas sizing for HiDPI displays.

/** Size `canvas`'s backing store for crisp drawing at the current display
 *  scale (CSS size W×H) and return the x/y scale to pass to setTransform.
 *
 *  Backing sizes are rounded to whole pixels. Windows commonly runs
 *  fractional display scales (125 %, 150 %, 175 %): `W * devicePixelRatio`
 *  is then fractional while `canvas.width` is an integer, so the old
 *  `canvas.width !== W * dpr` check was true on EVERY frame and the canvas
 *  was reallocated and cleared each time it drew (macOS's exact 2× never hit
 *  this). The returned scale is the exact backing/CSS ratio, so drawing in
 *  CSS pixels still fills the canvas edge to edge after rounding. */
export function fitCanvas(canvas: HTMLCanvasElement, W: number, H: number): { sx: number; sy: number } {
  const dpr = window.devicePixelRatio || 1
  const bw = Math.max(1, Math.round(W * dpr))
  const bh = Math.max(1, Math.round(H * dpr))
  if (canvas.width !== bw || canvas.height !== bh) {
    canvas.width = bw
    canvas.height = bh
    canvas.style.width = `${W}px`
    canvas.style.height = `${H}px`
  }
  return { sx: bw / Math.max(1, W), sy: bh / Math.max(1, H) }
}
