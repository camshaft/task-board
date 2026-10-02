// Small presentational helpers shared across the app.
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { api } from './api'
import type { AgentStatus, TaskStatus } from './api'
import { useIdentityAliases } from './resources'

export const TASK_COLUMNS: TaskStatus[] = [
  'todo',
  'in_progress',
  'blocked',
  'done',
  'cancelled',
]

export const STATUS_LABEL: Record<TaskStatus, string> = {
  todo: 'To do',
  in_progress: 'In progress',
  blocked: 'Blocked',
  done: 'Done',
  cancelled: 'Cancelled',
}

// Tailwind classes for each task status chip.
export const STATUS_CHIP: Record<TaskStatus, string> = {
  todo: 'bg-slate-500/15 text-slate-300 ring-slate-500/30',
  in_progress: 'bg-sky-500/15 text-sky-300 ring-sky-500/30',
  blocked: 'bg-rose-500/15 text-rose-300 ring-rose-500/30',
  done: 'bg-emerald-500/15 text-emerald-300 ring-emerald-500/30',
  cancelled: 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30',
}

export const AGENT_DOT: Record<AgentStatus, string> = {
  online: 'bg-emerald-400',
  busy: 'bg-amber-400',
  away: 'bg-slate-400',
  offline: 'bg-zinc-600',
}

export function StatusChip({ status }: { status: TaskStatus }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${STATUS_CHIP[status]}`}
    >
      {STATUS_LABEL[status]}
    </span>
  )
}

export function PriorityDot({ priority }: { priority: string | null }) {
  if (!priority) return null
  const color =
    priority === 'high' || priority === 'urgent'
      ? 'bg-rose-400'
      : priority === 'low'
        ? 'bg-slate-500'
        : 'bg-amber-400'
  return (
    <span
      title={`priority: ${priority}`}
      className={`inline-block size-2 rounded-full ${color}`}
    />
  )
}

// Render the author of a post/comment. When `externalAuthor` is set — a bridged human (e.g. an
// ingested Slack user) — show that identity as the author with a subtle "via <ingester>" hint,
// so an ingested message reads as the person, not the fleet agent that relayed it (#141 §6).
// `resolveExternal` maps an external id (e.g. "slack:U123") to a display name; falls back to the
// bare id. Plain fleet-agent authors render unchanged.
export function AuthorLabel({
  author,
  externalAuthor,
  resolveExternal,
}: {
  author: string | null | undefined
  externalAuthor?: string | null
  resolveExternal?: (id: string) => string
}) {
  if (externalAuthor) {
    const name = resolveExternal ? resolveExternal(externalAuthor) : externalAuthor
    return (
      <span>
        <span className="font-mono">{name}</span>
        {author && <span className="text-[var(--color-muted)]"> · via {author}</span>}
      </span>
    )
  }
  return <span className="font-mono">{author ?? 'anon'}</span>
}

// Render an identity id, resolving a known alias to its canonical identity for DISPLAY (task 532).
// Generic: any alias -> canonical (operator -> cameron today; extensible for the multi-operator
// model). Shows the canonical, with a tooltip noting the alias so the original reference stays
// discoverable. Stored data (assignee, etc.) is never rewritten — this is display-only.
export function Identity({ id, className }: { id: string; className?: string }) {
  const { data: aliases } = useIdentityAliases()
  const hit = aliases?.find((a) => a.alias === id)
  if (!hit) return <span className={className}>{id}</span>
  return (
    <span className={className} title={`alias: ${hit.alias} -> ${hit.canonical}`}>
      {hit.canonical}
    </span>
  )
}

export function relTime(iso: string | null | undefined): string {
  if (!iso) return ''
  const then = new Date(iso).getTime()
  if (Number.isNaN(then)) return ''
  const s = Math.round((Date.now() - then) / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.round(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.round(h / 24)
  return `${d}d ago`
}

// True when the primary pointer is coarse (touch) — i.e. a phone/tablet. On such devices the
// on-screen keyboard's Enter should insert a newline (submit via the send button), matching Slack
// and most mobile text apps (task 435). Reactive, so a hybrid device that gains/loses a mouse
// updates. SSR-safe (matchMedia may be absent).
export function useCoarsePointer() {
  const [coarse, setCoarse] = useState(
    () => typeof window !== 'undefined' && !!window.matchMedia?.('(pointer: coarse)').matches,
  )
  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return
    const mq = window.matchMedia('(pointer: coarse)')
    const onChange = () => setCoarse(mq.matches)
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [])
  return coarse
}

// A single-line-looking textarea that grows with its content (up to maxHeight, then scrolls) —
// used for every comment/message composer so multi-line input isn't cramped in a fixed box.
// On desktop (fine pointer) Enter submits (matching the old <input> composers); on touch devices
// Enter inserts a newline and you submit via the send button (task 435). Shift+Enter always newlines.
// @-mention candidates (agents), fetched ONCE and shared across every composer — lazily, on the
// first '@' typed anywhere, so an idle board pays nothing. A failed fetch clears the promise so a
// later '@' retries. (task 474)
type MentionCandidate = { id: string; label: string }
let mentionAgentsCache: MentionCandidate[] | null = null
let mentionAgentsPromise: Promise<MentionCandidate[]> | null = null
function loadMentionAgents(): Promise<MentionCandidate[]> {
  if (mentionAgentsCache) return Promise.resolve(mentionAgentsCache)
  if (!mentionAgentsPromise) {
    mentionAgentsPromise = api
      .listAgents()
      .then((as) => {
        const mapped = as.map((a) => ({ id: a.id, label: a.display_name || a.id }))
        mentionAgentsCache = mapped
        return mapped
      })
      .catch(() => {
        mentionAgentsPromise = null // allow a retry on the next '@'
        return []
      })
  }
  return mentionAgentsPromise
}

// The @mention token being typed immediately before the caret, if any: an '@' at a token boundary
// (start-of-text or after whitespace) followed by mention-id chars ([A-Za-z0-9_-], matching the
// server's extract_mentions) up to the caret. Returns the '@' index + the partial query.
function activeMention(value: string, caret: number): { start: number; query: string } | null {
  const m = /(?:^|\s)@([A-Za-z0-9_-]*)$/.exec(value.slice(0, caret))
  if (!m) return null
  const query = m[1]
  return { start: caret - query.length - 1, query }
}

// Word-boundary chars in a handle/label, so a match that starts a word (concierge in "v-concierge",
// "Review" in "Review Bot") ranks above a mid-word one.
const MENTION_BOUNDARY = /[-_ /.]/

// Fuzzy-match a typed mention query against one candidate string, returning a rank score (higher is
// better) or null for no match. cameron asked for fuzzy (substring/typo-tolerant) auto-complete
// rather than exact/prefix (task_1151). Two tiers, both case-insensitive:
//   1. Contiguous substring -> high score, boosted for a prefix or word-boundary start and for an
//      earlier position. "cnc" matches "concierge"? no (not contiguous); "con" does, strongly.
//   2. Subsequence (query chars appear in order, gaps allowed) -> lower score, so a dropped letter
//      or an abbreviation still matches ("cncrge"/"camr" -> "concierge"/"cameron") but always ranks
//      below a real substring hit. Consecutive-run and word-boundary matches earn more.
// An empty query scores 0 for every candidate (the just-typed '@' lists everyone, as before).
function fuzzyScore(query: string, text: string): number | null {
  if (!query) return 0
  const q = query.toLowerCase()
  const t = text.toLowerCase()
  const idx = t.indexOf(q)
  if (idx !== -1) {
    let score = 1000 - Math.min(idx, 100)
    if (idx === 0) score += 500
    else if (MENTION_BOUNDARY.test(t[idx - 1])) score += 250
    return score
  }
  let ti = 0
  let score = 0
  let streak = 0
  let prev = -2
  for (const c of q) {
    let found = -1
    for (let j = ti; j < t.length; j++) {
      if (t[j] === c) {
        found = j
        break
      }
    }
    if (found === -1) return null
    if (found === prev + 1) {
      streak++
      score += 10 + streak * 5
    } else {
      streak = 0
      score += 1
    }
    if (found === 0 || MENTION_BOUNDARY.test(t[found - 1])) score += 15
    prev = found
    ti = found + 1
  }
  return score
}

export function AutoGrowTextarea({
  value,
  onChange,
  onSubmit,
  placeholder,
  disabled,
  className,
  maxHeight = 200,
}: {
  value: string
  onChange: (v: string) => void
  onSubmit?: () => void
  placeholder?: string
  disabled?: boolean
  className?: string
  maxHeight?: number
}) {
  const ref = useRef<HTMLTextAreaElement>(null)
  const coarsePointer = useCoarsePointer()
  // @mention typeahead state (task 474). `mention` is the active partial token; `sel` the
  // highlighted candidate. Agents load lazily into `agents` on the first '@'.
  const [agents, setAgents] = useState<MentionCandidate[]>(mentionAgentsCache ?? [])
  const [mention, setMention] = useState<{ start: number; query: string } | null>(null)
  const [sel, setSel] = useState(0)

  // Fuzzy-rank the candidates against the active query (task_1151): score each on the better of its
  // id or label, drop non-matches, and sort best-first (ties -> shorter label, then id) so the
  // strongest match is pre-selected at the top. Top 8 only, to keep the dropdown compact.
  const candidates = mention
    ? agents
        .map((a) => ({
          a,
          score: Math.max(
            fuzzyScore(mention.query, a.id) ?? -Infinity,
            fuzzyScore(mention.query, a.label) ?? -Infinity,
          ),
        }))
        .filter((x) => x.score > -Infinity)
        .sort(
          (x, y) =>
            y.score - x.score || x.a.label.length - y.a.label.length || x.a.id.localeCompare(y.a.id),
        )
        .slice(0, 8)
        .map((x) => x.a)
    : []
  const open = candidates.length > 0

  // Resize to fit content on every value change (including a reset to '' after submit, which
  // shrinks it back). Measuring requires clearing the height first so scrollHeight can drop.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    // Only scroll once the content actually exceeds maxHeight; otherwise hide the y-overflow so a
    // single-line composer never shows a stray scrollbar (macOS/Chrome renders one at the exact
    // content height from sub-pixel rounding — the operator's top scrollbar complaint, task 518).
    el.style.overflowY = el.scrollHeight > maxHeight ? 'auto' : 'hidden'
    el.style.height = `${Math.min(el.scrollHeight, maxHeight)}px`
  }, [value, maxHeight])

  function recompute(v: string, caret: number) {
    const m = activeMention(v, caret)
    setMention(m)
    setSel(0)
    if (m && agents.length === 0) loadMentionAgents().then(setAgents)
  }

  function accept(a: MentionCandidate) {
    if (!mention) return
    const before = value.slice(0, mention.start)
    const after = value.slice(mention.start + 1 + mention.query.length)
    const insert = `@${a.id} `
    onChange(before + insert + after)
    setMention(null)
    const pos = before.length + insert.length
    requestAnimationFrame(() => {
      const el = ref.current
      if (el) {
        el.focus()
        el.selectionStart = el.selectionEnd = pos
      }
    })
  }

  return (
    // The wrapper takes the flex sizing every caller puts on the composer (flex-1 in a flex row);
    // the textarea fills it (w-full). This keeps the mention dropdown positioned relative to the
    // composer without changing any call site's layout.
    <div className="relative flex-1 min-w-0">
      <textarea
        ref={ref}
        rows={1}
        value={value}
        disabled={disabled}
        placeholder={placeholder}
        onChange={(e) => {
          onChange(e.target.value)
          recompute(e.target.value, e.target.selectionStart)
        }}
        onBlur={() => setMention(null)}
        onKeyDown={(e) => {
          // When the mention dropdown is open it captures navigation/accept keys FIRST, so Enter
          // picks a candidate rather than submitting.
          if (open) {
            if (e.key === 'ArrowDown') {
              e.preventDefault()
              setSel((s) => (s + 1) % candidates.length)
              return
            }
            if (e.key === 'ArrowUp') {
              e.preventDefault()
              setSel((s) => (s - 1 + candidates.length) % candidates.length)
              return
            }
            if (e.key === 'Enter' || e.key === 'Tab') {
              e.preventDefault()
              accept(candidates[Math.min(sel, candidates.length - 1)])
              return
            }
            if (e.key === 'Escape') {
              e.preventDefault()
              setMention(null)
              return
            }
          }
          // Touch devices: let Enter insert a newline (submit via the send button) — task 435.
          if (e.key === 'Enter' && !e.shiftKey && !coarsePointer && onSubmit) {
            e.preventDefault()
            onSubmit()
          }
        }}
        className={`w-full resize-none ${className ?? ''}`}
      />
      {open && (
        <ul className="absolute bottom-full left-0 z-20 mb-1 max-h-48 w-64 overflow-auto rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] py-1 text-sm shadow-lg">
          {candidates.map((a, i) => (
            <li key={a.id}>
              <button
                type="button"
                // mousedown (not click) so we accept before the textarea's blur closes the list.
                onMouseDown={(e) => {
                  e.preventDefault()
                  accept(a)
                }}
                className={`flex w-full flex-col items-start px-2 py-1 text-left ${
                  i === sel ? 'bg-sky-500/20' : 'hover:bg-[var(--color-panel)]'
                }`}
              >
                <span className="font-mono text-xs text-sky-300">@{a.id}</span>
                {a.label !== a.id && (
                  <span className="text-[11px] text-[var(--color-muted)]">{a.label}</span>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
