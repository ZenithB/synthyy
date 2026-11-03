// Monochrome vector icon set. Stroke-based, 16x16 grid, inherits currentColor —
// no emoji anywhere in the UI. Filled shapes opt out of the stroke defaults.

import React from 'react'

const F = { fill: 'currentColor', stroke: 'none' } as const

const ICONS: Record<string, React.ReactNode> = {
  // brand / transport
  logo: <>
    <circle cx="8" cy="8" r="5.6" />
    <line x1="8" y1="8" x2="11.4" y2="4.6" />
    <line x1="2" y1="13.4" x2="3.4" y2="12.6" /><line x1="14" y1="13.4" x2="12.6" y2="12.6" />
  </>,
  play: <path d="M5.2 3.2 L13 8 L5.2 12.8 Z" {...F} />,
  stop: <rect x="4.4" y="4.4" width="7.2" height="7.2" rx="1" {...F} />,
  stopOutline: <rect x="4.6" y="4.6" width="6.8" height="6.8" rx="1" />,
  rec: <circle cx="8" cy="8" r="4.2" {...F} />,
  metro: <>
    <path d="M6.2 2.5 h3.6 L12.4 13.5 H3.6 Z" />
    <line x1="8" y1="10.5" x2="11" y2="4.5" />
  </>,
  capture: <>
    <path d="M3 9.5 v3 a1 1 0 0 0 1 1 h8 a1 1 0 0 0 1-1 v-3" />
    <path d="M8 2.2 v6.3 M5.4 6.2 L8 8.8 L10.6 6.2" />
  </>,
  clock: <>
    <circle cx="8" cy="8" r="5.6" />
    <path d="M8 4.8 V8 L10.6 9.6" />
  </>,
  undo: <path d="M4 6.5 H10 a3.5 3.5 0 1 1 0 7 H7 M4 6.5 L7 3.5 M4 6.5 L7 9.5" />,
  redo: <path d="M12 6.5 H6 a3.5 3.5 0 1 0 0 7 H9 M12 6.5 L9 3.5 M12 6.5 L9 9.5" />,
  download: <path d="M8 2.4 v7.6 M4.8 7 L8 10.2 L11.2 7 M3.2 13.4 H12.8" />,
  sun: <>
    <circle cx="8" cy="8" r="3" />
    <path d="M8 1.6v1.8 M8 12.6v1.8 M1.6 8h1.8 M12.6 8h1.8 M3.5 3.5l1.3 1.3 M11.2 11.2l1.3 1.3 M12.5 3.5l-1.3 1.3 M4.8 11.2l-1.3 1.3" />
  </>,
  moon: <path d="M9.8 2.6 A5.8 5.8 0 1 0 13.4 10 A4.8 4.8 0 0 1 9.8 2.6 Z" />,
  link: <>
    <path d="M7 4.6 L8.4 3.2 a3 3 0 0 1 4.4 4.4 L11.4 9" />
    <path d="M9 11.4 L7.6 12.8 a3 3 0 0 1-4.4-4.4 L4.6 7" />
    <line x1="6.4" y1="9.6" x2="9.6" y2="6.4" />
  </>,
  chat: <path d="M4.7 3.2 h6.6 a2 2 0 0 1 2 2 v3.6 a2 2 0 0 1-2 2 H8.2 L5.4 13.4 v-2.6 h-.7 a2 2 0 0 1-2-2 V5.2 a2 2 0 0 1 2-2 Z" />,
  search: <>
    <circle cx="7" cy="7" r="4.2" />
    <line x1="10.2" y1="10.2" x2="13.8" y2="13.8" />
  </>,
  close: <path d="M4.2 4.2 L11.8 11.8 M11.8 4.2 L4.2 11.8" />,
  power: <>
    <path d="M8 2.2 v5.2" />
    <path d="M5 4.2 a5.4 5.4 0 1 0 6 0" />
  </>,
  chevL: <path d="M10 3.2 L5.4 8 L10 12.8" />,
  chevR: <path d="M6 3.2 L10.6 8 L6 12.8" />,
  chevDown: <path d="M3.2 6 L8 10.6 L12.8 6" />,
  plus: <path d="M8 3 v10 M3 8 h10" />,
  minus: <path d="M3 8 h10" />,
  more: <><circle cx="3.6" cy="8" r="1.3" {...F} /><circle cx="8" cy="8" r="1.3" {...F} /><circle cx="12.4" cy="8" r="1.3" {...F} /></>,
  zoomIn: <><circle cx="6.6" cy="6.6" r="3.8" /><line x1="9.5" y1="9.5" x2="14" y2="14" /><path d="M6.6 5 v3.2 M5 6.6 h3.2" /></>,
  zoomOut: <><circle cx="6.6" cy="6.6" r="3.8" /><line x1="9.5" y1="9.5" x2="14" y2="14" /><path d="M5 6.6 h3.2" /></>,
  grip: <path d="M3 6.4 H13 M3 9.6 H13" />,
  lfo: <><circle cx="8" cy="8" r="6" /><path d="M3.4 8 C4.5 4.6, 6 4.6, 7 8 S9.5 11.4, 10.6 8 12.6 5.4, 12.6 8" /></>,
  map: <><circle cx="8" cy="8" r="2.4" /><path d="M8 1.6 v2.2 M8 12.2 v2.2 M1.6 8 h2.2 M12.2 8 h2.2" /></>,
  duck: <path d="M2 4 V9 a2 2 0 0 0 2 2 h2 l2-7 2 7 h2 a2 2 0 0 0 2-2 V4" />,
  sampler: <><rect x="2.4" y="3" width="11.2" height="10" rx="1.4" /><path d="M2.4 8 h2 l1.5-3 1.5 6 1.5-4 1 2 h3.2" /></>,
  spectrum: <><line x1="3" y1="13" x2="3" y2="8" /><line x1="6" y1="13" x2="6" y2="4" /><line x1="9" y1="13" x2="9" y2="6" /><line x1="12" y1="13" x2="12" y2="9" /></>,
  star: <path d="M8 1.8 L9.9 6 L14.2 6.4 L11 9.4 L11.9 13.8 L8 11.4 L4.1 13.8 L5 9.4 L1.8 6.4 L6.1 6 Z" />,
  starFill: <path d="M8 1.8 L9.9 6 L14.2 6.4 L11 9.4 L11.9 13.8 L8 11.4 L4.1 13.8 L5 9.4 L1.8 6.4 L6.1 6 Z" fill="currentColor" stroke="none" />,
  macro: <><circle cx="8" cy="8" r="5.6" /><path d="M8 3.4 V8 L11 9.8" /><circle cx="8" cy="8" r="1.1" fill="currentColor" stroke="none" /></>,
  send: <path d="M2.4 13 L13.6 8 L2.4 3 L4.4 8 Z M4.4 8 H13.6" />,
  follow: <path d="M3 4 L8 8 L3 12 Z M9 4 L14 8 L9 12 Z" fill="currentColor" stroke="none" />,
  mic: <><rect x="6" y="2" width="4" height="7.5" rx="2" /><path d="M3.8 8 a4.2 4.2 0 0 0 8.4 0 M8 12.2 V14 M5.6 14 H10.4" /></>,
  headphones: <>
    <path d="M2.6 10 V8 a5.4 5.4 0 0 1 10.8 0 v2" />
    <rect x="1.8" y="9.4" width="3" height="4.6" rx="1.3" />
    <rect x="11.2" y="9.4" width="3" height="4.6" rx="1.3" />
  </>,
  steps: <><rect x="2" y="3" width="3" height="3" rx="0.5" fill="currentColor" stroke="none" /><rect x="6.5" y="3" width="3" height="3" rx="0.5" /><rect x="11" y="3" width="3" height="3" rx="0.5" fill="currentColor" stroke="none" /><rect x="2" y="10" width="3" height="3" rx="0.5" /><rect x="6.5" y="10" width="3" height="3" rx="0.5" fill="currentColor" stroke="none" /><rect x="11" y="10" width="3" height="3" rx="0.5" /></>,
  curve: <path d="M2 13 C7 13, 9 3, 14 3" />,
  pencil: <>
    <path d="M3 13 L3.6 10.4 L10.8 3.2 L12.8 5.2 L5.6 12.4 Z" />
    <line x1="9.6" y1="4.4" x2="11.6" y2="6.4" />
  </>,
  note: <>
    <path d="M10.6 11 V3 c1.6.7 2.2 1.5 2.2 3" />
    <ellipse cx="8.7" cy="11.2" rx="2" ry="1.5" {...F} />
  </>,
  tools: <path d="M13.8 4.4 a3.6 3.6 0 0 1-4.9 4.3 L5 12.6 a1.7 1.7 0 0 1-2.4-2.4 L6.5 6.3 A3.6 3.6 0 0 1 10.8 1.4 L8.6 3.6 l1 2.4 2.4 1 2.2-2.2 Z" />,
  spark: <path d="M8 1.8 L9.4 6.6 L14.2 8 L9.4 9.4 L8 14.2 L6.6 9.4 L1.8 8 L6.6 6.6 Z" {...F} />,
  newdoc: <>
    <rect x="3" y="3" width="10" height="10" rx="1.6" />
    <path d="M8 5.7 v4.6 M5.7 8 h4.6" />
  </>,
  save: <path d="M8 2.4 v7 M5 6.6 L8 9.6 L11 6.6 M3.4 13.2 H12.6" />,
  folder: <path d="M2.6 4.6 h3.8 l1.5 1.9 h5.5 v5.7 a1.2 1.2 0 0 1-1.2 1.2 H3.8 a1.2 1.2 0 0 1-1.2-1.2 Z" />,
  loop: <>
    <path d="M12.8 8 a4.8 4.8 0 1 1-1.6-3.6" />
    <path d="M11 1.8 L11.4 4.6 L8.6 5" />
  </>,
  grid: <>
    <rect x="3" y="3" width="10" height="10" rx="1.2" />
    <path d="M8 3 v10 M3 8 h10" />
  </>,
  dice: <>
    <rect x="2.8" y="2.8" width="10.4" height="10.4" rx="2.2" />
    <circle cx="5.7" cy="5.7" r="1" {...F} /><circle cx="10.3" cy="10.3" r="1" {...F} /><circle cx="8" cy="8" r="1" {...F} />
  </>,
  legato: <>
    <circle cx="4.6" cy="11" r="1.5" {...F} /><circle cx="11.4" cy="11" r="1.5" {...F} />
    <path d="M4.4 8.6 C6.8 5.2, 9.2 5.2, 11.6 8.6" />
  </>,
  reverse: <>
    <path d="M13 7 A5.4 5.4 0 0 0 4.3 4.5 M4 1.8 v3 h3" />
    <path d="M3 9 A5.4 5.4 0 0 0 11.7 11.5 M12 14.2 v-3 h-3" />
  </>,
  rampUp: <path d="M3 12.8 L12.4 3.4 M8.2 3.4 H12.4 V7.6" />,
  rampDown: <path d="M3 3.2 L12.4 12.6 M12.4 8.4 V12.6 H8.2" />,
  arpUp: <path d="M8 13 V3.6 M4.8 6.8 L8 3.6 L11.2 6.8" />,
  arpDown: <path d="M8 3 V12.4 M4.8 9.2 L8 12.4 L11.2 9.2" />,
  arpUpDown: <path d="M5.4 12.5 V4 M3.2 6.2 L5.4 4 L7.6 6.2 M10.6 3.5 V12 M8.4 9.8 L10.6 12 L12.8 9.8" />,
  strum: <path d="M4.4 13.2 L9.8 2.8 M8 13.2 L13.4 2.8" />,
  chord: <>
    <rect x="3" y="10.4" width="5" height="2.2" rx="1" {...F} />
    <rect x="5.8" y="6.9" width="5" height="2.2" rx="1" {...F} />
    <rect x="8.6" y="3.4" width="5" height="2.2" rx="1" {...F} />
  </>,

  // instruments
  wave: <path d="M2 8 C3.5 3.6, 6 3.6, 8 8 S12.5 12.4, 14 8" />,
  duo: <>
    <path d="M2 6.2 C3.5 2.4, 6 2.4, 8 6.2 S12.5 10, 14 6.2" />
    <path d="M2 10.4 C3.5 6.6, 6 6.6, 8 10.4 S12.5 14.2, 14 10.4" opacity="0.55" />
  </>,
  bell: <>
    <path d="M8 2.4 C5.4 2.4 4.8 4.8 4.7 7.2 C4.6 9.8 3.8 10.8 3.2 11.4 H12.8 C12.2 10.8 11.4 9.8 11.3 7.2 C11.2 4.8 10.6 2.4 8 2.4 Z" />
    <circle cx="8" cy="13.4" r="1" {...F} />
  </>,
  bass: <path d="M2 9.4 C4.4 3.2, 8.8 3.2, 10.8 9.4 C11.8 12.4, 13 12.6, 14 10.6" />,
  pluck: <>
    <line x1="8" y1="2.4" x2="8" y2="13.6" />
    <path d="M5.6 5 C7 6.8, 7 9.2, 5.6 11" opacity="0.6" />
    <path d="M10.4 5 C9 6.8, 9 9.2, 10.4 11" opacity="0.6" />
  </>,
  keys: <>
    <rect x="2.4" y="3.6" width="11.2" height="8.8" rx="1.2" />
    <path d="M6.1 3.6 v5 M9.9 3.6 v5" />
  </>,
  drum: <>
    <path d="M3.4 12.8 L11.2 3.6 M12.6 12.8 L4.8 3.6" />
    <circle cx="11.9" cy="2.9" r="1.1" {...F} /><circle cx="4.1" cy="2.9" r="1.1" {...F} />
  </>,

  // effects
  eq: <>
    <path d="M4 2.4 v11.2 M8 2.4 v11.2 M12 2.4 v11.2" opacity="0.55" />
    <circle cx="4" cy="9.6" r="1.6" {...F} /><circle cx="8" cy="5" r="1.6" {...F} /><circle cx="12" cy="11" r="1.6" {...F} />
  </>,
  filter: <path d="M2 5.2 H8 C10.4 5.2, 11 7, 11.9 9.8 C12.5 11.8, 13.2 13, 14 13.4" />,
  echo: <>
    <path d="M4 4.5 a5 5 0 0 1 0 7" />
    <path d="M7.4 3.6 a6.3 6.3 0 0 1 0 8.8" opacity="0.6" />
    <path d="M10.8 2.7 a7.6 7.6 0 0 1 0 10.6" opacity="0.3" />
  </>,
  reverb: <>
    <rect x="2.6" y="2.6" width="10.8" height="10.8" rx="1.4" />
    <path d="M2.6 8 C5.6 8, 8 5.6, 8 2.6" opacity="0.7" />
    <path d="M2.6 11.4 C7.4 11.4, 11.4 7.4, 11.4 2.6" opacity="0.4" />
  </>,
  // plate reverb: a suspended plate with ripples
  plate: <>
    <rect x="2.2" y="4.4" width="11.6" height="8" rx="1" />
    <path d="M4.2 8.4 C5.2 6.8, 6.4 6.8, 7.4 8.4 S9.8 10, 10.8 8.4" opacity="0.7" />
    <line x1="5" y1="1.8" x2="5" y2="4.4" /><line x1="11" y1="1.8" x2="11" y2="4.4" />
  </>,
  // granular: a scatter of grains around a wave
  grain: <>
    <path d="M2 8 C4 5.4, 6 5.4, 8 8 S12 10.6, 14 8" opacity="0.5" />
    <circle cx="4" cy="4.6" r="1" {...F} /><circle cx="7.4" cy="3.4" r="0.8" {...F} />
    <circle cx="11" cy="5" r="1.1" {...F} /><circle cx="5.4" cy="11.6" r="1.1" {...F} />
    <circle cx="9" cy="12.4" r="0.8" {...F} /><circle cx="12.6" cy="11" r="1" {...F} />
  </>,
  chorus: <>
    <path d="M2 8 C3.5 4, 6 4, 8 8 S12.5 12, 14 8" />
    <path d="M3.4 8 C4.9 4, 7.4 4, 9.4 8 S13.9 12, 15.4 8" opacity="0.4" />
  </>,
  bolt: <path d="M9.2 1.8 L4 9 H7.4 L6.2 14.2 L12 6.6 H8.4 Z" {...F} />,
  crush: <>
    <rect x="2.8" y="8.8" width="3.6" height="3.6" {...F} />
    <rect x="6.4" y="5.2" width="3.6" height="3.6" {...F} opacity="0.7" />
    <rect x="10" y="1.6" width="3.6" height="3.6" {...F} opacity="0.45" />
  </>,
  comp: <path d="M3 4 L7 8 L3 12 M13 4 L9 8 L13 12" />,
  opto: <>
    <circle cx="8" cy="8" r="3" {...F} />
    <circle cx="8" cy="8" r="5.6" />
    <line x1="8" y1="1" x2="8" y2="2.4" /><line x1="8" y1="13.6" x2="8" y2="15" />
    <line x1="1" y1="8" x2="2.4" y2="8" /><line x1="13.6" y1="8" x2="15" y2="8" />
  </>,
  destroyer: <>
    <path d="M1.5 8 L3.5 3.5 H6 L7.5 8 L9 12.5 H11.5 L13 8 L14.5 5" />
    <path d="M5 1.5 L4 4 M11 14.5 L12 12" opacity="0.6" />
  </>,
  gate: <>
    <path d="M2 12.5 H5 V4 H8.5 V12.5 H11.5 V4 H14" />
    <line x1="1.5" y1="8.2" x2="14.5" y2="8.2" strokeDasharray="2 2" opacity="0.55" />
  </>,

  mbcomp: <>
    <line x1="2.5" y1="11" x2="2.5" y2="5" /><line x1="5" y1="11" x2="5" y2="3" />
    <line x1="7.5" y1="11" x2="7.5" y2="6" /><line x1="10" y1="11" x2="10" y2="4" />
    <line x1="12.5" y1="11" x2="12.5" y2="7" />
    <line x1="1" y1="13" x2="15" y2="13" />
  </>,
  phaser: <>
    <circle cx="8" cy="8" r="5.6" />
    <path d="M3.6 8 C5 5.6, 6.6 5.6, 8 8 S11 10.4, 12.4 8" />
  </>,
  pingpong: <>
    <path d="M3 5.2 H12.2 M9.8 2.8 L12.4 5.2 L9.8 7.6" />
    <path d="M13 10.8 H3.8 M6.2 8.4 L3.6 10.8 L6.2 13.2" />
  </>,
  autofilt: <>
    <path d="M2 4.6 H7 C9.6 4.6, 10.4 6.6, 11.4 9.4" />
    <path d="M3.4 12.6 C4.4 10.8, 5.6 10.8, 6.6 12.6 S8.8 14.4, 9.8 12.6" opacity="0.7" />
  </>,
  trem: <>
    <path d="M2 8 C5 2.6, 11 2.6, 14 8" />
    <path d="M2 8 C5 13.4, 11 13.4, 14 8" />
    <line x1="2" y1="8" x2="14" y2="8" opacity="0.4" />
  </>,
  autopan: <path d="M4.6 8 H11.4 M6.6 5 L3.4 8 L6.6 11 M9.4 5 L12.6 8 L9.4 11" />,
  vib: <path d="M2 8 Q3.5 4.6, 5 8 T8 8 T11 8 T14 8" />,
  heat: <path d="M2 11 L5 5 L8 11 L11 5 L14 11" />,
  widen: <>
    <circle cx="8" cy="8" r="1.2" {...F} />
    <path d="M5.4 8 H2.6 M4.6 5.8 L2.4 8 L4.6 10.2 M10.6 8 H13.4 M11.4 5.8 L13.6 8 L11.4 10.2" />
  </>,
  shift: <>
    <path d="M5.4 13 V3.6 M3.2 5.8 L5.4 3.6 L7.6 5.8" />
    <path d="M10.6 3 V12.4 M8.4 10.2 L10.6 12.4 L12.8 10.2" opacity="0.6" />
  </>,
  // pitch snapped to discrete steps (quantize staircase) + a corrected note dot
  autotune: <>
    <path d="M2 12 H5 V9 H8 V6 H11 V3 H14" />
    <circle cx="5" cy="9" r="1.1" {...F} />
  </>,
}

export function Icon({ name, size = 14, className }: { name: string; size?: number; className?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={`icon${className ? ' ' + className : ''}`}
      aria-hidden
    >
      {ICONS[name] ?? ICONS.note}
    </svg>
  )
}
