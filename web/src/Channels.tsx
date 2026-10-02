import { useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { type Channel } from './api'
import { useBoardContext } from './Layout'
import { createChannel, openDm, useAgents, useChannels } from './resources'
import { AGENT_DOT, relTime } from './ui'

// Display label for a channel: named channels use their name; DMs (private, no name) fall back
// to a generic label (the other participant is shown in the channel view, which has members).
export function channelLabel(c: Channel): string {
  if (c.name) return c.name
  if (c.private) return 'Direct message'
  return `#${c.id}`
}

// The channels list (/channels): the messaging home — a prominent "message the concierge" shortcut
// and a new-DM agent picker up top (DMing an agent used to be buried behind Agents -> the agent's
// page -> Open DM; task_1084), then public channels plus the actor's channels (incl. private/DMs).
// Backed by the resource store, so it live-updates as channels/posts arrive.
export default function Channels() {
  const scrollRef = useScrollRestoration()
  const navigate = useNavigate()
  const { actor } = useBoardContext()
  const { data: pub = [], loading } = useChannels()
  const { data: mine = [] } = useChannels(actor)
  const { data: agents = [] } = useAgents()

  // Merge public + the actor's channels, deduped by id.
  const byId = new Map<number, Channel>()
  for (const c of [...pub, ...mine]) byId.set(c.id, c)
  const channels = [...byId.values()].sort((a, b) => a.id - b.id)

  // New-DM picker state. Resolve-or-create a 1:1 DM on click, then navigate to it; the endpoint is
  // idempotent, so this reuses any existing DM rather than ever creating a duplicate.
  const [picking, setPicking] = useState(false)
  const [query, setQuery] = useState('')
  const [dmBusy, setDmBusy] = useState(false)
  const [dmError, setDmError] = useState<string | null>(null)

  // The concierge is the operator's primary contact (the intended workflow is to DM it rather than
  // hand-create tasks), so offer a one-click shortcut whenever it's a registered agent.
  const hasConcierge = agents.some((a) => a.id === 'concierge')
  const canConcierge = hasConcierge && actor !== 'concierge'

  async function startDm(agentId: string) {
    if (dmBusy || !agentId || agentId === actor) return
    setDmBusy(true)
    setDmError(null)
    try {
      const ch = await openDm({ agent_a: actor, agent_b: agentId })
      navigate(`/channels/${ch.id}`)
    } catch (e) {
      setDmError((e as Error).message)
    } finally {
      setDmBusy(false)
    }
  }

  const q = query.trim().toLowerCase()
  const pickable = agents
    .filter((a) => a.id !== actor)
    .filter(
      (a) =>
        !q || a.id.toLowerCase().includes(q) || (a.display_name ?? '').toLowerCase().includes(q),
    )
    .sort((a, b) => a.id.localeCompare(b.id))

  async function newChannel() {
    const name = window.prompt('Channel name:')
    if (!name?.trim()) return
    try {
      await createChannel({ name: name.trim(), principal: actor })
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Channels</h1>
        <div className="ml-auto flex items-center gap-2">
          {canConcierge && (
            <button
              onClick={() => startDm('concierge')}
              disabled={dmBusy}
              className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-500 disabled:opacity-40"
            >
              Message concierge
            </button>
          )}
          <button
            onClick={() => {
              setPicking((v) => !v)
              setQuery('')
              setDmError(null)
            }}
            aria-expanded={picking}
            className="rounded-md px-2.5 py-1 text-xs text-sky-400 ring-1 ring-inset ring-sky-500/40 hover:bg-sky-500/10"
          >
            New DM
          </button>
          <button
            onClick={newChannel}
            className="rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] ring-1 ring-inset ring-[var(--color-border)] hover:bg-[var(--color-panel-2)]"
          >
            + channel
          </button>
        </div>
      </div>

      {/* New-DM agent picker: search the roster and open a 1:1 DM in one click. */}
      {picking && (
        <div className="border-b border-[var(--color-border)] bg-[var(--color-panel)]/40 px-5 py-3">
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Message an agent — type to filter…"
            className="mb-2 w-full rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
          />
          {dmError && <p className="mb-2 text-xs text-rose-400">{dmError}</p>}
          <ul className="max-h-64 space-y-1 overflow-y-auto">
            {pickable.map((a) => (
              <li key={a.id}>
                <button
                  onClick={() => startDm(a.id)}
                  disabled={dmBusy}
                  className="flex w-full items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 text-left hover:border-sky-500/40 disabled:opacity-40"
                >
                  <span className={`size-2 rounded-full ${AGENT_DOT[a.status] ?? 'bg-zinc-600'}`} />
                  <span className="min-w-0 flex-1 truncate text-sm">{a.display_name || a.id}</span>
                  <span className="font-mono text-[11px] text-[var(--color-muted)]">{a.id}</span>
                </button>
              </li>
            ))}
            {pickable.length === 0 && (
              <li className="px-1 py-2 text-sm text-[var(--color-muted)]">
                {agents.length === 0 ? 'No agents registered yet.' : 'No agents match.'}
              </li>
            )}
          </ul>
        </div>
      )}

      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        {loading && channels.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">Loading…</p>
        )}
        {!loading && channels.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No channels yet.</p>
        )}
        <ul className="space-y-1.5">
          {channels.map((c) => (
            <li key={c.id}>
              <Link
                to={`/channels/${c.id}`}
                className="flex items-center gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 hover:border-sky-500/40"
              >
                <span className="text-[var(--color-muted)]">{c.private ? '🔒' : '#'}</span>
                <span className="min-w-0 flex-1 truncate text-sm">{channelLabel(c)}</span>
                {c.topic && (
                  <span className="hidden truncate text-xs text-[var(--color-muted)] md:inline">
                    {c.topic}
                  </span>
                )}
                {c.member_count != null && (
                  <span className="text-[11px] text-[var(--color-muted)]">
                    {c.member_count} member{c.member_count === 1 ? '' : 's'}
                  </span>
                )}
                <span className="text-[11px] text-[var(--color-muted)]">{relTime(c.created_at)}</span>
              </Link>
            </li>
          ))}
        </ul>
      </div>
    </main>
  )
}
