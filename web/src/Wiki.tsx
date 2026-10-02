import { useScrollRestoration } from './scrollRestore'
import { useWiki } from './resources'
import { WikiTree } from './WikiTree'

// The wiki tree browser (/wiki): every path-filed document rendered as a collapsible folder
// tree, independent of the project list (the wiki spans projects). Clicking a page opens the
// document viewer. Live via the store (a path set/rename reshapes the tree over SSE). The tree
// itself is the shared WikiTree component, reused by the contextual docs sidebar (task_1096).
export default function Wiki() {
  const scrollRef = useScrollRestoration()
  const { data: docs = [], loading, error } = useWiki()

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Wiki</h1>
        <p className="mt-0.5 text-xs text-[var(--color-muted)]">
          Documents filed under a path, across every project. File a page from its document view.
        </p>
      </div>
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-3 py-3">
        {error && <p className="px-2 text-sm text-rose-300">{error.message}</p>}
        {loading && docs.length === 0 && (
          <p className="px-2 text-sm text-[var(--color-muted)]">Loading…</p>
        )}
        {!loading && docs.length === 0 && (
          <p className="px-2 text-sm text-[var(--color-muted)]">
            No filed pages yet. Open a document and set its wiki path to file it here.
          </p>
        )}
        {docs.length > 0 && <WikiTree docs={docs} />}
      </div>
    </main>
  )
}
