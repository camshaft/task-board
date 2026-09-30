import { useState } from 'react'
import { type ReviewTrendSlice } from './api'
import { useReviewTrend } from './resources'

// The review improvement-trend panel (task 377 / BUILD 5 read side): findings-per-review with an
// earlier-vs-later trend, counterbalanced by an escaped-defect signal, overall and sliced by kind
// and producing area. Collapsed by default; the data is fetched only when expanded (its own
// resource key). Dependency-free — plain numbers, arrows, and colored trend labels.
export default function ReviewTrend() {
  const [open, setOpen] = useState(false)
  return (
    <div className="mb-4 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)]">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center justify-between px-3 py-2 text-xs hover:bg-[var(--color-panel-2)]/50"
      >
        <span className="font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Improvement trend
        </span>
        <span className="text-sky-400">{open ? 'hide' : 'show'}</span>
      </button>
      {open && <TrendBody />}
    </div>
  )
}

function TrendBody() {
  const { data, error, loading } = useReviewTrend()
  if (loading && !data) {
    return <p className="px-3 pb-3 text-xs text-[var(--color-muted)]">Loading trend…</p>
  }
  if (error) {
    return <p className="px-3 pb-3 text-xs text-rose-300">{error.message}</p>
  }
  if (!data) return null
  return (
    <div className="space-y-3 border-t border-[var(--color-border)] px-3 py-3">
      <SliceRow label="Overall" slice={data.overall} />
      {data.by_kind.length > 0 && <SliceGroup title="By kind" slices={data.by_kind} labelKey="kind" />}
      {data.by_area.length > 0 && <SliceGroup title="By area" slices={data.by_area} labelKey="area" />}
      <p className="text-[10px] text-[var(--color-muted)]">
        Findings-per-review with an earlier-vs-later trend, counterbalanced by escaped defects
        (post-approval findings, re-opens, lineage follow-ups). A slice where findings fell while
        escaped defects rose is flagged — an improvement that may be shipping the defects instead.
      </p>
    </div>
  )
}

// Improving = fewer findings later (good); worsening = more. Falling escaped = good; rising = bad.
function findingsTrendCls(t: string): string {
  return t === 'improving'
    ? 'text-emerald-300'
    : t === 'worsening'
      ? 'text-rose-300'
      : 'text-[var(--color-muted)]'
}
function escapedTrendCls(t: string): string {
  return t === 'falling'
    ? 'text-emerald-300'
    : t === 'rising'
      ? 'text-rose-300'
      : 'text-[var(--color-muted)]'
}

function SliceRow({ label, slice }: { label: string; slice: ReviewTrendSlice }) {
  const ed = slice.escaped_defects
  return (
    <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-2.5">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
        <span className="font-medium">{label}</span>
        <span className="text-[var(--color-muted)]">
          {slice.reviews} review{slice.reviews === 1 ? '' : 's'}
        </span>
        <span className="font-mono">{slice.findings_per_review} findings/review</span>
        {slice.earlier && slice.later && (
          <span className="font-mono text-[var(--color-muted)]">
            {slice.earlier.findings_per_review} <span aria-hidden>→</span>{' '}
            {slice.later.findings_per_review}
          </span>
        )}
        <span className={findingsTrendCls(slice.findings_trend)}>
          {slice.findings_trend.replace(/_/g, ' ')}
        </span>
        <span className={escapedTrendCls(slice.escaped_trend)}>
          escaped {slice.escaped_trend.replace(/_/g, ' ')}
        </span>
        {slice.flagged && (
          <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-300 ring-1 ring-inset ring-amber-500/30">
            flagged
          </span>
        )}
      </div>
      {ed.total > 0 && (
        <div className="mt-1 text-[10px] text-[var(--color-muted)]">
          escaped: {ed.total} ({ed.post_approval_findings} post-approval, {ed.reopens} re-open
          {ed.reopens === 1 ? '' : 's'}, {ed.lineage_followups} lineage)
        </div>
      )}
    </div>
  )
}

function SliceGroup({
  title,
  slices,
  labelKey,
}: {
  title: string
  slices: ReviewTrendSlice[]
  labelKey: 'kind' | 'area'
}) {
  return (
    <div>
      <div className="mb-1 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
        {title}
      </div>
      <div className="space-y-1.5">
        {slices.map((s, i) => (
          <SliceRow key={i} label={s[labelKey] ?? '—'} slice={s} />
        ))}
      </div>
    </div>
  )
}
