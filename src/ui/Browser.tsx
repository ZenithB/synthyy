// Left-side browser: searchable sound packs + the user's sample bank. Single
// click auditions, double click (or drag onto a track/slot) loads. Sections
// collapse and drag-reorder; both persist locally so the layout is yours.
// The Samples section is a file-system style browser over the sample bank:
// folders, bulk folder import, rename/move/delete, drag-out to use anywhere.

import React, { useState, useSyncExternalStore } from 'react'
import { INST_PRESETS, DRUM_KITS, MIDI_LOOPS, PROGRESSIONS, progressionPitches } from '../packs'
import { DRUM_PACKS, DrumPack, RoleName, packSampleId, ROLE_LABEL } from '../audio/drumpacks'
import { engine } from '../audio/engine'
import { ui } from '../state/store'
import {
  applyPreset, applyDrumKit, loadLoop, loadProgression, loadDemo, newProject, importProjectFile,
  assignKitToTrack, pickAudioFiles, pickAudioFolder, importAudioToBank,
} from './actions'
import { exportProjectFile } from '../audio/render'
import { meta } from '../state/doc'
import { useY } from './hooks'
import { NOTE_NAMES } from '../theory'
import { instSchema } from '../audio/schema'
import { Icon } from './icons'
import { openMenu, MenuItem } from './widgets'
import { listUserPresets, removeUserPreset, isFavorite, toggleFavorite, useLib } from '../userlib'
import {
  listSamples, listSampleFolders, subscribeSampleLib, sampleLibVersion, SampleMeta,
  renameSample, moveSample, deleteSample, createSampleFolder, renameSampleFolder, deleteSampleFolder,
  collectDroppedAudio,
} from '../audio/samples'

function Star({ id }: { id: string }) {
  useLib()
  const fav = isFavorite(id)
  return (
    <button className={`bitem-star ${fav ? 'on' : ''}`} title={fav ? 'Unfavorite' : 'Favorite'}
      onClick={e => { e.stopPropagation(); toggleFavorite(id) }}>
      <Icon name={fav ? 'starFill' : 'star'} size={12} />
    </button>
  )
}

// ---------------- persisted section layout ----------------
const SECTION_IDS = ['mysounds', 'favorites', 'samples', 'instruments', 'drumkits', 'progressions', 'loops', 'drumpacks', 'project'] as const
type SectionId = typeof SECTION_IDS[number]
const ORDER_KEY = 'sf-browser-order'
const OPEN_KEY = 'sf-browser-open'

function loadOrder(): SectionId[] {
  try {
    const saved: string[] = JSON.parse(localStorage.getItem(ORDER_KEY) || '[]')
    const valid = saved.filter(id => (SECTION_IDS as readonly string[]).includes(id)) as SectionId[]
    // new sections that shipped after the user saved an order slot in at their default position
    const missing = SECTION_IDS.filter(id => !valid.includes(id))
    return [...valid, ...missing]
  } catch { return [...SECTION_IDS] }
}
function loadOpen(): Record<string, boolean> {
  try { return JSON.parse(localStorage.getItem(OPEN_KEY) || '{}') } catch { return {} }
}

// Collapsible sub-group inside a section (instrument categories, loop
// categories…). Open state persists locally, like the sections themselves.
const SUBOPEN_KEY = 'sf-browser-subopen'
function loadSubOpen(): Record<string, boolean> {
  try { return JSON.parse(localStorage.getItem(SUBOPEN_KEY) || '{}') } catch { return {} }
}
function SubGroup({ id, title, children }: { id: string; title: string; children: React.ReactNode }) {
  const [open, setOpen] = useState(() => loadSubOpen()[id] ?? true)
  const toggle = () => {
    const next = !open
    setOpen(next)
    const all = loadSubOpen(); all[id] = next
    localStorage.setItem(SUBOPEN_KEY, JSON.stringify(all))
  }
  return (
    <div className="bsubgroup">
      <button className="bcat bcat-toggle" onClick={toggle} data-info="Click to collapse/expand this group">
        <span className={`tri ${open ? 'open' : ''}`}>▸</span> {title}
      </button>
      {open && children}
    </div>
  )
}

function Section({ id, title, open, onToggle, onReorder, children }: {
  id: string; title: string; open: boolean
  onToggle: () => void
  onReorder: (dragId: string, beforeId: string) => void
  children: React.ReactNode
}) {
  const [hover, setHover] = useState(false)
  return (
    <div className={`bsection ${hover ? 'drop-before' : ''}`}>
      <button
        className="bsection-head"
        draggable
        onDragStart={e => { e.dataTransfer.setData('stg/bsection', id); e.dataTransfer.effectAllowed = 'move' }}
        onDragOver={e => { if (e.dataTransfer.types.includes('stg/bsection')) { e.preventDefault(); setHover(true) } }}
        onDragLeave={() => setHover(false)}
        onDrop={e => {
          setHover(false)
          const dragId = e.dataTransfer.getData('stg/bsection')
          if (dragId && dragId !== id) { e.preventDefault(); e.stopPropagation(); onReorder(dragId, id) }
        }}
        onClick={onToggle}
        data-info="Click to collapse/expand · drag onto another section header to reorder the browser"
      >
        <span className={`tri ${open ? 'open' : ''}`}>▸</span> {title}
        <span className="bsection-grip"><Icon name="grip" size={10} /></span>
      </button>
      {open && <div className="bsection-body">{children}</div>}
    </div>
  )
}

const LOOP_ICONS: Record<string, string> = { Drums: 'drum', Bass: 'bass', Chords: 'chord', Melody: 'note', Arps: 'arpUp' }

// A genre drum pack: drag the header to load the whole kit onto a drum track;
// expand to drag individual samples onto the timeline or a drum pad.
function DrumPackRow({ pack }: { pack: DrumPack }) {
  const [open, setOpen] = useState(false)
  const roles = Object.keys(pack.roles) as RoleName[]
  return (
    <div className="bpack">
      <div className="bpack-head" draggable
        onDragStart={e => e.dataTransfer.setData('stg/drumkit', pack.id)}
        onClick={() => setOpen(o => !o)}
        onDoubleClick={() => ui.selTrackId && assignKitToTrack(ui.selTrackId, pack.id)}
        data-info="Drag onto a drum track to load the whole kit (kick→kick, snare→snare…). Click to expand, double-click loads to the selected drum track.">
        <span className={`tri ${open ? 'open' : ''}`}>▸</span>
        <span className="bitem-icon"><Icon name="drum" size={12} /></span>{pack.name}
        <span className="bitem-sub">{pack.tag}</span>
      </div>
      {open && (
        <div className="bpack-body">
          {roles.map(role => (
            <div key={role} className="bitem bsample" draggable
              onDragStart={e => e.dataTransfer.setData('stg/sample', `${packSampleId(pack.id, role)}::${pack.name} ${ROLE_LABEL[role]}`)}
              onMouseEnter={() => engine.preloadSample(packSampleId(pack.id, role))}
              onClick={() => engine.auditionSample(packSampleId(pack.id, role))}
              data-info="Click to audition (click again to stop) · drag onto the arrangement timeline (audio clip) or onto a drum device pad">
              <span className="bitem-icon"><Icon name="sampler" size={11} /></span>{ROLE_LABEL[role]}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

// ---------------- the sample bank (file-system style) ----------------

function useSampleLib() {
  return useSyncExternalStore(subscribeSampleLib, sampleLibVersion)
}

function SampleRow({ s, folders }: { s: SampleMeta; folders: string[] }) {
  const [renaming, setRenaming] = useState(false)
  const [name, setName] = useState(s.name)
  const menu = (e: React.MouseEvent) => openMenu(e, [
    { label: <><Icon name="play" size={11} /> Audition</>, fn: () => engine.auditionSample(s.id) },
    { label: <><Icon name="pencil" size={11} /> Rename</>, fn: () => { setName(s.name); setRenaming(true) } },
    'sep',
    { custom: <div className="ctx-row ctx-row-head"><Icon name="folder" size={11} /> Move to</div> },
    ...[''].concat(folders).filter(f => f !== s.folder).map(f => ({
      label: <><Icon name="folder" size={11} /> {f === '' ? '(bank root)' : f}</>,
      fn: () => moveSample(s.id, f),
    })),
    'sep' as const,
    { label: <><Icon name="close" size={11} /> Delete sample</>, fn: () => { if (confirm(`Delete "${s.name}" from the sample bank?`)) deleteSample(s.id) }, danger: true },
  ] as MenuItem[])
  if (renaming) {
    return (
      <input className="inline-rename bsample-rename" autoFocus value={name}
        onChange={e => setName(e.target.value)}
        onBlur={() => { setRenaming(false); if (name.trim()) renameSample(s.id, name) }}
        onKeyDown={e => {
          if (e.key === 'Enter') { setRenaming(false); if (name.trim()) renameSample(s.id, name) }
          if (e.key === 'Escape') setRenaming(false)
          e.stopPropagation()
        }} />
    )
  }
  return (
    <div className="bitem bsample" draggable
      onDragStart={e => {
        e.dataTransfer.setData('stg/sample', `${s.id}::${s.name}`)   // use: timeline, pads, sampler, slots
        e.dataTransfer.setData('stg/samplemove', s.id)               // organize: drop on a folder row
      }}
      onMouseEnter={() => engine.preloadSample(s.id)}
      onClick={() => engine.auditionSample(s.id)}
      onDoubleClick={() => { setName(s.name); setRenaming(true) }}
      onContextMenu={menu}
      data-info="Click: audition (click again to stop) · drag onto an audio-track slot, the timeline, a drum pad or a sampler · right-click: rename / move / delete">
      <span className="bitem-icon"><Icon name="sampler" size={11} /></span>{s.name}
    </div>
  )
}

type FolderNode = { name: string; path: string; subs: FolderNode[]; files: SampleMeta[] }

function buildTree(samples: SampleMeta[], folders: string[]): FolderNode {
  const root: FolderNode = { name: '', path: '', subs: [], files: [] }
  const nodeAt = new Map<string, FolderNode>([['', root]])
  const ensure = (path: string): FolderNode => {
    const hit = nodeAt.get(path)
    if (hit) return hit
    const parts = path.split('/')
    const parent = ensure(parts.slice(0, -1).join('/'))
    const node: FolderNode = { name: parts[parts.length - 1], path, subs: [], files: [] }
    parent.subs.push(node)
    nodeAt.set(path, node)
    return node
  }
  folders.forEach(ensure)
  samples.forEach(s => { ensure(s.folder).files.push(s) })
  const sortNode = (n: FolderNode) => { n.subs.sort((a, b) => a.name.localeCompare(b.name)); n.subs.forEach(sortNode) }
  sortNode(root)
  return root
}

function FolderRow({ node, depth, folders }: { node: FolderNode; depth: number; folders: string[] }) {
  const [open, setOpen] = useState(depth === 0)
  const [hover, setHover] = useState(false)
  const count = node.files.length + node.subs.reduce((n, s) => n + s.files.length, 0)
  const menu = (e: React.MouseEvent) => openMenu(e, [
    { label: <><Icon name="folder" size={11} /> Import files here…</>, fn: () => pickAudioFiles(node.path) },
    { label: <><Icon name="folder" size={11} /> Import folder here…</>, fn: () => pickAudioFolder(node.path) },
    { label: <><Icon name="plus" size={11} /> New subfolder</>, fn: () => { const n = prompt('Folder name'); if (n?.trim()) createSampleFolder(`${node.path}/${n.trim()}`) } },
    'sep',
    { label: <><Icon name="pencil" size={11} /> Rename folder</>, fn: () => {
        const n = prompt('Rename folder', node.name)
        if (n?.trim()) renameSampleFolder(node.path, node.path.split('/').slice(0, -1).concat(n.trim()).join('/'))
      } },
    { label: <><Icon name="close" size={11} /> Delete folder</>, fn: () => {
        if (confirm(`Delete "${node.path}" and the ${count} sample${count === 1 ? '' : 's'} inside it?`)) deleteSampleFolder(node.path)
      }, danger: true },
  ])
  const acceptDrop = (e: React.DragEvent) =>
    e.dataTransfer.types.includes('stg/samplemove') || e.dataTransfer.types.includes('Files')
  return (
    <div className="bfolder" style={{ marginLeft: depth ? 10 : 0 }}>
      <div className={`bfolder-head ${hover ? 'drop' : ''}`}
        onClick={() => setOpen(o => !o)}
        onContextMenu={menu}
        onDragOver={e => { if (acceptDrop(e)) { e.preventDefault(); e.stopPropagation(); setHover(true) } }}
        onDragLeave={() => setHover(false)}
        onDrop={async e => {
          setHover(false)
          const mv = e.dataTransfer.getData('stg/samplemove')
          if (mv) { e.preventDefault(); e.stopPropagation(); moveSample(mv, node.path); return }
          if (e.dataTransfer.types.includes('Files')) {
            e.preventDefault(); e.stopPropagation()
            const items = await collectDroppedAudio(e.dataTransfer, node.path)
            importAudioToBank(items)
          }
        }}
        data-info="Click to open · drop samples here to move them · drop OS files/folders to import here · right-click for folder options">
        <span className={`tri ${open ? 'open' : ''}`}>▸</span>
        <span className="bitem-icon"><Icon name="folder" size={12} /></span>{node.name}
        <span className="bitem-sub">{count}</span>
      </div>
      {open && (
        <div className="bfolder-body">
          {node.subs.map(sub => <FolderRow key={sub.path} node={sub} depth={depth + 1} folders={folders} />)}
          {node.files.map(s => <SampleRow key={s.id} s={s} folders={folders} />)}
        </div>
      )}
    </div>
  )
}

function SamplesBrowser({ query }: { query: string }) {
  useSampleLib()
  const [rootHover, setRootHover] = useState(false)
  const all = listSamples()
  const folders = listSampleFolders()
  const filtered = query ? all.filter(s => `${s.folder}/${s.name}`.toLowerCase().includes(query)) : all
  const tree = buildTree(query ? [] : filtered, query ? [] : folders)
  return (
    <div className={`bsamples ${rootHover ? 'drop' : ''}`}
      onDragOver={e => { if (e.dataTransfer.types.includes('Files')) { e.preventDefault(); setRootHover(true) } }}
      onDragLeave={() => setRootHover(false)}
      onDrop={async e => {
        setRootHover(false)
        if (!e.dataTransfer.types.includes('Files')) return
        e.preventDefault(); e.stopPropagation()
        const items = await collectDroppedAudio(e.dataTransfer)
        importAudioToBank(items)
      }}>
      <div className="bsamples-tools">
        <button className="bbtn" onClick={() => pickAudioFiles('')} data-info="Import audio files into the sample bank"><Icon name="sampler" size={12} />Import files…</button>
        <button className="bbtn" onClick={() => pickAudioFolder('')} data-info="Import a whole folder (its structure is kept)"><Icon name="folder" size={12} />Import folder…</button>
        <button className="bbtn" onClick={() => { const n = prompt('Folder name'); if (n?.trim()) createSampleFolder(n.trim()) }} data-info="Create an empty folder to organise into"><Icon name="plus" size={12} />New folder</button>
      </div>
      {all.length === 0 && <div className="bcat" style={{ paddingBottom: 6 }}>Drop audio files or folders anywhere — they land here</div>}
      {query
        ? filtered.map(s => <SampleRow key={s.id} s={s} folders={folders} />)
        : (
          <>
            {tree.subs.map(sub => <FolderRow key={sub.path} node={sub} depth={0} folders={folders} />)}
            {tree.files.map(s => <SampleRow key={s.id} s={s} folders={folders} />)}
          </>
        )}
    </div>
  )
}

// ---------------- browser ----------------

export function Browser() {
  const [q, setQ] = useState('')
  useY(meta)
  const [order, setOrder] = useState<SectionId[]>(loadOrder)
  const [openMap, setOpenMap] = useState<Record<string, boolean>>(loadOpen)
  const isOpen = (id: string) => openMap[id] ?? true
  const toggle = (id: string) => {
    const next = { ...openMap, [id]: !isOpen(id) }
    setOpenMap(next)
    localStorage.setItem(OPEN_KEY, JSON.stringify(next))
  }
  const reorder = (dragId: string, beforeId: string) => {
    const next = order.filter(x => x !== dragId)
    next.splice(next.indexOf(beforeId as SectionId), 0, dragId as SectionId)
    setOrder(next)
    localStorage.setItem(ORDER_KEY, JSON.stringify(next))
  }

  const query = q.toLowerCase()
  const match = (s: string) => !query || s.toLowerCase().includes(query)

  const presets = INST_PRESETS.filter(p => match(`${p.name} ${p.cat}`))
  const kits = DRUM_KITS.filter(k => match(k.name))
  const loops = MIDI_LOOPS.filter(l => match(`${l.name} ${l.cat}`))
  const progs = PROGRESSIONS.filter(p => match(`${p.name} ${p.numerals} ${p.mode} chords progression`))

  useLib()
  const cats = [...new Set(presets.map(p => p.cat))]
  const loopCats = [...new Set(loops.map(l => l.cat))]
  const rootPc = meta.get('root') ?? 9
  const auditionInst = INST_PRESETS.find(p => p.name === 'Dream Keys')!
  const userPresets = listUserPresets().filter(p => match(p.name))
  const favPresets = INST_PRESETS.filter(p => isFavorite(`i:${p.name}`) && match(`${p.name} ${p.cat}`))

  // Each section as (title, body, hidden?) — render order + collapse state are the user's.
  const sections: Record<SectionId, { title: string; body: () => React.ReactNode; hidden?: boolean }> = {
    mysounds: {
      title: 'My Sounds', hidden: userPresets.length === 0,
      body: () => userPresets.map(p => (
        <div key={p.name} className="bitem" draggable
          onDragStart={e => e.dataTransfer.setData('stg/preset', p.name)}
          onClick={() => engine.audition(p.type, p.params)}
          onDoubleClick={() => applyPreset({ name: p.name, cat: 'User', type: p.type, params: p.params })}
          data-info="Your saved preset · double-click to load · ✕ to delete">
          <span className="bitem-icon"><Icon name={instSchema(p.type).icon} size={12} /></span>{p.name}
          <button className="bitem-star" title="Delete" onClick={e => { e.stopPropagation(); removeUserPreset(p.name) }}><Icon name="close" size={11} /></button>
        </div>
      )),
    },
    favorites: {
      title: 'Favorites', hidden: favPresets.length === 0,
      body: () => favPresets.map(p => (
        <div key={p.name} className="bitem" draggable
          onDragStart={e => e.dataTransfer.setData('stg/preset', p.name)}
          onClick={() => engine.audition(p.type, p.params)}
          onDoubleClick={() => applyPreset(p)}
          data-info="Click: audition · Double-click: load">
          <span className="bitem-icon"><Icon name={instSchema(p.type).icon} size={12} /></span>{p.name}
          <Star id={`i:${p.name}`} />
        </div>
      )),
    },
    samples: {
      title: 'Samples',
      body: () => <SamplesBrowser query={query} />,
    },
    instruments: {
      title: 'Instruments',
      body: () => cats.map(cat => (
        <SubGroup key={cat} id={`inst:${cat}`} title={cat}>
          {presets.filter(p => p.cat === cat).map(p => (
            <div key={p.name} className="bitem" draggable
              onDragStart={e => e.dataTransfer.setData('stg/preset', p.name)}
              onClick={() => engine.audition(p.type, p.params)}
              onDoubleClick={() => applyPreset(p)}
              data-info="Click: audition · Double-click: load to selected track · Drag onto a track">
              <span className="bitem-icon"><Icon name={instSchema(p.type).icon} size={12} /></span>{p.name}
              <Star id={`i:${p.name}`} />
            </div>
          ))}
        </SubGroup>
      )),
    },
    drumkits: {
      title: 'Drum Kits',
      body: () => kits.map(k => (
        <div key={k.name} className="bitem" draggable
          onDragStart={e => e.dataTransfer.setData('stg/kit', k.name)}
          onClick={() => engine.audition('drum', k.params)}
          onDoubleClick={() => applyDrumKit(k.name)}
          data-info="Click: audition · Double-click: load kit · Drag onto a drum track">
          <span className="bitem-icon"><Icon name="drum" size={12} /></span>{k.name}
        </div>
      )),
    },
    progressions: {
      title: 'Chord Progressions',
      body: () => (
        <>
          <div className="bcat">In your key: {NOTE_NAMES[rootPc]}</div>
          {progs.map(p => (
            <div key={p.name} className="bitem" draggable
              onDragStart={e => e.dataTransfer.setData('stg/prog', p.name)}
              onClick={() => engine.auditionPitches(auditionInst.type, auditionInst.params, p.chords.slice(0, 4).map(c => progressionPitches(rootPc, c)))}
              onDoubleClick={() => loadProgression(p)}
              data-info={`${p.numerals} (${p.mode}) — click: hear it in ${NOTE_NAMES[rootPc]} · double-click: drop into a slot · drag anywhere`}>
              <span className="bitem-icon"><Icon name={p.mode === 'major' ? 'sun' : 'moon'} size={12} /></span>{p.name}
              <span className="bitem-sub">{p.numerals}</span>
            </div>
          ))}
        </>
      ),
    },
    loops: {
      title: 'MIDI Loops',
      body: () => loopCats.map(cat => {
        const items = loops.filter(l => l.cat === cat)
        if (!items.length) return null
        return (
          <SubGroup key={cat} id={`loops:${cat}`} title={cat}>
            {items.map(l => (
              <div key={l.name} className="bitem" draggable
                onDragStart={e => e.dataTransfer.setData('stg/loop', l.name)}
                onDoubleClick={() => loadLoop(l)}
                data-info={l.forDrums
                  ? 'Double-click: drop into a free slot · Drag onto any clip slot or the timeline'
                  : `Loads transposed to your key (${NOTE_NAMES[rootPc]}) — double-click for a free slot, or drag anywhere`}>
                <span className="bitem-icon"><Icon name={LOOP_ICONS[cat] ?? 'note'} size={12} /></span>{l.name}
              </div>
            ))}
          </SubGroup>
        )
      }),
    },
    drumpacks: {
      title: 'Drum Packs',
      body: () => DRUM_PACKS.filter(p => match(`${p.name} ${p.tag} drum samples kit`)).map(p => <DrumPackRow key={p.id} pack={p} />),
    },
    project: {
      title: 'Project',
      body: () => (
        <>
          <button className="bbtn" onClick={loadDemo} data-info="Load the demo song (replaces the current project — undoable)"><Icon name="spark" size={13} />Load demo song</button>
          <button className="bbtn" onClick={newProject} data-info="Start fresh (undoable)"><Icon name="newdoc" size={13} />New project</button>
          <button className="bbtn" onClick={exportProjectFile} data-info="Download the project as a shareable .json file"><Icon name="save" size={13} />Save project file</button>
          <button className="bbtn" onClick={importProjectFile} data-info="Load a saved .synthtagram.json project"><Icon name="folder" size={13} />Import project</button>
        </>
      ),
    },
  }

  return (
    <div className="browser">
      <div className="browser-search">
        <Icon name="search" size={13} />
        <input
          placeholder="Search sounds…"
          value={q}
          onChange={e => setQ(e.target.value)}
          data-info="Search every pack: presets, kits, loops, progressions and your samples"
        />
      </div>
      <div className="browser-scroll">
        {order.map(id => {
          const s = sections[id]
          if (!s || s.hidden) return null
          return (
            <Section key={id} id={id} title={s.title} open={isOpen(id)} onToggle={() => toggle(id)} onReorder={reorder}>
              {s.body()}
            </Section>
          )
        })}
      </div>
    </div>
  )
}
