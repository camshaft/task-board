import { Link, useParams } from 'react-router-dom'
import { Markdown } from './markdown'
import { ReviewStatusChip, REVIEW_STATUS_FLOW, reviewSourceLink } from './Reviews'
import { useReview, useTask } from './resources'
import { relTime, StatusChip } from './ui'

// One review (/reviews/:reviewId): the lifecycle state, the source artifact (linked per source),
// the vetted gate, the append-only log timeline, and the child/proposal tasks its findings track.
// Read-only for now — status transitions and inline commenting are a follow-up; the improvement
// TREND (findings-per-review) is its own surface (needs the trend query, task 376).
export default function ReviewView() {
  const { reviewId } = useParams()
  const id = Number(reviewId)
  const { data: review, error, loading } = useReview(id)

  const log = review?.log ?? []
  // Child/proposal tasks tracked by this review's findings (distinct task ids across the log).
  const linkedTaskIds = [...new Set(log.map((e) => e.task_id).filter((t): t is number => t != null))]

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
                <dd className="font-mono text-xs">{review.assignee}</dd>
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
