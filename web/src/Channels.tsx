import { Link } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { type Channel } from './api'
import { useBoardContext } from './Layout'
import { createChannel, useChannels } from './resources'
import { relTime } from './ui'

// Display label for a channel: named channels use their name; DMs (private, no name) fall back
// to a generic label (the other participant is shown in the channel view, which has members).
export function channelLabel(c: Channel): string {
  if (c.name) return c.name
  if (c.private) return 'Direct message'
  return `#${c.id}`
}

// The channels list (/channels): public channels plus the current actor's channels (incl.
// private/DMs). Backed by the resource store, so it live-updates as channels/posts arrive.
export default function Channels() {
  const scrollRef = useScrollRestoration()
  const { actor } = useBoardContext()
  const { data: pub = [], loading } = useChannels()
  const { data: mine = [] } = useChannels(actor)

  // Merge public + the actor's channels, deduped by id.
  const byId = new Map<number, Channel>()
  for (const c of [...pub, ...mine]) byId.set(c.id, c)
  const channels = [...byId.values()].sort((a, b) => a.id - b.id)

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
        <button
          onClick={newChannel}
          className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-500"
        >
          + channel
        </button>
      </div>
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
