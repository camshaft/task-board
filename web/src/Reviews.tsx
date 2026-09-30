import { type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import ReviewTrend from './ReviewTrend'
import { useReviews } from './resources'
import { Identity, relTime } from './ui'

// The reviews list + the shared review chrome (status chip, lifecycle order, per-source link)
// reused by the single-review view. A review is a source-agnostic record over an artifact
// (board doc / GitHub PR / change request / URL) with an A2 lifecycle and an append-only log.
// Backed by the resource store; the list omits each review's log (fetch one for its timeline).

const REVIEW_STATUS_CHIP: Record<string, string> = {
  open: 'bg-slate-500/15 text-slate-300 ring-slate-500/30',
  in_review: 'bg-sky-500/15 text-sky-300 ring-sky-500/30',
  changes_requested: 'bg-amber-500/15 text-amber-300 ring-amber-500/30',
  approved: 'bg-emerald-500/15 text-emerald-300 ring-emerald-500/30',
  closed: 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30',
}

export function ReviewStatusChip({ status }: { status: string }) {
  const chip = REVIEW_STATUS_CHIP[status] ?? 'bg-zinc-500/15 text-zinc-400 ring-zinc-500/30'
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${chip}`}
    >
      {status.replace(/_/g, ' ')}
    </span>
  )
}

// The A2 lifecycle in order, for the stepper. changes_requested cycles back to in_review; the
// stepper shows the linear progression and highlights the current state (closed is terminal).
export const REVIEW_STATUS_FLOW = ['open', 'in_review', 'changes_requested', 'approved', 'closed']

// Render a review's source artifact as a link where we can construct a safe URL, else the raw
// ref. `source` names the artifact kind; `target_ref` locates it.
export function reviewSourceLink(source: string | null, targetRef: string | null): ReactNode {
  const cls = 'text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300'
  if (!targetRef) {
    return source ? <span className="text-[var(--color-muted)]">{source}</span> : null
  }
  const ext = (href: string, label: string) => (
    <a href={href} className={cls}>
      {label}
    </a>
  )
  const s = (source ?? '').toLowerCase()
  // board_doc: target_ref is a board document id -> internal link (basename-preserving).
  if (s === 'board_doc' && /^\d+$/.test(targetRef)) {
    return (
      <Link to={`/documents/${targetRef}`} className={cls}>
        document #{targetRef}
      </Link>
    )
  }
  // github_pr given as "owner/repo#123" -> the PR URL.
  if (s === 'github_pr') {
    const m = targetRef.match(/^([^/\s]+\/[^#\s]+)#(\d+)$/)
    if (m) return ext(`https://github.com/${m[1]}/pull/${m[2]}`, targetRef)
  }
  // Anything already an absolute http(s) URL -> external link (covers url, and code_amazon_cr /
  // github_pr supplied as a full URL).
  if (/^https?:\/\//i.test(targetRef)) return ext(targetRef, targetRef)
  // No safely-constructible URL: show the raw ref.
  return <span className="font-mono text-xs text-[var(--color-muted)]">{targetRef}</span>
}

export default function Reviews() {
  const { data: reviews, error, loading } = useReviews()

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Reviews</h1>
        <p className="mt-0.5 text-xs text-[var(--color-muted)]">
          Source-agnostic reviews over artifacts (documents, GitHub PRs, change requests, URLs),
          each with a lifecycle and an append-only log.
        </p>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-3">
        <ReviewTrend />
        {error && <p className="text-sm text-rose-300">{error.message}</p>}
        {loading && !reviews && <p className="text-sm text-[var(--color-muted)]">Loading…</p>}
        {reviews && reviews.length === 0 && (
          <p className="text-sm text-[var(--color-muted)]">No reviews yet.</p>
        )}
        <ul className="space-y-1.5">
          {reviews?.map((r) => (
            <li
              key={r.id}
              className="flex items-center gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-3 py-2"
            >
              <ReviewStatusChip status={r.status} />
              <Link
                to={`/reviews/${r.id}`}
                className="min-w-0 flex-1 truncate text-sm hover:text-sky-300"
              >
                {r.title || `${r.kind} review #${r.id}`}
              </Link>
              {r.vetted && (
                <span className="rounded bg-emerald-500/15 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-emerald-300">
                  vetted
                </span>
              )}
              <span className="hidden rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 font-mono text-[10px] text-[var(--color-muted)] sm:inline">
                {r.kind}
              </span>
              {r.assignee && (
                <Identity
                  id={r.assignee}
                  className="hidden font-mono text-[11px] text-[var(--color-muted)] md:inline"
                />
              )}
              <span className="text-[11px] text-[var(--color-muted)]">{relTime(r.updated_at)}</span>
            </li>
          ))}
        </ul>
      </div>
    </main>
  )
}
