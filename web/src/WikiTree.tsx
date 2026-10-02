import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { type DocumentSummary } from './api'
import { DocStatusChip } from './Documents'

// A node in the wiki tree. `doc` is set when a document is filed exactly at this path; a node
// can be BOTH a page and a folder (a doc at "a/b" with another at "a/b/c" makes "a/b" both).
interface WikiNode {
  name: string
  full: string
  children: Map<string, WikiNode>
  doc?: DocumentSummary
}

// Assemble the flat, path-ordered wiki listing into a nested folder tree by splitting each
// document's slash-separated path. Intermediate segments become folder nodes.
function buildWikiTree(docs: DocumentSummary[]): WikiNode {
  const root: WikiNode = { name: '', full: '', children: new Map() }
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
  activeDocId,
  onNavigate,
}: {
  node: WikiNode
  depth: number
  expanded: Set<string>
  toggle: (full: string) => void
  activeDocId?: number | null
  onNavigate?: () => void
}) {
  const entries = [...node.children.values()].sort((a, b) => a.name.localeCompare(b.name))
  return (
    <>
      {entries.map((n) => {
        const hasChildren = n.children.size > 0
        // Folders open on demand: a node is open only once the reader expands it, so the tree
        // starts collapsed at the top level rather than fully unfolded.
        const isOpen = expanded.has(n.full)
        const isActive = n.doc != null && n.doc.id === activeDocId
        return (
          <div key={n.full}>
            <div
              className={`flex items-center gap-2 rounded px-2 py-1 ${
                isActive ? 'bg-sky-500/15 font-medium' : 'hover:bg-[var(--color-panel-2)]'
              }`}
              style={{ paddingLeft: depth * 16 + 8 }}
            >
              {hasChildren ? (
                <button
                  onClick={() => toggle(n.full)}
                  aria-label={isOpen ? 'Collapse' : 'Expand'}
                  className="w-4 shrink-0 text-left text-[var(--color-muted)] hover:text-sky-800 dark:hover:text-sky-300"
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
                    onClick={onNavigate}
                    className="min-w-0 flex-1 truncate text-sm hover:text-sky-800 dark:hover:text-sky-300"
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
              <TreeRows
                node={n}
                depth={depth + 1}
                expanded={expanded}
                toggle={toggle}
                activeDocId={activeDocId}
                onNavigate={onNavigate}
              />
            )}
          </div>
        )
      })}
    </>
  )
}

// The shared wiki tree: path-filed documents rendered as a collapsible folder tree, used by both
// the full Wiki page and the contextual docs sidebar. Starts collapsed at the top level and
// discloses children on demand (task_1058 Phase-1), but auto-expands the ancestors of the active
// document so the open page stays visible in the tree, and highlights it.
export function WikiTree({
  docs,
  activeDocId,
  onNavigate,
}: {
  docs: DocumentSummary[]
  activeDocId?: number | null
  onNavigate?: () => void
}) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const toggle = (full: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(full)) n.delete(full)
      else n.add(full)
      return n
    })

  // Reveal the active doc: expand (never collapse) its folder ancestors when it changes.
  useEffect(() => {
    if (activeDocId == null) return
    const doc = docs.find((d) => d.id === activeDocId)
    if (!doc?.path) return
    const parts = doc.path.split('/').filter(Boolean)
    parts.pop() // the doc's own leaf segment; expand only its ancestor folders
    const ancestors: string[] = []
    let acc = ''
    for (const seg of parts) {
      acc = acc ? `${acc}/${seg}` : seg
      ancestors.push(acc)
    }
    if (ancestors.length === 0) return
    setExpanded((s) => {
      const n = new Set(s)
      let changed = false
      for (const a of ancestors)
        if (!n.has(a)) {
          n.add(a)
          changed = true
        }
      return changed ? n : s
    })
  }, [activeDocId, docs])

  const tree = buildWikiTree(docs)
  return (
    <TreeRows
      node={tree}
      depth={0}
      expanded={expanded}
      toggle={toggle}
      activeDocId={activeDocId}
      onNavigate={onNavigate}
    />
  )
}
