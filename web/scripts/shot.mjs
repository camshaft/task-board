#!/usr/bin/env node
// Parameterized CDP screenshot + optional DOM-eval driver for verifying the task-board web UI
// against a running headless_shell. Encapsulates the WebSocket plumbing (connect -> emulate ->
// navigate -> wait -> optional Runtime.evaluate asserts -> captureScreenshot) so a verify tick
// doesn't re-hand-roll it (and re-hit the env-not-forwarded / output-path papercuts). Dependency-
// free: Node 22 gives a global `WebSocket` + `fetch`.
//
// Usage:
//   node web/scripts/shot.mjs <url> <out.png> [options]
//     --wait <ms>                 wait after navigate before eval/screenshot (default 4000)
//     --eval "<js-expr>"          run a Runtime.evaluate after the wait, print its result to
//                                 stdout; repeatable, runs in order
//     --port <n>                  CDP port (default 9222)
//     --color-scheme light|dark   emulate prefers-color-scheme (Emulation.setEmulatedMedia) so a
//                                 theme verify doesn't hand-roll inline CDP (task 537)
//     --viewport <W>x<H>          override the layout viewport (Emulation.setDeviceMetricsOverride),
//                                 e.g. --viewport 420x900 for a mobile-width check
//     --match-media "<query>=<bool>"  stub window.matchMedia for <query> to return <bool>, injected
//                                 BEFORE any page script (Page.addScriptToEvaluateOnNewDocument).
//                                 Repeatable. Use for media features headless can't emulate — e.g.
//                                 --match-media "(pointer: coarse)=true" for a touch-device check
//                                 (task 537; proven pattern from the mobile-Enter / ambiguity work).
//                                 Non-matching queries fall through to the real matchMedia.
//
// Requires a headless_shell already listening on the CDP port (default 9222), e.g.:
//   headless_shell --headless --no-sandbox --disable-gpu \
//     --remote-debugging-port=9222 --window-size=1400,900 about:blank
//
// Notes:
// - <out.png> is resolved to an ABSOLUTE path here, so it does NOT depend on any env var being
//   forwarded through `nix develop -c node` (the papercut that ate a capture before).
// - --eval runs a Runtime.evaluate (returnByValue, awaitPromise) AFTER the wait and prints its
//   result to stdout; pass it multiple times to run several asserts in order. Return a JSON
//   string from the expression for easy shell-side parsing.
// - Emulation (--color-scheme / --viewport / --match-media) is applied BEFORE navigate so the app
//   sees it from first paint (a matchMedia stub or a theme read at mount is correct on load).
// - Why CDP over `headless_shell --screenshot`: that fires pre-hydration (blank), and
//   `--virtual-time-budget` hangs on the app's open SSE stream. Don't use either.

import { writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

const argv = process.argv.slice(2)
const positionals = []
const evals = []
const matchMedia = {} // query -> boolean, stubbed before navigate
let waitMs = 4000
let port = 9222
let colorScheme = null // 'light' | 'dark'
let viewport = null // { width, height }
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--wait') waitMs = Number(argv[++i])
  else if (a === '--port') port = Number(argv[++i])
  else if (a === '--eval') evals.push(argv[++i])
  else if (a === '--color-scheme') colorScheme = argv[++i]
  else if (a === '--viewport') {
    const m = /^(\d+)x(\d+)$/.exec(argv[++i] ?? '')
    if (!m) {
      console.error('--viewport expects <W>x<H>, e.g. 420x900')
      process.exit(2)
    }
    viewport = { width: Number(m[1]), height: Number(m[2]) }
  } else if (a === '--match-media') {
    const raw = argv[++i] ?? ''
    const eq = raw.lastIndexOf('=')
    if (eq < 0) {
      console.error('--match-media expects "<query>=<bool>", e.g. "(pointer: coarse)=true"')
      process.exit(2)
    }
    matchMedia[raw.slice(0, eq).trim()] = raw.slice(eq + 1).trim() === 'true'
  } else positionals.push(a)
}
const [url, outArg] = positionals
if (!url || !outArg) {
  console.error(
    'usage: shot.mjs <url> <out.png> [--wait ms] [--eval "expr"]... [--port n] [--color-scheme light|dark] [--viewport WxH] [--match-media "<query>=<bool>"]...',
  )
  process.exit(2)
}
if (colorScheme && colorScheme !== 'light' && colorScheme !== 'dark') {
  console.error('--color-scheme expects light or dark')
  process.exit(2)
}
const out = resolve(outArg)

const targets = await (await fetch(`http://localhost:${port}/json`)).json()
const pageTarget = targets.find((t) => t.type === 'page') ?? targets[0]
if (!pageTarget?.webSocketDebuggerUrl) {
  console.error(`no CDP page target on port ${port} — is headless_shell running there?`)
  process.exit(1)
}

const ws = new WebSocket(pageTarget.webSocketDebuggerUrl)
let id = 0
const pending = new Map()
const send = (method, params = {}) =>
  new Promise((res) => {
    const i = ++id
    pending.set(i, res)
    ws.send(JSON.stringify({ id: i, method, params }))
  })
await new Promise((r) => (ws.onopen = r))
ws.onmessage = (m) => {
  const d = JSON.parse(m.data)
  if (d.id && pending.has(d.id)) {
    pending.get(d.id)(d.result)
    pending.delete(d.id)
  }
}

await send('Runtime.enable')
await send('Page.enable')

// Emulation, applied before navigate so the page sees it from first paint.
if (colorScheme) {
  await send('Emulation.setEmulatedMedia', {
    features: [{ name: 'prefers-color-scheme', value: colorScheme }],
  })
}
if (viewport) {
  await send('Emulation.setDeviceMetricsOverride', {
    width: viewport.width,
    height: viewport.height,
    deviceScaleFactor: 1,
    mobile: false,
  })
}
if (Object.keys(matchMedia).length > 0) {
  // Stub window.matchMedia for the given queries before any page script runs, so a hook that reads
  // matchMedia at mount sees the override. Headless can't emulate some media features (e.g.
  // `pointer`), which is why this exists alongside --color-scheme (which Emulation handles).
  const source = `(function () {
    var overrides = ${JSON.stringify(matchMedia)};
    var norm = function (q) { return String(q).replace(/\\s+/g, '') };
    var keys = Object.keys(overrides).reduce(function (acc, k) { acc[norm(k)] = overrides[k]; return acc }, {});
    var real = window.matchMedia ? window.matchMedia.bind(window) : null;
    window.matchMedia = function (q) {
      var n = norm(q);
      if (Object.prototype.hasOwnProperty.call(keys, n)) {
        return {
          matches: keys[n], media: q, onchange: null,
          addEventListener: function () {}, removeEventListener: function () {},
          addListener: function () {}, removeListener: function () {},
          dispatchEvent: function () { return false },
        };
      }
      return real ? real(q) : { matches: false, media: q, addEventListener: function () {}, removeEventListener: function () {} };
    };
  })()`
  await send('Page.addScriptToEvaluateOnNewDocument', { source })
}

await send('Page.navigate', { url })
await new Promise((r) => setTimeout(r, waitMs))

for (const expr of evals) {
  const r = await send('Runtime.evaluate', {
    expression: expr,
    returnByValue: true,
    awaitPromise: true,
  })
  const v = r?.result?.value
  console.log(typeof v === 'string' ? v : JSON.stringify(v))
}

const shot = await send('Page.captureScreenshot', { format: 'png' })
writeFileSync(out, Buffer.from(shot.data, 'base64'))
console.log(`wrote ${out}`)
ws.close()
process.exit(0)
