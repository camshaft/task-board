#!/usr/bin/env node
// Parameterized CDP screenshot + optional DOM-eval driver for verifying the task-board web UI
// against a running headless_shell. Encapsulates the WebSocket plumbing (connect -> navigate ->
// wait -> optional Runtime.evaluate asserts -> captureScreenshot) so a verify tick doesn't
// re-hand-roll it (and re-hit the env-not-forwarded / output-path papercuts). Dependency-free:
// Node 22 gives a global `WebSocket` + `fetch`.
//
// Usage:
//   node web/scripts/shot.mjs <url> <out.png> [--wait <ms>] [--eval "<js-expr>"]... [--port <n>]
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
// - Why CDP over `headless_shell --screenshot`: that fires pre-hydration (blank), and
//   `--virtual-time-budget` hangs on the app's open SSE stream. Don't use either.

import { writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

const argv = process.argv.slice(2)
const positionals = []
const evals = []
let waitMs = 4000
let port = 9222
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--wait') waitMs = Number(argv[++i])
  else if (a === '--port') port = Number(argv[++i])
  else if (a === '--eval') evals.push(argv[++i])
  else positionals.push(a)
}
const [url, outArg] = positionals
if (!url || !outArg) {
  console.error('usage: shot.mjs <url> <out.png> [--wait ms] [--eval "expr"]... [--port n]')
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
