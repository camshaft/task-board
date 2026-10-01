import { useState } from 'react'
import {
  type AnswerPayload,
  type Comment,
  type QuestionKind,
  type QuestionOption,
  type QuestionPayload,
  type QuestionState,
} from './api'
import { AgeAnswer } from './age-answer'
import { Markdown } from './markdown'
import { AuthorLabel, AutoGrowTextarea, relTime } from './ui'
import { elementMeta, elementNameForCid } from './ui-registry'

// Operator-questions UI (task_629, consumer of the task_628 backend). Renders a question comment
// (prompt, kind/element, options, routing, lifecycle state) and its answers, plus an interactive
// answer form and the decline/cancel/supersede controls.
//
// A question is described one of two ways and the UI handles both uniformly via a normalized
// FormSpec: a legacy `kind` (one of the five baseline kinds, options in q.options), or -- the
// doc_33 v16 model -- a CID-keyed element whose canonical type id is ui.element_schema_cid, with
// its data living in ui.props. formSpecFor() resolves either into the same shape so one renderer
// serves both; an unknown CID (or an element with no inline form, e.g. age-request) falls back to
// the free-text out-of-frame escape.

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
// write). A legacy question carries a `kind`; a CID-keyed question (doc_33 v16) carries no kind and
// instead a `ui` descriptor + inline `response_schema`, so accept any of those markers.
function asQuestion(payload: unknown): QuestionPayload | null {
  if (
    payload &&
    typeof payload === 'object' &&
    ('kind' in payload || 'ui' in payload || 'response_schema' in payload)
  ) {
    return payload as QuestionPayload
  }
  return null
}

function asAnswer(payload: unknown): AnswerPayload {
  if (payload && typeof payload === 'object') return payload as AnswerPayload
  return {}
}

// Coerce an unknown props value into a well-formed option list (id + label strings), dropping
// anything malformed. CID-keyed questions carry options in ui.props.options rather than q.options.
function asOptions(v: unknown): QuestionOption[] {
  if (!Array.isArray(v)) return []
  const out: QuestionOption[] = []
  for (const o of v) {
    if (o && typeof o === 'object') {
      const rec = o as Record<string, unknown>
      if (typeof rec.id === 'string' && typeof rec.label === 'string') {
        out.push({ id: rec.id, label: rec.label })
      }
    }
  }
  return out
}

function asCount(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isInteger(v) && v >= 0 ? v : undefined
}

function asText(v: unknown): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined
}

// A normalized description of the interactive form a question needs, resolved from either a CID-keyed
// element (ui.element_schema_cid + ui.props) or a legacy kind (q.kind + q.options). `null` means no
// built-in form (an unknown CID or an element like age-request) -- the caller offers the free-text
// out-of-frame escape instead.
type FormSpec =
  // `scalar` (single-choice only): submit the chosen id as a bare string rather than a 1-element
  // array -- a CID-keyed single-select's response_schema is {type:string, enum:[ids]}, so a string
  // is required; the legacy multiple_choice kind validates by kind and expects an array.
  | { shape: 'bool'; yesLabel: string; noLabel: string }
  | { shape: 'choice'; multi: boolean; options: QuestionOption[]; min?: number; max?: number; scalar?: boolean }
  | { shape: 'text'; placeholder?: string }
  | { shape: 'ranked'; options: QuestionOption[]; maxRanked?: number }
  | { shape: 'age'; recipient: string }

// Does a response schema expect an array value (vs a scalar)? Used to decide a single-select's
// submitted value shape so it satisfies the question's inline response_schema.
function isArraySchema(s: unknown): boolean {
  return !!s && typeof s === 'object' && (s as Record<string, unknown>).type === 'array'
}

function formSpecFor(q: QuestionPayload): FormSpec | null {
  const name = elementNameForCid(q.ui?.element_schema_cid)
  if (name) {
    const p = q.ui?.props ?? {}
    switch (name) {
      case 'yes-no':
        return { shape: 'bool', yesLabel: asText(p.yes_label) ?? 'Yes', noLabel: asText(p.no_label) ?? 'No' }
      case 'single-select':
        // A CID-keyed single-select's response_schema is {type:string, enum}, so submit a scalar id
        // (unless the author declared an array schema).
        return {
          shape: 'choice',
          multi: false,
          options: asOptions(p.options),
          scalar: !isArraySchema(q.response_schema),
        }
      case 'multi-select':
        return {
          shape: 'choice',
          multi: true,
          options: asOptions(p.options),
          min: asCount(p.min_selections),
          max: asCount(p.max_selections),
        }
      case 'text':
        return { shape: 'text', placeholder: asText(p.placeholder) }
      case 'rank':
        return { shape: 'ranked', options: asOptions(p.options), maxRanked: asCount(p.max_ranked) }
      case 'age-request': {
        const recipient = asText(p.recipient)
        // With a recipient we can encrypt in-browser; without one, fall back to the free-text escape.
        return recipient ? { shape: 'age', recipient } : null
      }
      default:
        // Any future element with no inline form: handled read-only + free text.
        return null
    }
  }
  switch (q.kind) {
    case 'yes_no':
      return { shape: 'bool', yesLabel: 'Yes', noLabel: 'No' }
    case 'multiple_choice':
      return { shape: 'choice', multi: false, options: q.options ?? [] }
    case 'select_all':
      return { shape: 'choice', multi: true, options: q.options ?? [] }
    case 'fill_in_the_blank':
      return { shape: 'text' }
    case 'rank_list':
      return { shape: 'ranked', options: q.options ?? [] }
    default:
      return null
  }
}

// The options a question presents (choice / ranked shapes), for the read-only list and for resolving
// an answer's option ids to their labels. Empty for bool / text / formless questions.
function specOptions(spec: FormSpec | null): QuestionOption[] | undefined {
  if (spec && (spec.shape === 'choice' || spec.shape === 'ranked') && spec.options.length > 0) {
    return spec.options
  }
  return undefined
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

// The interactive answer form for an OPEN question, driven by its normalized FormSpec. Submits
// { shape, value } matching the backend contract: bool; choice (array of option ids, for single and
// multi select); text; ranked (ordered option ids). The element's props refine it -- custom yes/no
// labels, min/max selections, a text placeholder, a max_ranked cap. A free-text escape is always
// available for an out-of-frame answer (and is the only control when the spec is null, e.g. an
// age-request or an element this build does not recognize).
function AnswerForm({
  spec,
  busy,
  onSubmit,
}: {
  spec: FormSpec | null
  busy: boolean
  onSubmit: (shape: string, value: unknown) => void
}) {
  const options = spec && (spec.shape === 'choice' || spec.shape === 'ranked') ? spec.options : []
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

  const multiMin = spec?.shape === 'choice' && spec.multi ? (spec.min ?? 1) : 1
  const multiMax = spec?.shape === 'choice' && spec.multi ? spec.max : undefined
  const multiOk = multi.length >= multiMin && (multiMax == null || multi.length <= multiMax)
  const boundsHint =
    multiMax != null
      ? `Choose ${multiMin === multiMax ? `exactly ${multiMin}` : `${multiMin}–${multiMax}`}.`
      : multiMin > 1
        ? `Choose at least ${multiMin}.`
        : null

  const maxRanked = spec?.shape === 'ranked' ? spec.maxRanked : undefined
  const rankedSubmit = maxRanked != null ? order.slice(0, maxRanked) : order

  return (
    <div className="mt-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-2.5">
      {spec?.shape === 'bool' && (
        <div className="flex gap-2">
          <button disabled={busy} className={BTN} onClick={() => onSubmit('bool', true)}>
            {spec.yesLabel}
          </button>
          <button disabled={busy} className={BTN_GHOST} onClick={() => onSubmit('bool', false)}>
            {spec.noLabel}
          </button>
        </div>
      )}

      {spec?.shape === 'choice' && !spec.multi && (
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
            onClick={() => onSubmit('choice', spec.scalar ? choice : [choice])}
          >
            Submit answer
          </button>
        </div>
      )}

      {spec?.shape === 'choice' && spec.multi && (
        <div className="space-y-1.5">
          {boundsHint && <p className="text-xs text-[var(--color-muted)]">{boundsHint}</p>}
          {options.map((o) => {
            const checked = multi.includes(o.id)
            const atMax = multiMax != null && multi.length >= multiMax
            return (
              <label key={o.id} className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={checked}
                  disabled={!checked && atMax}
                  onChange={() => toggleMulti(o.id)}
                />
                {o.label}
              </label>
            )
          })}
          <button
            disabled={busy || !multiOk}
            className={BTN}
            onClick={() => onSubmit('choice', multi)}
          >
            Submit answer
          </button>
        </div>
      )}

      {spec?.shape === 'text' && (
        <div className="flex items-end gap-2">
          <AutoGrowTextarea
            value={text}
            onChange={setText}
            onSubmit={() => text.trim() && onSubmit('text', text.trim())}
            placeholder={spec.placeholder ?? 'Your answer...'}
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

      {spec?.shape === 'ranked' && (
        <div className="space-y-1.5">
          {maxRanked != null && maxRanked < order.length && (
            <p className="text-xs text-[var(--color-muted)]">
              Order your top {maxRanked}; only the first {maxRanked} are submitted.
            </p>
          )}
          <ol className="space-y-1">
            {order.map((id, i) => (
              <li
                key={id}
                className={`flex items-center gap-2 rounded border px-2 py-1 text-sm ${
                  maxRanked != null && i >= maxRanked
                    ? 'border-[var(--color-border)] bg-[var(--color-panel)] opacity-50'
                    : 'border-[var(--color-border)] bg-[var(--color-panel-2)]'
                }`}
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
          <button disabled={busy} className={BTN} onClick={() => onSubmit('ranked', rankedSubmit)}>
            Submit ranking
          </button>
        </div>
      )}

      {spec?.shape === 'age' && (
        <AgeAnswer recipient={spec.recipient} busy={busy} onSubmit={onSubmit} />
      )}

      {/* Out-of-frame escape: answer in free text when there is no inline text field (so a bool /
          choice / ranked / age / formless question can still be answered in words -- for an
          age-request that means pasting ciphertext encrypted out of band). */}
      {spec?.shape !== 'text' && (
        <div className={spec ? 'mt-2 border-t border-[var(--color-border)] pt-2' : ''}>
          {showFree ? (
            <div className="flex items-end gap-2">
              <AutoGrowTextarea
                value={freeText}
                onChange={setFreeText}
                onSubmit={() => freeText.trim() && onSubmit('text', freeText.trim())}
                placeholder={
                  spec?.shape === 'age' ? 'Paste pre-encrypted ciphertext...' : 'Answer in your own words...'
                }
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
              {spec?.shape === 'age'
                ? 'Paste pre-encrypted ciphertext instead'
                : spec
                  ? 'Answer in your own words instead'
                  : 'Answer in your own words'}
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
  const spec = q ? formSpecFor(q) : null
  const options = specOptions(spec)
  const elementName = elementNameForCid(q?.ui?.element_schema_cid)
  // Prefer the CID-keyed element's title; fall back to the legacy kind label.
  const kindLabel = q
    ? (elementMeta(elementName)?.title ?? KIND_LABEL[q.kind] ?? elementName ?? q.kind)
    : null
  const ageRecipient =
    elementName === 'age-request' ? asText(q?.ui?.props?.recipient) : undefined
  return (
    <div>
      <div className="mb-1.5 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
        <span className="inline-flex items-center rounded-full bg-violet-500/15 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-violet-300 ring-1 ring-inset ring-violet-500/30">
          Question
        </span>
        {kindLabel && <span>{kindLabel}</span>}
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

      {/* An encrypted-secret request names the age recipient the answer is encrypted to. */}
      {ageRecipient && (
        <p className="mt-1.5 text-xs text-[var(--color-muted)]">
          Encrypted to <span className="font-mono break-all">{ageRecipient}</span> client-side; no
          plaintext is stored on the board.
        </p>
      )}

      {/* Options for choice / ranked shapes (from q.options or, for a CID-keyed question, ui.props). */}
      {options && (
        <ul className="mt-2 space-y-1">
          {options.map((o) => (
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
              options={options}
              resolveExternal={resolveExternal}
            />
          ))}
        </div>
      )}

      {/* Open question: inline answer form + decline / cancel controls (slice 2). */}
      {isOpen && onAnswer && q && <AnswerForm spec={spec} busy={!!busy} onSubmit={onAnswer} />}
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
