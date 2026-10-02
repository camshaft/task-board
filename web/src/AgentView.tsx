import { useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'
import { useBoardContext } from './Layout'
import {
  eventHref,
  openDm,
  requestStandDown,
  sendDirectMessage,
  useAgent,
  useAgentActivity,
  useAgentTasks,
  useProjects,
  updateAgent,
} from './resources'
import { Markdown } from './markdown'
import { AGENT_DOT, AutoGrowTextarea, relTime, StatusChip } from './ui'

// Per-agent page (/agents/:agentId): identity + presence, charter, registry metadata (repos),
// and the tasks currently assigned to this agent across every project. Read-only; backed by the
// resource store, so the assigned-task list live-updates as tasks change (it's keyed under the
// `tasks:` prefix the mutation choke point already invalidates).
export default function AgentView() {
  const { agentId } = useParams()
  const { actor } = useBoardContext()
  const id = agentId ?? ''
  const { data: agent, error, loading } = useAgent(id)
  const { data: tasks = [] } = useAgentTasks(id)
  const { data: projects = [] } = useProjects()
  // This agent's own actions, server-filtered by actor (complete, not window-truncated).
  const { data: activity = [] } = useAgentActivity(id)
  // "Open DM": resolve-or-create the private 1:1 channel with this agent on click (POST /api/dms),
  // then navigate to it. Resolved on click (not on mount) so merely viewing a page never creates
  // an empty DM; the endpoint is idempotent + order-independent, so it reuses any existing DM.
  const navigate = useNavigate()
  const [dmBusy, setDmBusy] = useState(false)
  // One shared error banner for the header actions (open DM / request spin-down).
  const [opError, setOpError] = useState<string | null>(null)

  async function openDmChannel() {
    if (dmBusy) return
    setDmBusy(true)
    setOpError(null)
    try {
      const ch = await openDm({ agent_a: actor, agent_b: id })
      navigate(`/channels/${ch.id}`)
    } catch (e) {
      setOpError((e as Error).message)
    } finally {
      setDmBusy(false)
    }
  }

  // Quick nudge: a small inline composer that sends this agent a direct message (which also
  // creates the DM channel on first send).
  const [nudgeOpen, setNudgeOpen] = useState(false)
  const [nudge, setNudge] = useState('')
  const [sending, setSending] = useState(false)
  const [nudgeError, setNudgeError] = useState<string | null>(null)
  const [nudgeSent, setNudgeSent] = useState(false)

  async function sendNudge() {
    const body = nudge.trim()
    if (!body || sending) return
    setSending(true)
    setNudgeError(null)
    setNudgeSent(false)
    try {
      await sendDirectMessage({ from_agent: actor, to_agent: id, body })
      setNudge('')
      setNudgeSent(true)
    } catch (e) {
      setNudgeError((e as Error).message)
    } finally {
      setSending(false)
    }
  }

  // Request a graceful stand-down (a signal the agent observes in its loop — not a kill). The
  // request stays pending on the agent until it honors it by going offline, which clears it.
  const standDownPending = agent?.stand_down_requested_at != null

  async function requestSpinDown() {
    if (sending || standDownPending) return
    if (
      !window.confirm(
        `Request ${id} to gracefully stand down? This asks the agent to finish up and go offline — it does not force-stop it.`,
      )
    )
      return
    const reason = window.prompt('Reason (optional):') ?? undefined
    setSending(true)
    setOpError(null)
    try {
      await requestStandDown(id, { principal: actor, reason: reason || undefined })
    } catch (e) {
      setOpError((e as Error).message)
    } finally {
      setSending(false)
    }
  }
  const projectName = (pid: number | null | undefined) =>
    pid == null ? '' : (projects.find((p) => p.id === pid)?.name ?? `#${pid}`)

  // Metadata editor: null = not editing, string = editing this JSON draft (merged server-side).
  const [editMeta, setEditMeta] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)

  async function saveMeta() {
    if (editMeta === null) return
    let parsed: unknown
    try {
      parsed = JSON.parse(editMeta)
    } catch {
      setSaveError('Metadata must be valid JSON.')
      return
    }
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
      setSaveError('Metadata must be a JSON object.')
      return
    }
    setBusy(true)
    setSaveError(null)
    try {
      await updateAgent(id, { metadata: parsed as Record<string, unknown> })
      setEditMeta(null)
    } catch (e) {
      setSaveError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  // Registry repos, if present: metadata.repos = [{ repo, branch }, ...].
  const repos = Array.isArray(agent?.metadata?.repos)
    ? (agent!.metadata.repos as { repo?: string; branch?: string }[])
    : []

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Home
        </Link>
        {agent && (
          <span
            className={`size-2 rounded-full ${AGENT_DOT[agent.status] ?? 'bg-zinc-600'}`}
            title={agent.status}
          />
        )}
        <h1 className="truncate text-sm font-semibold">
          {agent?.display_name || id}
        </h1>
        <span className="font-mono text-xs text-[var(--color-muted)]">{id}</span>
        {/* Contact actions. Hidden on your own page (no self-DM/nudge). */}
        {agent && actor && id !== actor && (
          <div className="ml-auto flex items-center gap-2">
            <button
              onClick={openDmChannel}
              disabled={dmBusy}
              className="rounded-md px-2.5 py-1 text-xs text-sky-400 ring-1 ring-inset ring-sky-500/40 hover:bg-sky-500/10 disabled:opacity-40"
            >
              Open DM
            </button>
            <button
              onClick={() => setNudgeOpen((o) => !o)}
              className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-600"
            >
              Nudge
            </button>
            <button
              onClick={requestSpinDown}
              disabled={sending || standDownPending}
              className="rounded-md px-2.5 py-1 text-xs text-amber-300 ring-1 ring-inset ring-amber-500/40 hover:bg-amber-500/10 disabled:opacity-40"
            >
              {standDownPending ? 'Spin-down requested' : 'Request spin-down'}
            </button>
          </div>
        )}
      </div>

      {/* Pending graceful stand-down request (a signal; the agent clears it by going offline). */}
      {agent && standDownPending && (
        <div className="border-b border-amber-500/30 bg-amber-500/10 px-5 py-2 text-xs text-amber-200">
          Spin-down requested {relTime(agent.stand_down_requested_at ?? null)}
          {agent.stand_down_requested_by ? ` by ${agent.stand_down_requested_by}` : ''}
          {agent.stand_down_reason ? ` — ${agent.stand_down_reason}` : ''}. Waiting for the agent to
          go offline.
        </div>
      )}
      {opError && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {opError}
        </div>
      )}

      {/* Quick nudge composer: a direct message without leaving the agent page. */}
      {nudgeOpen && agent && actor && id !== actor && (
        <div className="border-b border-[var(--color-border)] bg-[var(--color-panel)]/40 px-5 py-2">
          {nudgeError && (
            <div className="mb-2 rounded-md bg-rose-500/15 px-3 py-2 text-sm text-rose-300">
              {nudgeError}
            </div>
          )}
          <div className="flex items-end gap-2">
            <AutoGrowTextarea
              value={nudge}
              onChange={(v) => {
                setNudge(v)
                setNudgeSent(false)
              }}
              onSubmit={sendNudge}
              placeholder={`Message ${id} as ${actor}…`}
              className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
            />
            <button
              onClick={sendNudge}
              disabled={sending || !nudge.trim()}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
            >
              Send
            </button>
            <button
              onClick={() => {
                setNudgeOpen(false)
                setNudge('')
                setNudgeError(null)
              }}
              className="rounded-md px-2 py-2 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
            >
              Cancel
            </button>
          </div>
          {nudgeSent && <p className="mt-1 text-[11px] text-emerald-300">Message sent.</p>}
        </div>
      )}

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error.message}
        </div>
      )}
      {loading && !agent && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      {agent && (
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          <dl className="mb-5 grid grid-cols-3 gap-y-2 text-sm">
            <dt className="text-[var(--color-muted)]">Status</dt>
            <dd className="col-span-2">
              {agent.status}
              {agent.status_message && (
                <span className="text-[var(--color-muted)]"> · {agent.status_message}</span>
              )}
            </dd>
            <dt className="text-[var(--color-muted)]">Kind</dt>
            <dd className="col-span-2">{agent.kind ?? '—'}</dd>
            <dt className="text-[var(--color-muted)]">Last seen</dt>
            <dd className="col-span-2">{relTime(agent.last_seen)}</dd>
            <dt className="text-[var(--color-muted)]">Registered</dt>
            <dd className="col-span-2">{relTime(agent.created_at)}</dd>
            {agent.webhook_url && (
              <>
                <dt className="text-[var(--color-muted)]">Webhook</dt>
                <dd className="col-span-2 truncate font-mono text-xs">{agent.webhook_url}</dd>
              </>
            )}
          </dl>

          {repos.length > 0 && (
            <div className="mb-5">
              <div className="mb-1 text-xs text-[var(--color-muted)]">Repos</div>
              <ul className="space-y-1">
                {repos.map((r, i) => (
                  <li key={i} className="font-mono text-xs">
                    {r.repo ?? '?'}
                    {r.branch && <span className="text-[var(--color-muted)]"> @ {r.branch}</span>}
                  </li>
                ))}
              </ul>
            </div>
          )}

          {agent.charter && (
            <div className="mb-5">
              <div className="mb-1 text-xs text-[var(--color-muted)]">Charter</div>
              <Markdown source={agent.charter} className="text-sm" />
            </div>
          )}

          <div>
            <div className="mb-2 text-xs text-[var(--color-muted)]">
              Assigned tasks ({tasks.length})
            </div>
            <ul className="space-y-1.5">
              {tasks.map((t) => (
                <li key={t.id}>
                  <Link
                    to={t.project_id != null ? `/projects/${t.project_id}/tasks/${t.id}` : '#'}
                    className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 hover:border-sky-500/40"
                  >
                    <StatusChip status={t.status} />
                    <span className="min-w-0 flex-1 truncate text-sm">{t.title}</span>
                    {t.project_id != null && (
                      <span className="hidden text-xs text-[var(--color-muted)] sm:inline">
                        {projectName(t.project_id)}
                      </span>
                    )}
                    <span className="font-mono text-[11px] text-[var(--color-muted)]">#{t.id}</span>
                  </Link>
                </li>
              ))}
              {tasks.length === 0 && (
                <li className="text-sm text-[var(--color-muted)]">No assigned tasks.</li>
              )}
            </ul>
          </div>

          <div className="mt-6">
            <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
              <span>Metadata</span>
              {editMeta === null && (
                <button
                  onClick={() => {
                    setSaveError(null)
                    setEditMeta(JSON.stringify(agent.metadata ?? {}, null, 2))
                  }}
                  className="rounded px-1.5 py-0.5 text-sky-400 hover:bg-[var(--color-panel-2)]"
                >
                  edit
                </button>
              )}
            </div>
            {saveError && (
              <div className="mb-2 rounded-md bg-rose-500/15 px-3 py-2 text-sm text-rose-300">
                {saveError}
              </div>
            )}
            {editMeta === null ? (
              Object.keys(agent.metadata ?? {}).length > 0 ? (
                <pre className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 text-xs">
                  {JSON.stringify(agent.metadata, null, 2)}
                </pre>
              ) : (
                <p className="text-sm text-[var(--color-muted)]">— none —</p>
              )
            ) : (
              <div className="space-y-2">
                <textarea
                  autoFocus
                  value={editMeta}
                  rows={8}
                  onChange={(e) => setEditMeta(e.target.value)}
                  className="w-full rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-2 font-mono text-xs outline-none focus:border-sky-500/50"
                />
                <p className="text-[11px] text-[var(--color-muted)]">
                  Keys are merged into the agent's existing metadata server-side; this can't
                  remove a key.
                </p>
                <div className="flex gap-2">
                  <button
                    disabled={busy}
                    onClick={saveMeta}
                    className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
                  >
                    Save
                  </button>
                  <button
                    onClick={() => {
                      setEditMeta(null)
                      setSaveError(null)
                    }}
                    className="rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
                  >
                    Cancel
                  </button>
                </div>
              </div>
            )}
          </div>

          <div className="mt-6">
            <div className="mb-2 text-xs text-[var(--color-muted)]">
              Recent activity ({activity.length})
            </div>
            <ul className="space-y-2">
              {activity.map((e) => {
                const href = eventHref(e)
                const body = (
                  <>
                    <div className="flex items-center gap-1.5">
                      <span className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 font-mono text-[10px] text-sky-300">
                        {e.type}
                      </span>
                      <span className="text-[var(--color-muted)]">{relTime(e.created_at)}</span>
                    </div>
                    {typeof e.data?.title === 'string' && (
                      <div className="mt-0.5 text-[var(--color-muted)]">{e.data.title as string}</div>
                    )}
                  </>
                )
                return (
                  <li key={e.seq} className="text-xs">
                    {href ? (
                      <Link
                        to={href}
                        className="-mx-1 block rounded px-1 hover:bg-[var(--color-panel-2)]"
                      >
                        {body}
                      </Link>
                    ) : (
                      body
                    )}
                  </li>
                )
              })}
              {activity.length === 0 && (
                <li className="text-xs text-[var(--color-muted)]">
                  No recent activity by this agent.
                </li>
              )}
            </ul>
          </div>
        </div>
      )}
    </main>
  )
}
