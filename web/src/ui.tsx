// Small presentational helpers shared across the app.
import { useLayoutEffect, useRef } from 'react'
import type { AgentStatus, TaskStatus } from './api'

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

// A single-line-looking textarea that grows with its content (up to maxHeight, then scrolls) —
// used for every comment/message composer so multi-line input isn't cramped in a fixed box.
// Enter submits (matching the old <input> composers); Shift+Enter inserts a newline.
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
  // Resize to fit content on every value change (including a reset to '' after submit, which
  // shrinks it back). Measuring requires clearing the height first so scrollHeight can drop.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(el.scrollHeight, maxHeight)}px`
  }, [value, maxHeight])
  return (
    <textarea
      ref={ref}
      rows={1}
      value={value}
      disabled={disabled}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' && !e.shiftKey && onSubmit) {
          e.preventDefault()
          onSubmit()
        }
      }}
      className={`resize-none ${className ?? ''}`}
    />
  )
}
