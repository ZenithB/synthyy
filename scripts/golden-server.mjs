// Golden-fixture receiver (docs/RUST_REWRITE_PLAN.md §8a).
// The /#fixtures page (or window.__renderFixture driver) POSTs rendered WAVs
// here so goldens land in fixtures/golden/ without manual download shuffling.
//
//   node scripts/golden-server.mjs        # listens on 127.0.0.1:8787
//   POST /golden/<name>  (body: audio/wav bytes) → fixtures/golden/<name>.wav

import { createServer } from 'node:http'
import { writeFileSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const goldenDir = join(root, 'fixtures', 'golden')
mkdirSync(goldenDir, { recursive: true })

createServer((req, res) => {
  res.setHeader('Access-Control-Allow-Origin', '*')
  res.setHeader('Access-Control-Allow-Methods', 'POST, OPTIONS')
  res.setHeader('Access-Control-Allow-Headers', 'content-type')
  if (req.method === 'OPTIONS') { res.writeHead(204); res.end(); return }
  const m = req.method === 'POST' && req.url?.match(/^\/golden\/([\w][\w.-]*)$/)
  if (!m) { res.writeHead(404); res.end('POST /golden/<name>'); return }
  const chunks = []
  req.on('data', c => chunks.push(c))
  req.on('end', () => {
    const buf = Buffer.concat(chunks)
    if (buf.length < 100 || buf.toString('latin1', 0, 4) !== 'RIFF') {
      res.writeHead(400); res.end('not a WAV'); return
    }
    const file = join(goldenDir, `${m[1]}.wav`)
    writeFileSync(file, buf)
    console.log(`wrote ${file} (${buf.length} bytes)`)
    res.writeHead(200); res.end('ok')
  })
}).listen(8787, '127.0.0.1', () => console.log('golden receiver on http://127.0.0.1:8787'))
