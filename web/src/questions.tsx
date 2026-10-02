import { useState, type ReactNode } from 'react'
import {
  ipfsUrl,
  type AnswerPayload,
  type Comment,
  type QuestionKind,
  type QuestionOption,
  type QuestionPayload,
  type QuestionState,
} from './api'
import { AgeAnswer } from './age-answer'
import { ipfsCidFromSrc, Markdown } from './markdown'
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
  point_allocation: 'Point allocation',
  quiz: 'Knowledge check',
}

const STATE_CHIP: Record<QuestionState, string> = {
  open: 'bg-sky-100 text-sky-800 ring-sky-500/30 dark:bg-sky-500/15 dark:text-sky-300',
  answered: 'bg-emerald-100 text-emerald-800 ring-emerald-500/30 dark:bg-emerald-500/15 dark:text-emerald-300',
  answered_outside_frame: 'bg-amber-100 text-amber-800 ring-amber-500/30 dark:bg-amber-500/15 dark:text-amber-300',
  declined: 'bg-rose-100 text-rose-700 ring-rose-500/30 dark:bg-rose-500/15 dark:text-rose-300',
  cancelled: 'bg-zinc-100 text-zinc-600 ring-zinc-500/30 dark:bg-zinc-500/15 dark:text-zinc-400',
  superseded: 'bg-zinc-100 text-zinc-600 ring-zinc-500/30 dark:bg-zinc-500/15 dark:text-zinc-400',
}

function StateChip({ state }: { state: string }) {
  const chip =
    STATE_CHIP[state as QuestionState] ??
    'bg-zinc-100 text-zinc-600 ring-zinc-500/30 dark:bg-zinc-500/15 dark:text-zinc-400'
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

// Conversational one-at-a-time flow (doc_3371 A1 entry 12). Not a new answer element: a UI flow that
// groups several EXISTING atomic questions into a sequence shown one screen at a time (progressive
// disclosure), each still answered separately. The grouping signal rides the opaque ui descriptor --
// no schema/backend change: ui.props.group (shared key), ui.props.group_order (1-based position),
// and optional ui.props.group_size (total N, for "Question k of N").
function questionGroup(comment: Comment): { key: string; order: number; size?: number } | null {
  const p = asQuestion(comment.payload)?.ui?.props
  const key = p && typeof p.group === 'string' && p.group ? p.group : null
  if (!key) return null
  return {
    key,
    order: p && typeof p.group_order === 'number' ? p.group_order : 0,
    size: p && typeof p.group_size === 'number' ? p.group_size : undefined,
  }
}

// Partition a question list into ordered sequence groups (by shared group key) and the rest. Used by
// a question surface (e.g. the awaiting view) to render a grouped set as one ConversationalFlow and
// the ungrouped questions normally.
export function groupQuestions(questions: Comment[]): {
  groups: { key: string; items: Comment[] }[]
  ungrouped: Comment[]
} {
  const ungrouped: Comment[] = []
  const byKey = new Map<string, Comment[]>()
  for (const c of questions) {
    const g = questionGroup(c)
    if (!g) {
      ungrouped.push(c)
      continue
    }
    const arr = byKey.get(g.key) ?? []
    arr.push(c)
    byKey.set(g.key, arr)
  }
  const groups = [...byKey.entries()].map(([key, items]) => ({
    key,
    items: items.sort((a, b) => (questionGroup(a)?.order ?? 0) - (questionGroup(b)?.order ?? 0)),
  }))
  return { groups, ungrouped }
}

// Render a sequence group one question at a time: only the first still-open question is answerable
// (the caller passes the already-open set, so answering one drops it and the next becomes current on
// the next data refresh), with a progress header and a count of what remains. Progressive disclosure
// without bundling -- each question stays atomic and separately answered.
export function ConversationalFlow({
  group,
  renderQuestion,
}: {
  group: Comment[]
  renderQuestion: (q: Comment) => ReactNode
}) {
  if (group.length === 0) return null
  const current = group[0]
  const g = questionGroup(current)
  const order = g?.order || 1
  const size = g?.size
  const rest = group.length - 1
  return (
    <div className="rounded-lg border border-sky-500/20 bg-sky-500/5 p-3">
      <div className="mb-2 flex items-center gap-2 text-xs text-[var(--color-muted)]">
        <span className="inline-flex items-center rounded-full bg-sky-500/15 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-sky-300 ring-1 ring-inset ring-sky-500/30">
          Sequence
        </span>
        <span>{size != null ? `Question ${order} of ${size}` : `Question ${order}`} · one at a time</span>
        {size != null && (
          <span className="ml-auto h-1.5 w-24 overflow-hidden rounded-full bg-[var(--color-panel-2)]">
            <span
              className="block h-full bg-sky-500"
              style={{ width: `${Math.min(100, Math.round((order / size) * 100))}%` }}
            />
          </span>
        )}
      </div>
      {renderQuestion(current)}
      {rest > 0 && (
        <p className="mt-2 border-t border-[var(--color-border)] pt-2 text-xs text-[var(--color-muted)]">
          {rest} more in this sequence after you answer.
        </p>
      )}
    </div>
  )
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
        // Carry an optional per-option image through for the image-choice variant (entry 9); the
        // renderer confidentiality-gates it like a markdown image, so a non-IPFS src is dropped.
        const image = typeof rec.image === 'string' ? rec.image : undefined
        out.push({ id: rec.id, label: rec.label, ...(image ? { image } : {}) })
      }
    }
  }
  return out
}

function asCount(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isInteger(v) && v >= 0 ? v : undefined
}

// Any finite number (vs asCount's non-negative integer) -- for the numeric element's bounds/step.
function asNumber(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined
}

// Default confidence scale for the confidence-tag element when the question supplies none.
const DEFAULT_CONFIDENCE_LEVELS: QuestionOption[] = [
  { id: 'low', label: 'Low' },
  { id: 'medium', label: 'Medium' },
  { id: 'high', label: 'High' },
]

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
  // `display` (single-choice only): a one-tap presentation over the radio-list + submit default.
  // 'buttons' is the inline button row (task_1179); 'scale' is an ordered rating/Likert row with
  // optional end labels (doc_3371 A1 entry 1, task_1203). Both submit the chosen id on click.
  | {
      shape: 'choice'
      multi: boolean
      options: QuestionOption[]
      min?: number
      max?: number
      scalar?: boolean
      display?: 'buttons' | 'scale' | 'stars' | 'nps' | 'image'
      minLabel?: string
      maxLabel?: string
    }
  // `text` also backs the editable-value element (doc_3371 A1 entry 11, approve-with-edit): `initial`
  // prefills the field with a proposed value the operator edits before accepting.
  | { shape: 'text'; placeholder?: string; initial?: string }
  | { shape: 'ranked'; options: QuestionOption[]; maxRanked?: number }
  | { shape: 'list'; min?: number; max?: number; placeholder?: string; itemLabel?: string }
  // `number` (schema-driven, doc_3371 A1 entry 3): a single bounded number validated against the
  // question's inline response_schema. The bounds here mirror that schema for the input's own min/
  // max/step; the submitted value is a bare number (integer when `integer`).
  | {
      shape: 'number'
      min?: number
      max?: number
      integer?: boolean
      step?: number
      placeholder?: string
      unit?: string
      // 'slider' (doc_3371 A1 entry 4) renders the same bounded-number answer as a range input with
      // a live value readout rather than a typed field; 'input' (default) is the numeric text field.
      display?: 'input' | 'slider'
    }
  // `datetime` (schema-driven, doc_3371 A1 entry 7): a native date/datetime/time picker whose string
  // value the inline response_schema validates with a pattern regex. mode picks the control.
  | { shape: 'datetime'; mode: 'date' | 'datetime' | 'time'; min?: string; max?: string }
  // `confidence` (schema-driven, doc_3371 A1 entry 6): a composite {choice, confidence} object -- one
  // decision plus how sure the operator is -- validated by the inline object response_schema.
  | { shape: 'confidence'; options: QuestionOption[]; levels: QuestionOption[]; confidenceLabel?: string }
  // `allocation` (legacy point_allocation kind, doc_3371 A1 entry 8): spread a fixed `budget` of
  // integer points across the options -- a constant-sum answer. The submitted value is an object
  // {option_id: integer_points} whose values must total the budget exactly (enforced in-form before
  // submit and re-validated by the backend).
  | { shape: 'allocation'; options: QuestionOption[]; budget: number }
  // `age` (age-request element, task_713): encrypt the secret in-browser to one or more age
  // recipients; only the ciphertext is submitted. `target` is optional advisory destination metadata.
  | { shape: 'age'; recipients: string[]; target?: string }

// Does a response schema expect an array value (vs a scalar)? Used to decide a single-select's
// submitted value shape so it satisfies the question's inline response_schema.
function isArraySchema(s: unknown): boolean {
  return !!s && typeof s === 'object' && (s as Record<string, unknown>).type === 'array'
}

// The one-tap presentation a single-choice question asks for, or undefined for the default radio
// list. Same data model either way: the hint rides the opaque ui descriptor (no schema/kind
// change). 'buttons' = inline button row (task_1179); 'scale' = ordered rating/Likert row (doc_3371
// A1 entry 1); 'stars' = star-glyph rating row (A1 entry 2). Each element's aliases map here;
// honored via ui.element or a variant/display prop.
function choiceDisplay(q: QuestionPayload): 'buttons' | 'scale' | 'stars' | 'nps' | 'image' | undefined {
  const el = q.ui?.element
  const p = q.ui?.props
  const anyIs = (names: string[]) => (v: unknown) => typeof v === 'string' && names.includes(v)
  const matches = (names: string[]) => {
    const is = anyIs(names)
    return is(el) || (!!p && (is(p.variant) || is(p.display)))
  }
  if (matches(['buttons'])) return 'buttons'
  if (matches(['scale', 'rating', 'likert'])) return 'scale'
  if (matches(['stars', 'star', 'rating-stars'])) return 'stars'
  if (matches(['nps', 'net-promoter', 'net_promoter'])) return 'nps'
  if (matches(['image', 'image-choice', 'visual', 'visual-choice'])) return 'image'
  return undefined
}

// NPS is a 0-10 scale, so it shares the scale render but carries conventional default end labels
// when the question did not supply its own (doc_3371 A1 entry 5).
function endLabels(
  display: 'buttons' | 'scale' | 'stars' | 'nps' | 'image' | undefined,
  min: string | undefined,
  max: string | undefined,
): { minLabel?: string; maxLabel?: string } {
  if (display === 'nps')
    return { minLabel: min ?? 'Not at all likely', maxLabel: max ?? 'Extremely likely' }
  return { minLabel: min, maxLabel: max }
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
          display: choiceDisplay(q),
          ...endLabels(choiceDisplay(q), asText(p.min_label), asText(p.max_label)),
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
      case 'editable-value':
        // Approve-with-edit (entry 11, text/scalar form): prefill the proposed value for the operator
        // to adjust and accept in one step. Validated as a string by the inline response_schema.
        return { shape: 'text', placeholder: asText(p.placeholder), initial: asText(p.proposed) }
      case 'rank':
        return { shape: 'ranked', options: asOptions(p.options), maxRanked: asCount(p.max_ranked) }
      case 'string-list':
        return {
          shape: 'list',
          min: asCount(p.min_items),
          max: asCount(p.max_items),
          placeholder: asText(p.placeholder),
          itemLabel: asText(p.item_label),
        }
      case 'numeric':
        return {
          shape: 'number',
          min: asNumber(p.minimum),
          max: asNumber(p.maximum),
          integer: p.integer === true,
          step: asNumber(p.step),
          placeholder: asText(p.placeholder),
          unit: asText(p.unit),
          display: 'input',
        }
      case 'slider':
        // Same bounded-number answer as numeric (entry 3), rendered as a range input. A slider needs
        // both bounds to draw its track; fall back to the typed input if either is missing.
        return {
          shape: 'number',
          min: asNumber(p.minimum),
          max: asNumber(p.maximum),
          integer: p.integer === true,
          step: asNumber(p.step),
          unit: asText(p.unit),
          display: asNumber(p.minimum) != null && asNumber(p.maximum) != null ? 'slider' : 'input',
        }
      case 'datetime': {
        const mode = p.mode === 'datetime' || p.mode === 'time' ? p.mode : 'date'
        return { shape: 'datetime', mode, min: asText(p.min), max: asText(p.max) }
      }
      case 'confidence-tag': {
        const levels = asOptions(p.confidence_levels)
        return {
          shape: 'confidence',
          options: asOptions(p.options),
          levels: levels.length >= 2 ? levels : DEFAULT_CONFIDENCE_LEVELS,
          confidenceLabel: asText(p.confidence_label),
        }
      }
      case 'age-request': {
        // Recipients (one or more age pubkeys) ride props.recipient_pubkeys; target is optional
        // advisory metadata. With at least one recipient we can encrypt in-browser; otherwise fall
        // back to the free-text (paste-ciphertext) escape.
        const recipients = Array.isArray(p.recipient_pubkeys)
          ? p.recipient_pubkeys.filter((r): r is string => typeof r === 'string' && r.length > 0)
          : []
        return recipients.length > 0 ? { shape: 'age', recipients, target: asText(p.target) } : null
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
      return {
        shape: 'choice',
        multi: false,
        options: q.options ?? [],
        display: choiceDisplay(q),
        ...endLabels(choiceDisplay(q), asText(q.ui?.props?.min_label), asText(q.ui?.props?.max_label)),
      }
    case 'select_all':
      return { shape: 'choice', multi: true, options: q.options ?? [] }
    case 'quiz':
      // Knowledge-check quiz (doc_3371 entry 10): a choice answer scored server-side against a hidden
      // key. The answer is an array of 1+ chosen option ids (like select_all), so reuse multi-select;
      // the correct-answer key is redacted from the open question and revealed on the answer.
      return { shape: 'choice', multi: true, options: q.options ?? [] }
    case 'fill_in_the_blank':
      return { shape: 'text' }
    case 'rank_list':
      return { shape: 'ranked', options: q.options ?? [] }
    case 'point_allocation': {
      // Constant-sum allocation (entry 8): the budget rides the question's per-kind config. Without a
      // valid budget there is no usable form, so fall back to the free-text escape.
      const budget = asNumber(q.config?.budget)
      return budget != null && budget >= 1
        ? { shape: 'allocation', options: q.options ?? [], budget }
        : null
    }
    default:
      return null
  }
}

// The options a question presents (choice / ranked shapes), for the read-only list and for resolving
// an answer's option ids to their labels. Empty for bool / text / formless questions.
function specOptions(spec: FormSpec | null): QuestionOption[] | undefined {
  if (
    spec &&
    (spec.shape === 'choice' || spec.shape === 'ranked' || spec.shape === 'allocation') &&
    spec.options.length > 0
  ) {
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
  if (typeof v === 'number') return <span className="text-sm font-medium">{v}</span>
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
  // Composite choice-with-confidence answer {choice, confidence} (doc_3371 A1 entry 6): resolve the
  // choice id to its option label and show the confidence alongside, rather than raw JSON.
  if (
    v &&
    typeof v === 'object' &&
    !Array.isArray(v) &&
    'choice' in v &&
    'confidence' in v
  ) {
    const rec = v as { choice: unknown; confidence: unknown }
    return (
      <span className="text-sm">
        <span className="font-medium">{labelFor(rec.choice)}</span>
        <span className="text-[var(--color-muted)]">
          {' '}
          · confidence: {typeof rec.confidence === 'string' ? rec.confidence : JSON.stringify(rec.confidence)}
        </span>
      </span>
    )
  }
  // Allocation answer {option_id: points} (doc_3371 A1 entry 8): resolve each id to its option label
  // and list the points highest-first (mirrors the backend summary), rather than raw JSON.
  if (
    answer.shape === 'allocation' &&
    v &&
    typeof v === 'object' &&
    !Array.isArray(v) &&
    Object.values(v as Record<string, unknown>).every((n) => typeof n === 'number')
  ) {
    const entries = Object.entries(v as Record<string, number>).sort((a, b) => b[1] - a[1])
    return (
      <ul className="ml-5 list-disc text-sm marker:text-[var(--color-muted)]">
        {entries.map(([id, pts]) => (
          <li key={id}>
            {labelFor(id)} <span className="text-[var(--color-muted)]">— {pts} pts</span>
          </li>
        ))}
      </ul>
    )
  }
  return (
    <pre className="overflow-x-auto rounded bg-[var(--color-panel)] p-2 font-mono text-xs">
      {JSON.stringify(v, null, 2)}
    </pre>
  )
}

// Quiz score reveal (doc_3371 A1 entry 10): shown beneath a quiz answer's chosen value. The backend
// scores the choice server-side against the hidden key and rides the result on the answer comment, so
// the correct-answer key + explanation are only revealed here, after the answerer has committed. The
// correct/incorrect badge reuses the AA-safe light/dark palette; correct_answer ids resolve to labels.
function QuizScoreReveal({
  correct,
  correctAnswer,
  explanation,
  options,
}: {
  correct: boolean
  correctAnswer?: string[]
  explanation?: string
  options?: QuestionOption[]
}) {
  const labelFor = (id: string) => options?.find((o) => o.id === id)?.label ?? id
  return (
    <div className="mt-2 border-t border-[var(--color-border)] pt-2 text-sm">
      <span
        className={`inline-flex items-center rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide ring-1 ring-inset ${
          correct
            ? 'bg-emerald-100 text-emerald-800 ring-emerald-500/30 dark:bg-emerald-500/15 dark:text-emerald-300'
            : 'bg-rose-100 text-rose-700 ring-rose-500/30 dark:bg-rose-500/15 dark:text-rose-300'
        }`}
      >
        {correct ? 'Correct' : 'Incorrect'}
      </span>
      {correctAnswer && correctAnswer.length > 0 && (
        <span className="ml-2">
          <span className="text-[var(--color-muted)]">Correct answer: </span>
          <span className="font-medium">{correctAnswer.map(labelFor).join(', ')}</span>
        </span>
      )}
      {explanation && (
        <div className="mt-1.5 rounded-md bg-[var(--color-panel-2)] p-2">
          <Markdown source={explanation} className="text-sm" />
        </div>
      )}
    </div>
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
      {payload.correct !== undefined && (
        <QuizScoreReveal
          correct={payload.correct}
          correctAnswer={payload.correct_answer}
          explanation={payload.explanation}
          options={options}
        />
      )}
    </div>
  )
}

const BTN = 'rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40'
const BTN_GHOST =
  'rounded-md px-2.5 py-1 text-xs text-[var(--color-muted)] ring-1 ring-inset ring-[var(--color-border)] hover:bg-[var(--color-panel-2)] disabled:opacity-40'

// Star-rating render for a single-choice question (doc_3371 A1 entry 2, task_1206). The ordered
// options are drawn as star glyphs that fill (solid) up to the pointed/focused star; one tap submits
// that option's id -- same value as every other single-choice variant. Its own component because the
// hover/focus fill needs local state. Each star carries an aria-label so it is answerable without
// the visual fill. Large glyphs, so AA treats them as large text.
function StarRating({
  options,
  busy,
  scalar,
  onSubmit,
}: {
  options: QuestionOption[]
  busy: boolean
  scalar?: boolean
  onSubmit: (shape: string, value: unknown) => void
}) {
  const [hover, setHover] = useState(-1)
  // Theme-aware gold (set on each star below): amber-700 on a light panel (>=4.5:1) and the brighter
  // amber-400 on dark (>=13:1). amber-400 alone fails WCAG AA on white, so it is never unconditional.
  return (
    <div className="flex items-center gap-1" onMouseLeave={() => setHover(-1)}>
      {options.map((o, i) => (
        <button
          key={o.id}
          type="button"
          disabled={busy}
          title={o.label || `${i + 1}`}
          aria-label={o.label || `${i + 1} of ${options.length}`}
          onMouseEnter={() => setHover(i)}
          onFocus={() => setHover(i)}
          onClick={() => onSubmit('choice', scalar ? o.id : [o.id])}
          className="text-2xl leading-none text-amber-700 disabled:opacity-40 dark:text-amber-400"
        >
          {i <= hover ? '★' : '☆'}
        </button>
      ))}
    </div>
  )
}

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
  // Prefill with the proposed value for an editable-value question (entry 11); '' otherwise.
  const [text, setText] = useState(() => (spec?.shape === 'text' ? (spec.initial ?? '') : ''))
  const [num, setNum] = useState('')
  const [dt, setDt] = useState('')
  const [confChoice, setConfChoice] = useState('')
  const [confLevel, setConfLevel] = useState('')
  // Allocation: points per option id (missing = 0). Kept as integers via the setter below.
  const [alloc, setAlloc] = useState<Record<string, number>>({})
  const [order, setOrder] = useState<string[]>(options.map((o) => o.id))
  // string-list rows (start with one empty row the operator types into).
  const [items, setItems] = useState<string[]>([''])
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

  // string-list: the submittable value is the trimmed, non-empty rows; valid within [min, max].
  const listMin = spec?.shape === 'list' ? (spec.min ?? 1) : 1
  const listMax = spec?.shape === 'list' ? spec.max : undefined
  const cleanedItems = items.map((s) => s.trim()).filter((s) => s.length > 0)
  const listOk = cleanedItems.length >= listMin && (listMax == null || cleanedItems.length <= listMax)
  const setItem = (i: number, v: string) => setItems((xs) => xs.map((x, j) => (j === i ? v : x)))
  const addItem = () => setItems((xs) => [...xs, ''])
  const removeItem = (i: number) =>
    setItems((xs) => (xs.length <= 1 ? [''] : xs.filter((_, j) => j !== i)))

  // number: parse the typed value and validate it against the element's bounds before enabling
  // submit, so the client never posts a value the inline response_schema would reject.
  const numSpec = spec?.shape === 'number' ? spec : null
  const numParsed = num.trim() === '' ? null : Number(num)
  const numOk =
    numParsed != null &&
    Number.isFinite(numParsed) &&
    (!numSpec?.integer || Number.isInteger(numParsed)) &&
    (numSpec?.min == null || numParsed >= numSpec.min) &&
    (numSpec?.max == null || numParsed <= numSpec.max)
  const numHint = (() => {
    if (!numSpec) return null
    const lo = numSpec.min != null
    const hi = numSpec.max != null
    const range = lo && hi ? `${numSpec.min}–${numSpec.max}` : lo ? `≥ ${numSpec.min}` : hi ? `≤ ${numSpec.max}` : null
    const kind = numSpec.integer ? 'whole number' : 'number'
    return range ? `Enter a ${kind} ${range}.` : numSpec.integer ? 'Enter a whole number.' : null
  })()
  // A slider always has a value: start the thumb at the midpoint of the (required) bounds, rounded
  // for an integer slider, so the operator can submit immediately or drag to adjust.
  const numMid =
    numSpec && numSpec.min != null && numSpec.max != null
      ? (numSpec.min + numSpec.max) / 2
      : (numSpec?.min ?? 0)
  const sliderVal = num !== '' ? Number(num) : numSpec?.integer ? Math.round(numMid) : numMid

  // allocation (constant-sum, entry 8): points per option, totalling exactly the budget. Submit is
  // gated on total === budget; the value is {option_id: points} over every option (zeros included,
  // all valid ids), which the backend re-validates. The setter clamps to a non-negative integer.
  const allocSpec = spec?.shape === 'allocation' ? spec : null
  const allocPts = (id: string) => alloc[id] ?? 0
  const setAllocPts = (id: string, n: number) =>
    setAlloc((a) => ({ ...a, [id]: Math.max(0, Math.floor(Number.isFinite(n) ? n : 0)) }))
  const allocTotal = allocSpec ? allocSpec.options.reduce((s, o) => s + allocPts(o.id), 0) : 0
  const allocRemaining = allocSpec ? allocSpec.budget - allocTotal : 0
  const allocOk = !!allocSpec && allocRemaining === 0
  const allocValue = allocSpec
    ? Object.fromEntries(allocSpec.options.map((o) => [o.id, allocPts(o.id)]))
    : {}

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

      {spec?.shape === 'choice' && !spec.multi && spec.display === 'buttons' && (
        // Quick button-response variant (task_1179): one tap on an option submits it immediately,
        // no separate submit step. Same submitted value as the radio variant below.
        <div className="flex flex-wrap gap-2">
          {options.map((o) => (
            <button
              key={o.id}
              disabled={busy}
              className={BTN_GHOST}
              onClick={() => onSubmit('choice', spec.scalar ? o.id : [o.id])}
            >
              {o.label}
            </button>
          ))}
        </div>
      )}

      {spec?.shape === 'choice' && !spec.multi && spec.display === 'stars' && (
        <StarRating options={options} busy={busy} scalar={spec.scalar} onSubmit={onSubmit} />
      )}

      {spec?.shape === 'choice' && !spec.multi && spec.display === 'image' && (
        // Visual / image choice (doc_3371 A1 entry 9, task_1198): each option is a tappable image
        // tile (image + label); one tap submits the option id -- same value as the radio variant.
        // The image is an IPFS reference resolved + confidentiality-gated like a markdown image (a
        // non-IPFS / absolute src renders an inert placeholder, never fetched), so a tile still
        // answers via its label even when its image is blocked or absent.
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          {options.map((o) => {
            const cid = ipfsCidFromSrc(o.image)
            return (
              <button
                key={o.id}
                type="button"
                disabled={busy}
                aria-label={o.label}
                onClick={() => onSubmit('choice', spec.scalar ? o.id : [o.id])}
                className="flex flex-col items-center gap-1.5 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-2 text-sm hover:border-sky-500/50 disabled:opacity-40"
              >
                {cid ? (
                  <img src={ipfsUrl(cid)} alt={o.label} className="h-24 w-full rounded object-cover" />
                ) : (
                  <span className="flex h-24 w-full items-center justify-center rounded bg-[var(--color-panel)] text-xs text-[var(--color-muted)]">
                    [image]
                  </span>
                )}
                <span className="text-center">{o.label}</span>
              </button>
            )
          })}
        </div>
      )}

      {spec?.shape === 'choice' && !spec.multi && (spec.display === 'scale' || spec.display === 'nps') && (
        // Rating / Likert scale (doc_3371 A1 entry 1, task_1203): the ordered options as one row of
        // equal-width one-tap buttons, with optional end labels beneath. One tap submits the chosen
        // option id -- same value as the radio variant.
        <div className="space-y-1">
          <div className="flex gap-1">
            {options.map((o) => (
              <button
                key={o.id}
                disabled={busy}
                className={`${BTN_GHOST} flex-1 justify-center text-center`}
                onClick={() => onSubmit('choice', spec.scalar ? o.id : [o.id])}
              >
                {o.label}
              </button>
            ))}
          </div>
          {(spec.minLabel || spec.maxLabel) && (
            <div className="flex justify-between text-[11px] text-[var(--color-muted)]">
              <span>{spec.minLabel ?? ''}</span>
              <span>{spec.maxLabel ?? ''}</span>
            </div>
          )}
        </div>
      )}

      {spec?.shape === 'choice' && !spec.multi && !spec.display && (
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
        <div className="space-y-1.5">
          {spec.initial != null && (
            <p className="text-xs text-[var(--color-muted)]">Proposed value — edit if needed, then accept.</p>
          )}
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
              {spec.initial != null ? 'Accept' : 'Submit'}
            </button>
          </div>
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

      {spec?.shape === 'list' && (
        <div className="space-y-1.5">
          {(listMin > 1 || listMax != null) && (
            <p className="text-xs text-[var(--color-muted)]">
              {listMax != null
                ? `Add ${listMin === listMax ? `exactly ${listMin}` : `${listMin}–${listMax}`} ${spec.itemLabel ?? 'item'}${listMax === 1 ? '' : 's'}.`
                : `Add at least ${listMin} ${spec.itemLabel ?? 'item'}${listMin === 1 ? '' : 's'}.`}
            </p>
          )}
          {items.map((v, i) => (
            <div key={i} className="flex items-center gap-2">
              <input
                type="text"
                value={v}
                onChange={(e) => setItem(i, e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && (listMax == null || items.length < listMax)) {
                    e.preventDefault()
                    addItem()
                  }
                }}
                placeholder={spec.placeholder ?? `${spec.itemLabel ?? 'Item'} ${i + 1}`}
                className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
              />
              <button
                disabled={busy || (items.length <= 1 && !v)}
                onClick={() => removeItem(i)}
                className="px-1 text-[var(--color-muted)] hover:text-rose-300 disabled:opacity-30"
                aria-label="Remove row"
              >
                x
              </button>
            </div>
          ))}
          <div className="flex items-center gap-3">
            <button
              disabled={busy || (listMax != null && items.length >= listMax)}
              onClick={addItem}
              className={BTN_GHOST}
            >
              + Add {spec.itemLabel ?? 'item'}
            </button>
            <button
              disabled={busy || !listOk}
              className={BTN}
              onClick={() => onSubmit('list', cleanedItems)}
            >
              Submit list
            </button>
          </div>
        </div>
      )}

      {spec?.shape === 'number' && spec.display === 'slider' && (
        // Slider (doc_3371 A1 entry 4): a range input over the same bounded-number answer, with the
        // min/max endpoints and a live value readout. One submit posts the current value, which the
        // range input keeps within [min,max] at `step`, so it always satisfies the response_schema.
        <div className="space-y-2">
          {numHint && <p className="text-xs text-[var(--color-muted)]">{numHint}</p>}
          <div className="flex items-center gap-3">
            <span className="text-xs tabular-nums text-[var(--color-muted)]">{spec.min}</span>
            <input
              type="range"
              min={spec.min}
              max={spec.max}
              step={spec.step ?? (spec.integer ? 1 : 'any')}
              value={sliderVal}
              disabled={busy}
              onChange={(e) => setNum(e.target.value)}
              aria-label="Value"
              className="flex-1 accent-sky-700"
            />
            <span className="text-xs tabular-nums text-[var(--color-muted)]">{spec.max}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="rounded-md bg-[var(--color-panel-2)] px-2 py-0.5 text-sm font-medium tabular-nums">
              {sliderVal}
              {spec.unit ? ` ${spec.unit}` : ''}
            </span>
            <button disabled={busy} className={BTN} onClick={() => onSubmit('number', sliderVal)}>
              Submit
            </button>
          </div>
        </div>
      )}

      {spec?.shape === 'number' && spec.display !== 'slider' && (
        <div className="space-y-1.5">
          {numHint && <p className="text-xs text-[var(--color-muted)]">{numHint}</p>}
          <div className="flex items-center gap-2">
            <input
              type="number"
              value={num}
              min={spec.min}
              max={spec.max}
              step={spec.step ?? (spec.integer ? 1 : undefined)}
              inputMode={spec.integer ? 'numeric' : 'decimal'}
              onChange={(e) => setNum(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && numOk && !busy) {
                  e.preventDefault()
                  onSubmit('number', numParsed)
                }
              }}
              placeholder={spec.placeholder ?? 'Enter a number…'}
              className="w-40 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
            />
            {spec.unit && <span className="text-sm text-[var(--color-muted)]">{spec.unit}</span>}
            <button
              disabled={busy || !numOk}
              className={BTN}
              onClick={() => onSubmit('number', numParsed)}
            >
              Submit
            </button>
          </div>
        </div>
      )}

      {spec?.shape === 'datetime' && (
        // Native date/datetime/time picker (doc_3371 A1 entry 7): submits the control's string value
        // (YYYY-MM-DD / YYYY-MM-DDTHH:MM / HH:MM), which the inline response_schema validates by
        // pattern. The native control guarantees the shape, so just require a non-empty pick.
        <div className="flex items-center gap-2">
          <input
            type={spec.mode === 'datetime' ? 'datetime-local' : spec.mode}
            value={dt}
            min={spec.min}
            max={spec.max}
            disabled={busy}
            onChange={(e) => setDt(e.target.value)}
            className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-sm outline-none focus:border-sky-500/50"
          />
          <button
            disabled={busy || !dt}
            className={BTN}
            onClick={() => onSubmit('datetime', dt)}
          >
            Submit
          </button>
        </div>
      )}

      {spec?.shape === 'confidence' && (
        // Choice-with-confidence (doc_3371 A1 entry 6): pick one option AND one confidence level, then
        // submit the composite {choice, confidence} object the inline response_schema validates. One
        // decision -- confidence is an attribute of the same answer, not a second question.
        <div className="space-y-2.5">
          <div className="space-y-1.5">
            {spec.options.map((o) => (
              <label key={o.id} className="flex items-center gap-2 text-sm">
                <input
                  type="radio"
                  name="conf-choice"
                  checked={confChoice === o.id}
                  onChange={() => setConfChoice(o.id)}
                />
                {o.label}
              </label>
            ))}
          </div>
          <div>
            <p className="mb-1 text-xs text-[var(--color-muted)]">
              {spec.confidenceLabel ?? 'How confident are you?'}
            </p>
            <div className="flex flex-wrap gap-2">
              {spec.levels.map((l) => (
                <button
                  key={l.id}
                  disabled={busy}
                  onClick={() => setConfLevel(l.id)}
                  className={
                    confLevel === l.id
                      ? 'rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40'
                      : BTN_GHOST
                  }
                >
                  {l.label}
                </button>
              ))}
            </div>
          </div>
          <button
            disabled={busy || !confChoice || !confLevel}
            className={BTN}
            onClick={() => onSubmit('confidence', { choice: confChoice, confidence: confLevel })}
          >
            Submit answer
          </button>
        </div>
      )}

      {spec?.shape === 'allocation' && (
        // Constant-sum point allocation (doc_3371 A1 entry 8): spread exactly `budget` integer points
        // across the options. A live total/remaining readout guides the operator to the budget, and
        // submit stays disabled until it lands on it. The submitted {option_id: points} is re-checked
        // by the backend.
        <div className="space-y-2.5">
          <p
            className={`text-xs font-medium ${
              allocRemaining === 0
                ? 'text-emerald-700 dark:text-emerald-400'
                : allocRemaining < 0
                  ? 'text-rose-600 dark:text-rose-400'
                  : 'text-[var(--color-muted)]'
            }`}
            aria-live="polite"
          >
            {allocRemaining === 0
              ? `All ${allocSpec?.budget} points allocated.`
              : allocRemaining > 0
                ? `${allocRemaining} of ${allocSpec?.budget} points left to allocate.`
                : `${-allocRemaining} over budget (budget is ${allocSpec?.budget}).`}
          </p>
          <div className="space-y-1.5">
            {allocSpec?.options.map((o) => (
              <div key={o.id} className="flex items-center gap-2 text-sm">
                <span className="min-w-0 flex-1 break-words">{o.label}</span>
                <button
                  type="button"
                  disabled={busy || allocPts(o.id) <= 0}
                  aria-label={`Remove a point from ${o.label}`}
                  onClick={() => setAllocPts(o.id, allocPts(o.id) - 1)}
                  className="rounded-md px-2 py-0.5 text-sm leading-none ring-1 ring-inset ring-[var(--color-border)] hover:bg-[var(--color-panel-2)] disabled:opacity-40"
                >
                  −
                </button>
                <input
                  type="number"
                  min={0}
                  step={1}
                  inputMode="numeric"
                  disabled={busy}
                  aria-label={`Points for ${o.label}`}
                  value={allocPts(o.id)}
                  onChange={(e) => setAllocPts(o.id, Math.trunc(Number(e.target.value)))}
                  className="w-16 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-right text-sm tabular-nums outline-none focus:border-sky-500/50"
                />
                <button
                  type="button"
                  disabled={busy}
                  aria-label={`Add a point to ${o.label}`}
                  onClick={() => setAllocPts(o.id, allocPts(o.id) + 1)}
                  className="rounded-md px-2 py-0.5 text-sm leading-none ring-1 ring-inset ring-[var(--color-border)] hover:bg-[var(--color-panel-2)] disabled:opacity-40"
                >
                  +
                </button>
              </div>
            ))}
          </div>
          <button
            disabled={busy || !allocOk}
            className={BTN}
            onClick={() => onSubmit('allocation', allocValue)}
          >
            Submit allocation
          </button>
        </div>
      )}

      {spec?.shape === 'age' && (
        <AgeAnswer
          recipients={spec.recipients}
          target={spec.target}
          busy={busy}
          onSubmit={onSubmit}
        />
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
  const ageProps =
    elementName === 'age-request'
      ? {
          recipients: Array.isArray(q?.ui?.props?.recipient_pubkeys)
            ? (q!.ui!.props!.recipient_pubkeys as unknown[]).filter(
                (r): r is string => typeof r === 'string' && r.length > 0,
              )
            : [],
          target: asText(q?.ui?.props?.target),
        }
      : null
  // When the interactive answer form renders (an open, answerable choice / ranked question), it
  // already draws every option as its own control (radio / buttons / scale / stars / checkboxes /
  // rank rows). Rendering the read-only options list as well would show the options TWICE -- the
  // doubled-question regression (task_1225). So the standalone list is for the READ-ONLY case only
  // (viewing someone else's question, a terminal/answered state, or any build with no inline form);
  // suppress it whenever the form below will render the same options.
  const formShowsOptions =
    isOpen && !!onAnswer && !!q && (spec?.shape === 'choice' || spec?.shape === 'ranked')
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

      {/* An encrypted-secret request names the age recipient(s) the answer is encrypted to, plus any
          advisory destination (target). */}
      {ageProps && ageProps.recipients.length > 0 && (
        <p className="mt-1.5 text-xs text-[var(--color-muted)]">
          Encrypted client-side to {ageProps.recipients.length} recipient
          {ageProps.recipients.length === 1 ? '' : 's'} (
          <span className="font-mono break-all">{ageProps.recipients.join(', ')}</span>); no plaintext
          is stored on the board.
          {ageProps.target && (
            <>
              {' '}
              Destination: <span className="font-mono break-all">{ageProps.target}</span>.
            </>
          )}
        </p>
      )}

      {/* Options for choice / ranked shapes (from q.options or, for a CID-keyed question, ui.props).
          Read-only only -- hidden when the answer form below renders the same options (task_1225). */}
      {options && !formShowsOptions && (
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
              className="text-amber-800 hover:text-amber-700 disabled:opacity-40 dark:text-amber-400 dark:hover:text-amber-300"
            >
              Decline
            </button>
          )}
          {isAsker && onSupersede && (
            <button
              disabled={busy}
              onClick={onSupersede}
              className="text-sky-700 hover:text-sky-600 disabled:opacity-40 dark:text-sky-400 dark:hover:text-sky-300"
            >
              Supersede
            </button>
          )}
          {isAsker && onCancel && (
            <button
              disabled={busy}
              onClick={onCancel}
              className="text-rose-700 hover:text-rose-600 disabled:opacity-40 dark:text-rose-400 dark:hover:text-rose-300"
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
