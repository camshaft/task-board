import { useEffect, useRef, useState } from 'react'
import type { CommentAnnotation } from './api'
import {
  asQuote,
  captureSelectionQuote,
  findQuoteRange,
  paintRegionRanges,
  type RegionQuote,
} from './annotations'
import { Markdown } from './markdown'
import { annotateComment, resolveCommentAnnotation, useCommentAnnotations } from './resources'
import { AuthorLabel, AutoGrowTextarea } from './ui'

// Anchored annotations on a single task comment (task_1033). Renders the comment body, lets the
// reader select a span of it to annotate, paints every open annotation's quote with the shared
// ::highlight(tb-region) pass, and shows the annotation thread (note, resolve, one-level replies)
// below. The selection-to-selector capture, quote-range resolution, and highlight painting are the
// same shared helpers the document view uses, so a comment and a document annotate identically.
export function CommentAnnotations({
  commentId,
  actor,
  body,
  resolveExternal,
}: {
  commentId: number
  actor: string
  body: string
  resolveExternal: (id: string) => string
}) {
  const ref = useRef<HTMLDivElement>(null)
  const { data: annotations = [] } = useCommentAnnotations(commentId)
  const [pending, setPending] = useState<RegionQuote | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [open, setOpen] = useState(false)

  // Paint every open annotation's quote in this comment's rendered body. The shared registry keys
  // ranges by this comment, so several annotated comments in the thread do not clobber one another.
  // Re-runs on content or data change; the cleanup drops this comment's ranges.
  useEffect(() => {
    const el = ref.current
    if (!el) return
    const ranges: Range[] = []
    for (const a of annotations) {
      if (a.status === 'resolved' || a.reply_to != null) continue
      const q = asQuote(a.region)
      if (!q) continue
      const r = findQuoteRange(el, q.exact, q.prefix)
      if (r) ranges.push(r)
    }
    return paintRegionRanges(`comment-${commentId}`, ranges)
  }, [annotations, body, commentId])

  function onMouseUp() {
    const el = ref.current
    if (!el) return
    const q = captureSelectionQuote(el)
    if (q) setPending(q)
  }

  async function run(fn: () => Promise<unknown>) {
    setBusy(true)
    setError(null)
    try {
      await fn()
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  const topLevel = annotations.filter((a) => a.reply_to == null)
  const repliesOf = (id: number) => annotations.filter((a) => a.reply_to === id)
  const openCount = topLevel.filter((a) => a.status !== 'resolved').length

  return (
    <div>
      <div ref={ref} onMouseUp={onMouseUp}>
        <Markdown source={body} className="text-sm" />
      </div>

      {error && <p className="mt-1 text-xs text-rose-400">{error}</p>}

      {pending && (
        <AnnotationComposer
          quote={pending}
          actor={actor}
          busy={busy}
          onSubmit={(note) =>
            run(async () => {
              await annotateComment(commentId, { body: note, principal: actor, region: pending })
              setPending(null)
            })
          }
          onCancel={() => setPending(null)}
        />
      )}

      {topLevel.length > 0 && (
        <div className="mt-2">
          <button
            onClick={() => setOpen((v) => !v)}
            className="text-xs text-[var(--color-muted)] underline decoration-dotted underline-offset-2 hover:text-sky-300"
          >
            {open ? 'Hide' : 'Show'} {topLevel.length} annotation{topLevel.length === 1 ? '' : 's'}
            {openCount > 0 ? ` (${openCount} open)` : ''}
          </button>
          {open && (
            <ul className="mt-2 space-y-2">
              {topLevel.map((a) => (
                <AnnotationThread
                  key={a.id}
                  annotation={a}
                  replies={repliesOf(a.id)}
                  actor={actor}
                  busy={busy}
                  resolveExternal={resolveExternal}
                  onResolve={() =>
                    run(() => resolveCommentAnnotation(a.id, commentId, { principal: actor }))
                  }
                  onReply={(note) =>
                    run(() =>
                      annotateComment(commentId, { body: note, principal: actor, reply_to: a.id }),
                    )
                  }
                />
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  )
}

// The inline composer shown when the reader has selected a span of the comment: the quoted span and
// a note box. Posting anchors a new annotation to that span.
function AnnotationComposer({
  quote,
  actor,
  busy,
  onSubmit,
  onCancel,
}: {
  quote: RegionQuote
  actor: string
  busy: boolean
  onSubmit: (body: string) => void
  onCancel: () => void
}) {
  const [draft, setDraft] = useState('')
  function send() {
    if (draft.trim() && !busy) onSubmit(draft.trim())
  }
  return (
    <div className="mt-2 rounded-md border border-amber-500/40 bg-[var(--color-panel-2)] p-2">
      <blockquote className="mb-1 line-clamp-2 border-l-2 border-amber-500/40 pl-2 text-xs italic text-[var(--color-muted)]">
        {quote.exact}
      </blockquote>
      <div className="flex items-end gap-2">
        <AutoGrowTextarea
          value={draft}
          onChange={setDraft}
          onSubmit={send}
          placeholder={`Annotate selection as ${actor}...`}
          className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-2 py-1 text-xs outline-none focus:border-sky-500/50"
        />
        <button
          onClick={send}
          disabled={busy || !draft.trim()}
          className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
        >
          Annotate
        </button>
        <button
          onClick={onCancel}
          aria-label="Cancel"
          className="rounded-md px-1.5 py-1 text-xs text-[var(--color-muted)] hover:bg-[var(--color-panel)]"
        >
          x
        </button>
      </div>
    </div>
  )
}

// One top-level annotation and its one level of replies: the quoted span, the note, a resolve
// control while open, the replies, and a reply box.
function AnnotationThread({
  annotation,
  replies,
  actor,
  busy,
  resolveExternal,
  onResolve,
  onReply,
}: {
  annotation: CommentAnnotation
  replies: CommentAnnotation[]
  actor: string
  busy: boolean
  resolveExternal: (id: string) => string
  onResolve: () => void
  onReply: (body: string) => void
}) {
  const [draft, setDraft] = useState('')
  const quote = asQuote(annotation.region)
  const resolved = annotation.status === 'resolved'
  function send() {
    if (draft.trim() && !busy) {
      onReply(draft.trim())
      setDraft('')
    }
  }
  return (
    <li className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-2">
      {quote && (
        <blockquote className="mb-1 line-clamp-2 border-l-2 border-amber-500/40 pl-2 text-xs italic text-[var(--color-muted)]">
          {quote.exact}
        </blockquote>
      )}
      <div className="mb-1 flex items-center justify-between text-xs text-[var(--color-muted)]">
        <AuthorLabel
          author={annotation.author}
          externalAuthor={annotation.external_author}
          resolveExternal={resolveExternal}
        />
        {resolved ? (
          <span>resolved</span>
        ) : (
          <button
            onClick={onResolve}
            disabled={busy}
            className="underline decoration-dotted underline-offset-2 hover:text-emerald-300 disabled:opacity-40"
          >
            Resolve
          </button>
        )}
      </div>
      <Markdown source={annotation.body} className="text-sm" />
      {replies.length > 0 && (
        <ul className="mt-2 space-y-1 border-l border-[var(--color-border)] pl-2">
          {replies.map((r) => (
            <li key={r.id}>
              <div className="text-xs text-[var(--color-muted)]">
                <AuthorLabel
                  author={r.author}
                  externalAuthor={r.external_author}
                  resolveExternal={resolveExternal}
                />
              </div>
              <Markdown source={r.body} className="text-sm" />
            </li>
          ))}
        </ul>
      )}
      {!resolved && (
        <div className="mt-2 flex items-end gap-2">
          <AutoGrowTextarea
            value={draft}
            onChange={setDraft}
            onSubmit={send}
            placeholder={`Reply as ${actor}...`}
            className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs outline-none focus:border-sky-500/50"
          />
          <button
            onClick={send}
            disabled={busy || !draft.trim()}
            className="rounded-md bg-sky-600 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40"
          >
            Reply
          </button>
        </div>
      )}
    </li>
  )
}
