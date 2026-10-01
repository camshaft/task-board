#!/usr/bin/env node
// Declarative local-board seeder for stateful CDP verifies -- the server-STATE sibling of
// shot.mjs (which drives the browser). Instead of hand-rolling a deeply-escaped curl+node heredoc
// to POST a fixture, describe the state as JSON and let this create it, resolving cross-references
// by alias. Dependency-free: Node 22 gives a global `fetch`; everything else is node: builtins.
//
// Usage:
//   node web/scripts/seed.mjs <spec.json|-> [--serve] [options]   seed (optionally boot a board)
//   node web/scripts/seed.mjs --stop <pidfile>                    kill a --serve'd board + clean tmp
//
//   --serve                boot a throwaway task-board (temp sqlite + generated cfg) and KEEP it
//                          running for the verify; prints its pid + pidfile. Without --serve, seed
//                          an already-running board at --base.
//   --base <url>           board base URL to seed (default http://127.0.0.1:<port> when --serve)
//   --bin <path>           task-board binary for --serve (default ./target/debug/task-board)
//   --web-dir <path>       built UI assets to serve for --serve (default web/dist), so shot.mjs can
//                          load the SPA at <base>/tasks/<id>
//   --port <n>             port for --serve (default: a free ephemeral port is chosen)
//   --db <path>            sqlite path for --serve (default: a fresh temp file)
//   --pidfile <path>       where to write the server pid for --serve (default: under the temp dir)
//   --wait-health <ms>     how long to wait for the server to answer /api/projects (default 20000)
//
// The spec is JSON (from the file arg or stdin via `-`). Each section is created in dependency
// order; an entry's `alias` is remembered so later entries can reference it:
//   {
//     "agents":    [{ "agent_id": "alice", "charter": "...", "metadata": {} }],
//     "projects":  [{ "alias": "p",  "name": "demo", "created_by": "human" }],
//     "documents": [{ "alias": "d",  "title": "Doc", "cid": "Qm...", "project": "p",
//                     "versions": [{ "cid": "Qm...", "summary": "v2" }] }],
//     "tasks":     [{ "alias": "t",  "project": "p", "title": "T", "status": "todo",
//                     "assignee": "alice", "priority": "high", "parent": "t0" }],
//     "questions": [{ "alias": "q",  "task": "t", "kind": "yes_no", "prompt": "Ship?",
//                     "routed_to": "operator", "blocking": true,
//                     "options": [{ "id": "a", "label": "A" }], "ui": {...},
//                     "response_schema": {...}, "default": ..., "wait_period_seconds": ... }],
//     "answers":   [{ "question": "q", "shape": "bool", "value": true, "actor": "operator" }],
//     "comments":  [{ "task": "t", "body": "hi", "author": "alice" }],
//     "reviews":   [{ "alias": "r", "kind": "document", "target_ref": "doc:d", "status": "open" }]
//   }
// A question's alias resolves to its comment id (what /answer and reply_to use). In a review's
// `target_ref`, "task:<alias>" / "doc:<alias>" / "project:<alias>" expand to task_<id> etc.
//
// On success prints a JSON summary to stdout: { baseUrl, serverPid?, pidfile?, tmpDir?, ids,
// taskUrls } -- parse it (jq/python) or eyeball it, then point shot.mjs at a taskUrl. When --serve
// is used the board is left RUNNING (node exits, the child is detached); tear it down with
// `--stop <pidfile>` (or kill the pid) when the verify is done.

import { spawn } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const argv = process.argv.slice(2)
const opts = { serve: false, waitHealth: 20000 }
const positionals = []
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--serve') opts.serve = true
  else if (a === '--base') opts.base = argv[++i]
  else if (a === '--bin') opts.bin = argv[++i]
  else if (a === '--web-dir') opts.webDir = argv[++i]
  else if (a === '--port') opts.port = Number(argv[++i])
  else if (a === '--db') opts.db = argv[++i]
  else if (a === '--pidfile') opts.pidfile = argv[++i]
  else if (a === '--wait-health') opts.waitHealth = Number(argv[++i])
  else if (a === '--stop') opts.stop = argv[++i]
  else positionals.push(a)
}

const die = (msg) => {
  console.error(msg)
  process.exit(1)
}

// --- teardown mode: kill a previously --serve'd board and clean its temp dir ---
if (opts.stop != null) {
  const pidfile = opts.stop
  if (!existsSync(pidfile)) die(`--stop: no pidfile at ${pidfile}`)
  const info = JSON.parse(readFileSync(pidfile, 'utf8'))
  try {
    process.kill(info.pid)
  } catch (e) {
    console.error(`kill ${info.pid}: ${e instanceof Error ? e.message : e} (already gone?)`)
  }
  if (info.tmpDir) rmSync(info.tmpDir, { recursive: true, force: true })
  rmSync(pidfile, { force: true })
  console.log(JSON.stringify({ stopped: info.pid, cleaned: info.tmpDir ?? null }))
  process.exit(0)
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// Pick a free TCP port by letting the OS assign one on a throwaway listener.
function freePort() {
  return new Promise((res, rej) => {
    const srv = createServer()
    srv.on('error', rej)
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address()
      srv.close(() => res(port))
    })
  })
}

async function waitForHealth(base, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      const r = await fetch(`${base}/api/projects`)
      if (r.ok) return
    } catch {
      // not up yet
    }
    await sleep(250)
  }
  throw new Error(`board at ${base} did not become healthy within ${timeoutMs}ms`)
}

let tmpDir = null
let serverPid = null
let pidfile = null

async function maybeServe() {
  if (!opts.serve) {
    if (!opts.base) die('without --serve you must pass --base <url> of a running board')
    return opts.base
  }
  const bin = resolve(opts.bin ?? './target/debug/task-board')
  if (!existsSync(bin)) die(`--serve: binary not found at ${bin} (build it, or pass --bin)`)
  const webDir = resolve(opts.webDir ?? 'web/dist')
  const port = opts.port ?? (await freePort())
  tmpDir = mkdtempSync(join(tmpdir(), 'tb-seed-'))
  const db = opts.db ?? join(tmpDir, 'board.db')
  const cfg = join(tmpDir, 'config.toml')
  writeFileSync(cfg, `db_path = "${db}"\nhost = "127.0.0.1"\nport = ${port}\n`)
  const log = join(tmpDir, 'server.log')
  const logFd = (await import('node:fs')).openSync(log, 'a')
  const child = spawn(bin, ['--config', cfg, '--web-dir', webDir], {
    detached: true,
    stdio: ['ignore', logFd, logFd],
  })
  child.unref()
  serverPid = child.pid
  const base = `http://127.0.0.1:${port}`
  pidfile = opts.pidfile ?? join(tmpDir, 'server.pid')
  writeFileSync(pidfile, JSON.stringify({ pid: serverPid, base, tmpDir, log }))
  try {
    await waitForHealth(base, opts.waitHealth)
  } catch (e) {
    try {
      process.kill(serverPid)
    } catch {
      // ignore
    }
    die(`${e instanceof Error ? e.message : e}\n--- server log (${log}) ---\n${readFileSync(log, 'utf8').slice(-2000)}`)
  }
  return base
}

function loadSpec() {
  const src = positionals[0]
  let raw
  if (!src || src === '-') {
    raw = readFileSync(0, 'utf8') // stdin
  } else {
    raw = readFileSync(resolve(src), 'utf8')
  }
  if (!raw.trim()) return {}
  return JSON.parse(raw)
}

async function run() {
  const base = await maybeServe()
  const spec = loadSpec()

  const post = async (path, body) => {
    const r = await fetch(`${base}${path}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    const text = await r.text()
    if (!r.ok) throw new Error(`POST ${path} -> ${r.status}: ${text.slice(0, 300)}`)
    return text ? JSON.parse(text) : {}
  }

  // alias -> created id, per entity kind
  const ids = { projects: {}, tasks: {}, questions: {}, documents: {}, reviews: {}, agents: {} }
  const need = (kind, alias, forWhat) => {
    const id = ids[kind][alias]
    if (id == null) throw new Error(`${forWhat} references unknown ${kind} alias "${alias}"`)
    return id
  }

  // agents (no alias needed, but record agent_id)
  for (const a of spec.agents ?? []) {
    if (!a.agent_id) throw new Error('agent entry needs an agent_id')
    await post('/api/agents', {
      agent_id: a.agent_id,
      display_name: a.display_name,
      kind: a.kind,
      charter: a.charter,
      metadata: a.metadata,
    })
    ids.agents[a.agent_id] = a.agent_id
  }

  // projects
  for (const p of spec.projects ?? []) {
    const r = await post('/api/projects', {
      name: p.name,
      description: p.description,
      created_by: p.created_by,
      metadata: p.metadata,
    })
    if (p.alias) ids.projects[p.alias] = r.id
  }

  // documents (+ versions, + optional path)
  for (const d of spec.documents ?? []) {
    const r = await post('/api/documents', {
      title: d.title,
      cid: d.cid,
      content: d.content,
      content_type: d.content_type,
      project_id: d.project != null ? need('projects', d.project, 'document.project') : d.project_id,
      summary: d.summary,
      created_by: d.created_by,
      metadata: d.metadata,
    })
    if (d.alias) ids.documents[d.alias] = r.id
    for (const v of d.versions ?? []) {
      await post(`/api/documents/${r.id}/versions`, {
        cid: v.cid,
        content: v.content,
        content_type: v.content_type,
        summary: v.summary,
        created_by: v.created_by,
      })
    }
    if (d.path != null) await post(`/api/documents/${r.id}/path`, { path: d.path, actor: d.created_by })
  }

  // tasks (parent + project by alias)
  for (const t of spec.tasks ?? []) {
    const r = await post('/api/tasks', {
      project_id: t.project != null ? need('projects', t.project, 'task.project') : t.project_id,
      title: t.title,
      description: t.description,
      assignee: t.assignee,
      priority: t.priority,
      created_by: t.created_by,
      metadata: t.metadata,
      parent_id: t.parent != null ? need('tasks', t.parent, 'task.parent') : t.parent_id,
    })
    if (t.alias) ids.tasks[t.alias] = r.id
    // a task may carry a non-default status, applied with a follow-up update
    if (t.status && t.status !== 'todo') {
      await fetch(`${base}/api/tasks/${r.id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ status: t.status, actor: t.created_by }),
      })
    }
  }

  // questions (task by alias; alias -> the question comment id)
  for (const q of spec.questions ?? []) {
    const taskId = need('tasks', q.task, 'question.task')
    const r = await post(`/api/tasks/${taskId}/questions`, {
      kind: q.kind,
      prompt: q.prompt,
      options: q.options,
      routed_to: q.routed_to,
      blocking: q.blocking,
      default: q.default,
      wait_period_seconds: q.wait_period_seconds,
      response_schema: q.response_schema,
      ui: q.ui,
      actor: q.actor,
    })
    if (q.alias) ids.questions[q.alias] = r.id
  }

  // answers (question alias -> its comment id)
  for (const a of spec.answers ?? []) {
    const cid = need('questions', a.question, 'answer.question')
    await post(`/api/comments/${cid}/answer`, { shape: a.shape, value: a.value, actor: a.actor })
  }

  // plain comments
  for (const c of spec.comments ?? []) {
    const taskId = need('tasks', c.task, 'comment.task')
    await post(`/api/tasks/${taskId}/comments`, { body: c.body, author: c.author })
  }

  // reviews (target_ref supports task:/doc:/project: alias expansion)
  const expandRef = (ref) => {
    if (typeof ref !== 'string') return ref
    const m = /^(task|doc|project):(.+)$/.exec(ref)
    if (!m) return ref
    const [, kind, alias] = m
    if (kind === 'task') return `task_${need('tasks', alias, 'review.target_ref')}`
    if (kind === 'doc') return `doc_${need('documents', alias, 'review.target_ref')}`
    return `project_${need('projects', alias, 'review.target_ref')}`
  }
  for (const rv of spec.reviews ?? []) {
    const r = await post('/api/reviews', {
      kind: rv.kind,
      source: rv.source,
      target_ref: expandRef(rv.target_ref),
      title: rv.title,
      status: rv.status,
      created_by: rv.created_by,
      assignee: rv.assignee,
      metadata: rv.metadata,
    })
    if (rv.alias) ids.reviews[rv.alias] = r.id
  }

  const taskUrls = {}
  for (const [alias, id] of Object.entries(ids.tasks)) taskUrls[alias] = `${base}/tasks/${id}`

  console.log(
    JSON.stringify({ baseUrl: base, serverPid, pidfile, tmpDir, ids, taskUrls }, null, 2),
  )
}

run().catch((e) => {
  // On a seed failure during --serve, leave the server up only if it was already healthy enough to
  // seed against; otherwise maybeServe already tore it down. Surface the error either way.
  console.error(e instanceof Error ? e.stack : String(e))
  if (serverPid && pidfile) {
    console.error(`note: a board may still be running (pid ${serverPid}); stop it with --stop ${pidfile}`)
  }
  process.exit(1)
})
