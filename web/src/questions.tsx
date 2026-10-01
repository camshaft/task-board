import {
  type AnswerPayload,
  type Comment,
  type QuestionKind,
  type QuestionOption,
  type QuestionPayload,
  type QuestionState,
} from './api'
import { Markdown } from './markdown'
import { AuthorLabel, relTime } from './ui'

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

// A question comment with its answers nested beneath it. Read-only for slice 1.
export function QuestionComment({
  comment,
  answers,
  resolveExternal,
}: {
  comment: Comment
  answers: Comment[]
  resolveExternal: (id: string) => string
}) {
  const q = asQuestion(comment.payload)
  const state = (comment.state ?? 'open') as string
  const blocking = q?.blocking === true
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

      {/* A superseded question points at its replacement. */}
      {comment.superseded_by != null && (
        <p className="mt-1.5 text-xs text-[var(--color-muted)]">
          Superseded by a newer question.
        </p>
      )}
    </div>
  )
}
