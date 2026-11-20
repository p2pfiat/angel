// Build the Scriptable script: bundles src/scriptable.js and fills scriptable/template.js.
//
//   node scriptable/build.mjs [--watchers npub1...,npub1...] [--out PATH]
//
// Without --watchers the script is generic (fill WATCHERS on the phone).

import { build } from 'esbuild'
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs'
import { dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import { DEFAULT_RELAYS, parsePubkey } from '../src/core.js'

const here = dirname(fileURLToPath(import.meta.url))
const args = process.argv.slice(2)
const opt = (name) => {
  const i = args.indexOf(name)
  return i >= 0 ? args[i + 1] : undefined
}
const watchers = (opt('--watchers') || '').split(',').filter(Boolean)
watchers.forEach(parsePubkey) // fail early on a typo
const out = opt('--out') || `${here}/../dist/Sync.js`

const bundle = await build({
  entryPoints: [`${here}/../src/scriptable.js`],
  bundle: true,
  minify: true,
  format: 'iife',
  target: 'safari15',
  write: false,
})
const code = bundle.outputFiles[0].text

const script = readFileSync(`${here}/template.js`, 'utf8')
  .replace('__WATCHERS__', JSON.stringify(watchers, null, 2))
  .replace('__RELAYS__', JSON.stringify(DEFAULT_RELAYS, null, 2))
  .replace('__BUNDLE__', () => JSON.stringify(code))

mkdirSync(dirname(out), { recursive: true })
writeFileSync(out, script)
console.log(`wrote ${out} (${Math.round(script.length / 1024)} KB, ${watchers.length} watcher(s))`)
