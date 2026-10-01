import { useState } from 'react'
import {
  type AnswerPayload,
  type Comment,
  type QuestionKind,
  type QuestionOption,
  type QuestionPayload,
  type QuestionState,
} from './api'
import { Markdown } from './markdown'
import { AuthorLabel, AutoGrowTextarea, relTime } from './ui'

// Operator-questions UI (task_629, consumer of the task_628 backend). Slice 1 is read-only: render
// a question comment (prompt, kind, options, routing, lifecycle state) and any answer replying to
// it, so a question posed on a task is legible in the thread. The answer/decline/cancel controls
// and the schema-driven answer form are later slices.

const KIND_LABEL: Record<QuestionKind, string> = {
  yes_no: 'Yes / no',
  multiple_choice: 'Multiple choice',
  select_all: 'Select all',
  fill_in_the_blank: 'Fill in the blank',
  rank_list: 'Rank list',
}

const STATE_CHIP: Record<QuestionState, string> = {
  open: 'bg-sky-500/15 text-sky-300 ring-sky-500/30',
  answered: 'bg-emerald-500/15 text-emerald-300 ring-emerald-500/30',
  answered_outside_frame: 'bg-amber-500/15 text-amber-300 ring-amber-500/30',
  declined: 'bg-rose-500/15 text-rose-300 ring-rose-500/30',
  cancelled: 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30',
  superseded: 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30',
}

function StateChip({ state }: { state: string }) {
  const chip = STATE_CHIP[state as QuestionState] ?? 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30'
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide ring-1 ring-inset ${chip}`}
    >
      {state.replace(/_/g, ' ')}
    </span>
  )
}

// Narrow a comment's opaque payload to a QuestionPayload (best-effort; the backend validates on
// write, so a question comment always carries at least a kind).
function asQuestion(payload: unknown): QuestionPayload | null {
  if (payload && typeof payload === 'object' && 'kind' in payload) {
    return payload as QuestionPayload
  }
  return null
}

function asAnswer(payload: unknown): AnswerPayload {
  if (payload && typeof payload === 'object') return payload as AnswerPayload
  return {}
}

// Render an answer's value by its shape: bool as yes/no, a choice/ranked list as a list (option ids
// resolved to their labels when the question's options are known), text verbatim, anything else as
// pretty JSON. Falls back to the comment body (the answer summary) when there is no structured value.
function AnswerValue({
  answer,
  body,
  options,
}: {
  answer: AnswerPayload
  body: string
  options?: QuestionOption[]
}) {
  const v = answer.value
  const labelFor = (id: unknown) =>
    (typeof id === 'string' && options?.find((o) => o.id === id)?.label) ||
    (typeof id === 'string' ? id : JSON.stringify(id))
  if (v == null) return <Markdown source={body} className="text-sm" />
  if (typeof v === 'boolean') return <span className="text-sm font-medium">{v ? 'Yes' : 'No'}</span>
  if (typeof v === 'string') return <span className="text-sm">{labelFor(v)}</span>
  if (Array.isArray(v)) {
    const ordered = answer.shape === 'ranked'
    const List = ordered ? 'ol' : 'ul'
    return (
      <List
        className={`ml-5 text-sm ${ordered ? 'list-decimal' : 'list-disc'} marker:text-[var(--color-muted)]`}
      >
        {v.map((item, i) => (
          <li key={i}>{labelFor(item)}</li>
        ))}
      </List>
    )
  }
  return (
    <pre className="overflow-x-auto rounded bg-[var(--color-panel)] p-2 font-mono text-xs">
      {JSON.stringify(v, null, 2)}
    </pre>
  )
}

function AnswerCard({
  answer,
  options,
  resolveExternal,
}: {
  answer: Comment
  options?: QuestionOption[]
  resolveExternal: (id: string) => string
}) {
  const payload = asAnswer(answer.payload)
  const outOfFrame = payload.shape === 'text' // kept simple; the question state carries the frame call
  return (
    <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-2.5">
      <div className="mb-1 flex items-center gap-2 text-xs text-[var(--color-muted)]">
        <span className="inline-flex items-center rounded-full bg-emerald-500/15 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-emerald-300 ring-1 ring-inset ring-emerald-500/30">
          Answer
        </span>
        <AuthorLabel
          author={answer.author}
          externalAuthor={answer.external_author}
          resolveExternal={resolveExternal}
        />
        {payload.shape && !outOfFrame && <span>· {payload.shape}</span>}
        <span className="ml-auto">{relTime(answer.created_at)}</span>
      </div>
      <AnswerValue answer={payload} body={answer.body} options={options} />
    </div>
  )
}

const BTN = 'rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40'
const BTN_GHOST =
  'rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] ring-1 ring-inset ring-[var(--color-border)] hover:bg-[var(--color-panel-2)] disabled:opacity-40'

// The interactive answer form for an OPEN question, keyed by kind. Submits { shape, value } matching
// the backend's per-kind contract: yes_no -> bool; multiple_choice / select_all -> choice (array of
// option ids); fill_in_the_blank -> text; rank_list -> ranked (ordered option ids). A free-text
// escape is always available for an out-of-frame answer. Schema-driven questions carry an inline
// response_schema that the backend validates; any rejection surfaces via the caller's error path.
function AnswerForm({
  q,
  busy,
  onSubmit,
}: {
  q: QuestionPayload
  busy: boolean
  onSubmit: (shape: string, value: unknown) => void
}) {
  const options = q.options ?? []
  const [choice, setChoice] = useState('')
  const [multi, setMulti] = useState<string[]>([])
  const [text, setText] = useState('')
  const [order, setOrder] = useState<string[]>(options.map((o) => o.id))
  const [freeText, setFreeText] = useState('')
  const [showFree, setShowFree] = useState(false)

  const toggleMulti = (id: string) =>
    setMulti((m) => (m.includes(id) ? m.filter((x) => x !== id) : [...m, id]))
  const move = (i: number, d: -1 | 1) =>
    setOrder((o) => {
      const j = i + d
      if (j < 0 || j >= o.length) return o
      const next = [...o]
      ;[next[i], next[j]] = [next[j], next[i]]
      return next
    })
  const labelOf = (id: string) => options.find((o) => o.id === id)?.label ?? id

  return (
    <div className="mt-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-2.5">
      {q.kind === 'yes_no' && (
        <div className="flex gap-2">
          <button disabled={busy} className={BTN} onClick={() => onSubmit('bool', true)}>
            Yes
          </button>
          <button disabled={busy} className={BTN_GHOST} onClick={() => onSubmit('bool', false)}>
            No
          </button>
        </div>
      )}

      {q.kind === 'multiple_choice' && (
        <div className="space-y-1.5">
          {options.map((o) => (
            <label key={o.id} className="flex items-center gap-2 text-sm">
              <input
                type="radio"
                name={`mc-answer`}
                checked={choice === o.id}
                onChange={() => setChoice(o.id)}
              />
              {o.label}
            </label>
          ))}
          <button
            disabled={busy || !choice}
            className={BTN}
            onClick={() => onSubmit('choice', [choice])}
          >
            Submit answer
          </button>
        </div>
      )}

      {q.kind === 'select_all' && (
        <div className="space-y-1.5">
          {options.map((o) => (
            <label key={o.id} className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={multi.includes(o.id)}
                onChange={() => toggleMulti(o.id)}
              />
              {o.label}
            </label>
          ))}
          <button
            disabled={busy || multi.length === 0}
            className={BTN}
            onClick={() => onSubmit('choice', multi)}
          >
            Submit answer
          </button>
        </div>
      )}

      {q.kind === 'fill_in_the_blank' && (
        <div className="flex items-end gap-2">
          <AutoGrowTextarea
            value={text}
            onChange={setText}
            onSubmit={() => text.trim() && onSubmit('text', text.trim())}
            placeholder="Your answer..."
            className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
          />
          <button
            disabled={busy || !text.trim()}
            className={BTN}
            onClick={() => onSubmit('text', text.trim())}
          >
            Submit
          </button>
        </div>
      )}

      {q.kind === 'rank_list' && (
        <div className="space-y-1.5">
          <ol className="space-y-1">
            {order.map((id, i) => (
              <li
                key={id}
                className="flex items-center gap-2 rounded border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm"
              >
                <span className="font-mono text-[var(--color-muted)]">{i + 1}.</span>
                <span className="min-w-0 flex-1 truncate">{labelOf(id)}</span>
                <button
                  disabled={busy || i === 0}
                  onClick={() => move(i, -1)}
                  className="px-1 text-[var(--color-muted)] hover:text-sky-300 disabled:opacity-30"
                  aria-label="Move up"
                >
                  ^
                </button>
                <button
                  disabled={busy || i === order.length - 1}
                  onClick={() => move(i, 1)}
                  className="px-1 text-[var(--color-muted)] hover:text-sky-300 disabled:opacity-30"
                  aria-label="Move down"
                >
                  v
                </button>
              </li>
            ))}
          </ol>
          <button disabled={busy} className={BTN} onClick={() => onSubmit('ranked', order)}>
            Submit ranking
          </button>
        </div>
      )}

      {/* Out-of-frame escape: answer in free text regardless of kind. */}
      {q.kind !== 'fill_in_the_blank' && (
        <div className="mt-2 border-t border-[var(--color-border)] pt-2">
          {showFree ? (
            <div className="flex items-end gap-2">
              <AutoGrowTextarea
                value={freeText}
                onChange={setFreeText}
                onSubmit={() => freeText.trim() && onSubmit('text', freeText.trim())}
                placeholder="Answer in your own words..."
                className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
              />
              <button
                disabled={busy || !freeText.trim()}
                className={BTN}
                onClick={() => onSubmit('text', freeText.trim())}
              >
                Send
              </button>
            </div>
          ) : (
            <button
              onClick={() => setShowFree(true)}
              className="text-xs text-sky-400 hover:text-sky-300"
            >
              Answer in your own words instead
            </button>
          )}
        </div>
      )}
    </div>
  )
}

// A question comment with its answers nested beneath it, and -- when the question is open and the
// caller passes action handlers -- an inline answer form plus decline / cancel controls (task_629
// slice 2). Omitting the handlers (or on a terminal state) renders it read-only.
export function QuestionComment({
  comment,
  answers,
  resolveExternal,
  actor,
  busy,
  onAnswer,
  onDecline,
  onCancel,
  onSupersede,
}: {
  comment: Comment
  answers: Comment[]
  resolveExternal: (id: string) => string
  actor?: string
  busy?: boolean
  onAnswer?: (shape: string, value: unknown) => void
  onDecline?: () => void
  onCancel?: () => void
  onSupersede?: () => void
}) {
  const q = asQuestion(comment.payload)
  const state = (comment.state ?? 'open') as string
  const blocking = q?.blocking === true
  const isOpen = state === 'open'
  const isAsker = actor != null && comment.author === actor
  return (
    <div>
      <div className="mb-1.5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
        <span className="inline-flex items-center rounded-full bg-violet-500/15 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-violet-300 ring-1 ring-inset ring-violet-500/30">
          Question
        </span>
        {q && <span>{KIND_LABEL[q.kind] ?? q.kind}</span>}
        <StateChip state={state} />
        {q?.routed_to && (
          <span>
            · asked of <span className="font-mono">{q.routed_to}</span>
          </span>
        )}
        <span>· {blocking ? 'blocking' : 'non-blocking'}</span>
        <AuthorLabel
          author={comment.author}
          externalAuthor={comment.external_author}
          resolveExternal={resolveExternal}
        />
        <span className="ml-auto">{relTime(comment.created_at)}</span>
      </div>

      {/* The prompt. */}
      <Markdown source={comment.body} className="text-sm" />

      {/* Options for choice-shaped kinds. */}
      {q?.options && q.options.length > 0 && (
        <ul className="mt-2 space-y-1">
          {q.options.map((o) => (
            <li
              key={o.id}
              className="rounded border border-[var(--color-border)] bg-[var(--color-panel)] px-2 py-1 text-sm"
            >
              {o.label}
            </li>
          ))}
        </ul>
      )}

      {/* Non-blocking questions proceed on a default (optionally after a wait); surface that. */}
      {!blocking && q?.default != null && (
        <p className="mt-1.5 text-xs text-[var(--color-muted)]">
          Proceeds on default{' '}
          <span className="font-mono">
            {typeof q.default === 'string' ? q.default : JSON.stringify(q.default)}
          </span>
          {q.wait_period_seconds ? ` after ${q.wait_period_seconds}s` : ''}.
        </p>
      )}

      {/* Answers (if any) nested beneath the question. */}
      {answers.length > 0 && (
        <div className="mt-2 space-y-2 border-l-2 border-emerald-500/30 pl-3">
          {answers.map((a) => (
            <AnswerCard
              key={a.id}
              answer={a}
              options={q?.options}
              resolveExternal={resolveExternal}
            />
          ))}
        </div>
      )}

      {/* Open question: inline answer form + decline / cancel controls (slice 2). */}
      {isOpen && onAnswer && q && <AnswerForm q={q} busy={!!busy} onSubmit={onAnswer} />}
      {isOpen && (onDecline || (isAsker && (onCancel || onSupersede))) && (
        <div className="mt-2 flex items-center gap-3 text-xs">
          {onDecline && (
            <button
              disabled={busy}
              onClick={onDecline}
              className="text-amber-400 hover:text-amber-300 disabled:opacity-40"
            >
              Decline
            </button>
          )}
          {isAsker && onSupersede && (
            <button
              disabled={busy}
              onClick={onSupersede}
              className="text-sky-400 hover:text-sky-300 disabled:opacity-40"
            >
              Supersede
            </button>
          )}
          {isAsker && onCancel && (
            <button
              disabled={busy}
              onClick={onCancel}
              className="text-rose-400 hover:text-rose-300 disabled:opacity-40"
            >
              Cancel question
            </button>
          )}
        </div>
      )}

      {/* A superseded question points at its replacement. */}
      {comment.superseded_by != null && (
        <p className="mt-1.5 text-xs text-[var(--color-muted)]">
          Superseded by a newer question.
        </p>
      )}
    </div>
  )
}
