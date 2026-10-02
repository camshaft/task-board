import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useScrollRestoration } from './scrollRestore'
import { type DocumentSummary } from './api'
import { useWiki } from './resources'
import { DocStatusChip } from './Documents'

// A node in the wiki tree. `doc` is set when a document is filed exactly at this path; a node
// can be BOTH a page and a folder (a doc at "a/b" with another at "a/b/c" makes "a/b" both).
interface Node {
  name: string
  full: string
  children: Map<string, Node>
  doc?: DocumentSummary
}

// Assemble the flat, path-ordered wiki listing into a nested folder tree by splitting each
// document's slash-separated path. Intermediate segments become folder nodes.
function buildTree(docs: DocumentSummary[]): Node {
  const root: Node = { name: '', full: '', children: new Map() }
  for (const d of docs) {
    if (!d.path) continue
    let node = root
    let acc = ''
    for (const seg of d.path.split('/').filter(Boolean)) {
      acc = acc ? `${acc}/${seg}` : seg
      let child = node.children.get(seg)
      if (!child) {
        child = { name: seg, full: acc, children: new Map() }
        node.children.set(seg, child)
      }
      node = child
    }
    node.doc = d
  }
  return root
}

function TreeRows({
  node,
  depth,
  expanded,
  toggle,
}: {
  node: Node
  depth: number
  expanded: Set<string>
  toggle: (full: string) => void
}) {
  const entries = [...node.children.values()].sort((a, b) => a.name.localeCompare(b.name))
  return (
    <>
      {entries.map((n) => {
        const hasChildren = n.children.size > 0
        // Folders open on demand: a node is open only once the reader expands it, so the tree
        // starts collapsed at the top level rather than fully unfolded.
        const isOpen = expanded.has(n.full)
        return (
          <div key={n.full}>
            <div
              className="flex items-center gap-2 rounded px-2 py-1 hover:bg-[var(--color-panel-2)]"
              style={{ paddingLeft: depth * 16 + 8 }}
            >
              {hasChildren ? (
                <button
                  onClick={() => toggle(n.full)}
                  aria-label={isOpen ? 'Collapse' : 'Expand'}
                  className="w-4 shrink-0 text-left text-[var(--color-muted)] hover:text-sky-300"
                >
                  {isOpen ? '▾' : '▸'}
                </button>
              ) : (
                <span className="w-4 shrink-0" />
              )}
              {n.doc ? (
                <>
                  <DocStatusChip status={n.doc.status} />
                  <Link
                    to={`/documents/${n.doc.id}`}
                    className="min-w-0 flex-1 truncate text-sm hover:text-sky-300"
                  >
                    {n.name}
                  </Link>
                </>
              ) : (
                <button
                  onClick={() => toggle(n.full)}
                  className="min-w-0 flex-1 truncate text-left text-sm font-medium text-[var(--color-muted)]"
                >
                  {n.name}/
                </button>
              )}
            </div>
            {hasChildren && isOpen && (
              <TreeRows node={n} depth={depth + 1} expanded={expanded} toggle={toggle} />
            )}
          </div>
        )
      })}
    </>
  )
}

// The wiki tree browser (/wiki): every path-filed document rendered as a collapsible folder
// tree, independent of the project list (the wiki spans projects). Clicking a page opens the
// document viewer. Live via the store (a path set/rename reshapes the tree over SSE). Backlinks
// + [[wiki-link]] resolution land once the document_links backend does.
export default function Wiki() {
  const scrollRef = useScrollRestoration()
  const { data: docs = [], loading, error } = useWiki()
  // Which folders are expanded. Empty by default, so the tree opens collapsed at the top level and
  // the reader discloses children on demand (task_1058 goal: the wiki was unusable fully expanded).
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const toggle = (full: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(full)) n.delete(full)
      else n.add(full)
      return n
    })

  const tree = buildTree(docs)

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
        {docs.length > 0 && (
          <TreeRows node={tree} depth={0} expanded={expanded} toggle={toggle} />
        )}
      </div>
    </main>
  )
}
