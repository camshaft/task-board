import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type ReviewStatus } from './api'
import { useBoardContext } from './Layout'
import { Markdown } from './markdown'
import { ReviewStatusChip, REVIEW_STATUS_FLOW, reviewSourceLink } from './Reviews'
import { appendReviewLog, setReviewStatus, setReviewVetted, useReview, useTask } from './resources'
import { AutoGrowTextarea, Identity, relTime, StatusChip } from './ui'

// Sensible next A2 transitions per current status (the backend accepts any valid status; this is
// UX guidance). changes_requested / approved are person-owned decisions (design D17) — the UI
// confirms them so it never implies an agent glibly self-approving; real authz belongs on the
// endpoint (flagged to v-task-board).
const NEXT_STATUS: Record<string, ReviewStatus[]> = {
  open: ['in_review', 'closed'],
  in_review: ['changes_requested', 'approved', 'closed'],
  changes_requested: ['in_review', 'closed'],
  approved: ['in_review', 'closed'],
  closed: ['in_review'],
}
const PERSON_OWNED: ReviewStatus[] = ['changes_requested', 'approved']

// One review (/reviews/:reviewId): the lifecycle state, the source artifact (linked per source),
// the vetted gate, the append-only log timeline, and the child/proposal tasks its findings track.
// Interactive: status transitions, the vetted-gate toggle, and an inline comment/finding composer.
// The improvement TREND (findings-per-review) is its own surface (needs the trend query, task 376).
export default function ReviewView() {
  const { reviewId } = useParams()
  const { actor } = useBoardContext()
  const id = Number(reviewId)
  const { data: review, error, loading } = useReview(id)

  const log = review?.log ?? []
  // Child/proposal tasks tracked by this review's findings (distinct task ids across the log).
  const linkedTaskIds = [...new Set(log.map((e) => e.task_id).filter((t): t is number => t != null))]

  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)
  // Inline composer: append a comment or a finding (with an optional linked task).
  const [entryType, setEntryType] = useState<'comment' | 'finding'>('comment')
  const [entryBody, setEntryBody] = useState('')
  const [findingTask, setFindingTask] = useState('')

  async function transition(to: ReviewStatus) {
    if (busy) return
    if (PERSON_OWNED.includes(to)) {
      if (
        !window.confirm(
          `Set this review to "${to.replace(/_/g, ' ')}"? This is a reviewer decision — make it as yourself (${actor}), not on an agent's behalf.`,
        )
      )
        return
    }
    const note = window.prompt('Add a note to this transition? (optional)') ?? undefined
    setBusy(true)
    setActionError(null)
    try {
      await setReviewStatus(id, { status: to, principal: actor, note: note || undefined })
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  // The adversarial-review gate (task 428). Like the person-owned transitions, this is a reviewer
  // decision — confirm as yourself so the affordance never implies an agent self-vetting (D17).
  async function toggleVetted(next: boolean) {
    if (busy || !review) return
    if (
      !window.confirm(
        `${next ? 'Mark this review vetted' : 'Clear the vetted gate on this review'}? This is a reviewer decision — make it as yourself (${actor}), not on an agent's behalf.`,
      )
    )
      return
    const note = window.prompt('Add a note to this decision? (optional)') ?? undefined
    setBusy(true)
    setActionError(null)
    try {
      await setReviewVetted(id, { vetted: next, principal: actor, note: note || undefined })
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function submitEntry() {
    const body = entryBody.trim()
    if (!body || busy) return
    const taskId = entryType === 'finding' && findingTask.trim() ? Number(findingTask.trim()) : undefined
    if (taskId !== undefined && !Number.isFinite(taskId)) {
      setActionError('Linked task must be a numeric task id.')
      return
    }
    setBusy(true)
    setActionError(null)
    try {
      await appendReviewLog(id, { entry_type: entryType, body, principal: actor, task_id: taskId })
      setEntryBody('')
      setFindingTask('')
    } catch (e) {
      setActionError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to="/reviews" className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Reviews
        </Link>
        <h1 className="truncate text-sm font-semibold">
          {review?.title || `Review #${id}`}
        </h1>
        {review && <ReviewStatusChip status={review.status} />}
        {review?.vetted && (
          <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-emerald-300">
            vetted
          </span>
        )}
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error.message}
        </div>
      )}
      {loading && !review && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      {review && (
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          {/* Lifecycle stepper: the A2 states in order, current highlighted. changes_requested
              cycles back to in_review; closed is terminal. */}
          <div className="mb-5 flex flex-wrap items-center gap-1.5">
            {REVIEW_STATUS_FLOW.map((s, i) => (
              <span key={s} className="flex items-center gap-1.5">
                {i > 0 && <span className="text-[var(--color-muted)]">→</span>}
                <span
                  className={`rounded-full px-2 py-0.5 text-xs ${
                    s === review.status
                      ? 'bg-sky-500/20 font-semibold text-sky-200 ring-1 ring-inset ring-sky-500/40'
                      : 'text-[var(--color-muted)]'
                  }`}
                >
                  {s.replace(/_/g, ' ')}
                </span>
              </span>
            ))}
          </div>

          {/* Transition controls. Person-owned decisions (changes_requested / approved) confirm
              first, as yourself — the affordance shouldn't imply an agent self-approving (D17). */}
          {(NEXT_STATUS[review.status] ?? []).length > 0 && (
            <div className="mb-5 flex flex-wrap items-center gap-2">
              <span className="text-xs text-[var(--color-muted)]">Transition to</span>
              {(NEXT_STATUS[review.status] ?? []).map((to) => (
                <button
                  key={to}
                  onClick={() => transition(to)}
                  disabled={busy}
                  className={`rounded-md px-2.5 py-1 text-xs ring-1 ring-inset disabled:opacity-40 ${
                    to === 'approved'
                      ? 'text-emerald-300 ring-emerald-500/40 hover:bg-emerald-500/10'
                      : to === 'changes_requested'
                        ? 'text-amber-300 ring-amber-500/40 hover:bg-amber-500/10'
                        : 'text-sky-400 ring-sky-500/40 hover:bg-sky-500/10'
                  }`}
                >
                  {to.replace(/_/g, ' ')}
                </button>
              ))}
            </div>
          )}

          {/* Vetted gate: the adversarial-review sign-off. A reviewer decision like the person-owned
              transitions, so it confirms as yourself (D17). Toggles the current state. */}
          <div className="mb-5 flex flex-wrap items-center gap-2">
            <span className="text-xs text-[var(--color-muted)]">Vetted gate</span>
            <button
              onClick={() => toggleVetted(!review.vetted)}
              disabled={busy}
              className={`rounded-md px-2.5 py-1 text-xs ring-1 ring-inset disabled:opacity-40 ${
                review.vetted
                  ? 'text-zinc-300 ring-zinc-500/40 hover:bg-zinc-500/10'
                  : 'text-emerald-300 ring-emerald-500/40 hover:bg-emerald-500/10'
              }`}
            >
              {review.vetted ? 'Clear vetted' : 'Mark vetted'}
            </button>
            <span className="text-[11px] text-[var(--color-muted)]">
              {review.vetted ? 'Passed adversarial review.' : 'Not yet signed off.'}
            </span>
          </div>

          {actionError && (
            <div className="mb-4 rounded-md bg-rose-500/15 px-3 py-2 text-sm text-rose-300">
              {actionError}
            </div>
          )}

          <dl className="mb-5 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-sm">
            <dt className="text-[var(--color-muted)]">Kind</dt>
            <dd className="font-mono text-xs">{review.kind}</dd>
            <dt className="text-[var(--color-muted)]">Source</dt>
            <dd className="min-w-0">
              {review.source ? (
                <span className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-xs text-[var(--color-muted)]">
                    {review.source}
                  </span>
                  {reviewSourceLink(review.source, review.target_ref)}
                </span>
              ) : (
                <span className="text-[var(--color-muted)]">—</span>
              )}
            </dd>
            {review.created_by && (
              <>
                <dt className="text-[var(--color-muted)]">Created by</dt>
                <dd className="font-mono text-xs">{review.created_by}</dd>
              </>
            )}
            {review.assignee && (
              <>
                <dt className="text-[var(--color-muted)]">Reviewer</dt>
                <dd className="font-mono text-xs">
                  <Identity id={review.assignee} />
                </dd>
              </>
            )}
            <dt className="text-[var(--color-muted)]">Updated</dt>
            <dd>{relTime(review.updated_at)}</dd>
          </dl>

          {/* Child / proposal tasks the findings track. */}
          {linkedTaskIds.length > 0 && (
            <div className="mb-5">
              <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                Linked tasks ({linkedTaskIds.length})
              </h2>
              <ul className="space-y-1.5">
                {linkedTaskIds.map((tid) => (
                  <li key={tid}>
                    <LinkedTask taskId={tid} />
                  </li>
                ))}
              </ul>
            </div>
          )}

          {/* The append-only log timeline (oldest first). Findings link the task tracking the fix. */}
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Log ({log.length})
          </h2>
          <ul className="space-y-2">
            {log.map((e) => (
              <li
                key={e.id}
                className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3"
              >
                <div className="mb-1 flex flex-wrap items-center gap-2 text-xs text-[var(--color-muted)]">
                  <EntryTypeBadge type={e.entry_type} />
                  {e.author && <span className="font-mono">{e.author}</span>}
                  <span>· {relTime(e.created_at)}</span>
                  {e.task_id != null && (
                    <Link to={`/tasks/${e.task_id}`} className="text-sky-400 hover:text-sky-300">
                      task #{e.task_id}
                    </Link>
                  )}
                </div>
                {e.body && <Markdown source={e.body} className="text-sm" />}
              </li>
            ))}
            {log.length === 0 && (
              <li className="text-sm text-[var(--color-muted)]">No log entries yet.</li>
            )}
          </ul>

          {/* Composer: append a comment or a finding (a finding may link the task tracking it). */}
          <div className="mt-4 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-3">
            <div className="mb-2 flex items-center gap-2 text-xs">
              <span className="text-[var(--color-muted)]">Add</span>
              {(['comment', 'finding'] as const).map((t) => (
                <button
                  key={t}
                  onClick={() => setEntryType(t)}
                  className={`rounded-md px-2 py-0.5 ring-1 ring-inset ${
                    entryType === t
                      ? 'bg-sky-500/15 text-sky-200 ring-sky-500/40'
                      : 'text-[var(--color-muted)] ring-[var(--color-border)] hover:bg-[var(--color-panel-2)]'
                  }`}
                >
                  {t}
                </button>
              ))}
            </div>
            <div className="flex items-end gap-2">
              <AutoGrowTextarea
                value={entryBody}
                onChange={setEntryBody}
                onSubmit={submitEntry}
                placeholder={
                  entryType === 'finding'
                    ? `Describe the finding as ${actor}…`
                    : `Comment as ${actor}…`
                }
                className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2 text-sm outline-none focus:border-sky-500/50"
              />
              {entryType === 'finding' && (
                <input
                  value={findingTask}
                  onChange={(e) => setFindingTask(e.target.value)}
                  placeholder="task # (optional)"
                  inputMode="numeric"
                  className="w-28 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-2 font-mono text-xs outline-none focus:border-sky-500/50"
                />
              )}
              <button
                onClick={submitEntry}
                disabled={busy || !entryBody.trim()}
                className="rounded-md bg-sky-600 px-3 py-2 text-sm font-medium text-white disabled:opacity-40"
              >
                {entryType === 'finding' ? 'Add finding' : 'Comment'}
              </button>
            </div>
          </div>
        </div>
      )}
    </main>
  )
}

const ENTRY_TYPE_CLS: Record<string, string> = {
  finding: 'text-amber-300',
  finding_resolved: 'text-emerald-300',
  state_change: 'text-sky-300',
  decision: 'text-violet-300',
  adversarial_review: 'text-rose-300',
  submitted: 'text-slate-300',
  revised: 'text-slate-300',
  comment: 'text-[var(--color-muted)]',
}

function EntryTypeBadge({ type }: { type: string }) {
  const cls = ENTRY_TYPE_CLS[type] ?? 'text-[var(--color-muted)]'
  return (
    <span className={`rounded bg-[var(--color-panel)] px-1.5 py-0.5 font-mono text-[10px] ${cls}`}>
      {type.replace(/_/g, ' ')}
    </span>
  )
}

// One linked task, fetched for its title + status so the review shows what its findings track.
function LinkedTask({ taskId }: { taskId: number }) {
  const { data: task } = useTask(taskId)
  return (
    <Link
      to={`/tasks/${taskId}`}
      className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2 hover:border-sky-500/40"
    >
      {task && <StatusChip status={task.status} />}
      <span className="min-w-0 flex-1 truncate text-sm">{task?.title ?? `Task #${taskId}`}</span>
      <span className="font-mono text-[11px] text-[var(--color-muted)]">#{taskId}</span>
    </Link>
  )
}
