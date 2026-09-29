import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type ChannelPost } from './api'
import { channelLabel } from './Channels'
import { useBoardContext } from './Layout'
import { Markdown } from './markdown'
import { inviteToChannel, postToChannel, useChannel, useChannelPosts } from './resources'
import { relTime } from './ui'

// One channel (/channels/:channelId): header (label, topic, members, invite), a threaded post
// pane (one level of reply_to nesting), and a composer. Backed by the resource store; posts
// live-update via the channel.* / message.direct SSE path (channel_id → touched).
export default function ChannelView() {
  const { channelId } = useParams()
  const { actor } = useBoardContext()
  const id = Number(channelId)
  const { data: channel, error: chErr, loading } = useChannel(id)
  const { data: posts = [] } = useChannelPosts(id)
  const [draft, setDraft] = useState('')
  const [replyTo, setReplyTo] = useState<number | null>(null)
  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)

  const author = (p: ChannelPost) => p.data.from ?? p.actor ?? 'anon'

  async function send() {
    const body = draft.trim()
    if (!body) return
    setBusy(true)
    setActionError(null)
    try {
      await postToChannel(id, { sender: actor, body, reply_to: replyTo ?? undefined })
      setDraft('')
      setReplyTo(null)
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function invite() {
    const who = window.prompt('Invite which agent? (agent id)')
    if (!who?.trim()) return
    setBusy(true)
    setActionError(null)
    try {
      await inviteToChannel(id, { agent_id: who.trim(), invited_by: actor })
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  // Thread one level: top-level posts (no reply_to) in order, each followed by replies whose
  // reply_to points at its seq. Any reply whose parent isn't present renders at top level.
  const bySeq = new Set(posts.map((p) => p.seq))
  const topLevel = posts.filter((p) => p.data.reply_to == null || !bySeq.has(p.data.reply_to))
  const repliesOf = (seq: number) => posts.filter((p) => p.data.reply_to === seq)

  // A DM (private, no name): title with the other member if the channel carries members.
  const otherMember = channel?.members?.find((m) => m !== actor)
  const title =
    channel && channel.private && !channel.name && otherMember
      ? `DM · ${otherMember}`
      : channel
        ? channelLabel(channel)
        : `Channel #${id}`
  const error = actionError ?? chErr?.message ?? null

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/channels" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Channels
        </Link>
        <h1 className="truncate text-sm font-semibold">{title}</h1>
        {channel?.topic && (
          <span className="hidden truncate text-xs text-[var(--color-muted)] md:inline">
            {channel.topic}
          </span>
        )}
        {channel && !channel.private && (
          <button
            onClick={invite}
            disabled={busy}
            className="ml-auto rounded-md px-2 py-1 text-xs text-sky-400 hover:bg-[var(--color-panel-2)]"
          >
            + invite
          </button>
        )}
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error}
        </div>
      )}
      {loading && !channel && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      {channel?.members && channel.members.length > 0 && (
        <div className="border-b border-[var(--color-border)] px-5 py-1.5 text-[11px] text-[var(--color-muted)]">
          Members: <span className="font-mono">{channel.members.join(', ')}</span>
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        <ul className="space-y-2">
          {topLevel.map((p) => (
            <li key={p.seq}>
              <PostRow p={p} author={author(p)} onReply={() => setReplyTo(p.seq)} />
              {repliesOf(p.seq).length > 0 && (
                <ul className="mt-1.5 space-y-1.5 border-l border-[var(--color-border)] pl-4">
                  {repliesOf(p.seq).map((r) => (
                    <li key={r.seq}>
                      <PostRow p={r} author={author(r)} />
                    </li>
                  ))}
                </ul>
              )}
            </li>
          ))}
          {posts.length === 0 && (
            <li className="text-sm text-[var(--color-muted)]">No messages yet.</li>
          )}
        </ul>
      </div>

      <div className="border-t border-[var(--color-border)] p-4">
        <div className="flex gap-2">
          <input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && send()}
            placeholder={
              replyTo != null ? `Reply to #${replyTo} as ${actor}…` : `Message as ${actor}…`
            }
            className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
          />
          {replyTo != null && (
            <button
              onClick={() => setReplyTo(null)}
              className="rounded-md px-2 py-2 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]"
            >
              cancel reply
            </button>
          )}
          <button
            onClick={send}
            disabled={busy || !draft.trim()}
            className="rounded-md bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
          >
            Send
          </button>
        </div>
      </div>
    </main>
  )
}

function PostRow({ p, author, onReply }: { p: ChannelPost; author: string; onReply?: () => void }) {
  return (
    <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3">
      <div className="mb-1 flex items-center gap-2 text-xs text-[var(--color-muted)]">
        <span className="font-mono">{author}</span>
        <span>· {relTime(p.created_at)}</span>
        {onReply && (
          <button onClick={onReply} className="ml-auto hover:text-sky-300">
            reply
          </button>
        )}
      </div>
      <Markdown source={p.data.body ?? ''} className="text-sm" />
    </div>
  )
}
