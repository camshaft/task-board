// Shared text-anchored annotation helpers (task_1033). The document view and the task-comment
// thread both anchor a note to a highlighted span of immutable rendered text, using a W3C-style
// text-quote selector and the CSS Custom Highlight API to paint it with no DOM surgery. These are
// the pieces common to both; the per-view wiring (state, pins, popovers) stays in each component.

// A client-defined text-quote region selector (the board stores a comment/annotation region as
// opaque JSON).
export interface RegionQuote {
  type: 'text-quote'
  exact: string
  prefix?: string
  suffix?: string
}

// How much surrounding context to capture on each side of a selected quote, for disambiguation and
// highlight matching against the shown text.
const CONTEXT = 32

// Build a text-quote selector for `exact` within `fullText`, capturing a little surrounding context
// (prefix/suffix) to disambiguate repeated text. When `exact` is not found, prefix/suffix are empty
// and the quote still carries the exact text.
export function buildTextQuote(fullText: string, exact: string): RegionQuote {
  const idx = fullText.indexOf(exact)
  return {
    type: 'text-quote',
    exact,
    prefix: idx > 0 ? fullText.slice(Math.max(0, idx - CONTEXT), idx) : '',
    suffix: idx >= 0 ? fullText.slice(idx + exact.length, idx + exact.length + CONTEXT) : '',
  }
}

// Capture the current window selection inside `container` as a text-quote region, or null when
// there is no usable selection (collapsed, outside the container, or shorter than two characters).
export function captureSelectionQuote(container: HTMLElement): RegionQuote | null {
  const sel = window.getSelection()
  if (!sel || sel.isCollapsed || !sel.anchorNode || !container.contains(sel.anchorNode)) return null
  const exact = sel.toString().trim()
  if (exact.length < 2) return null
  return buildTextQuote(container.textContent ?? '', exact)
}

// Narrow an opaque region to a text-quote selector (with a usable `exact`), else null.
export function asQuote(region: unknown): RegionQuote | null {
  if (region && typeof region === 'object') {
    const r = region as Record<string, unknown>
    if (r.type === 'text-quote' && typeof r.exact === 'string' && r.exact.length > 0) {
      return {
        type: 'text-quote',
        exact: r.exact,
        prefix: typeof r.prefix === 'string' ? r.prefix : undefined,
        suffix: typeof r.suffix === 'string' ? r.suffix : undefined,
      }
    }
  }
  return null
}

// Shared CSS Custom Highlight registry for anchored regions. Several owners (each annotated comment
// in a list) contribute ranges to the single '::highlight(tb-region)' pass, so a per-owner call
// must not clobber the others. paintRegionRanges records this owner's ranges and rebuilds the
// highlight from every owner; the returned cleanup drops this owner (call it on unmount or before
// recomputing). Degrades to a no-op where the CSS Custom Highlight API is absent.
const highlightOwners = new Map<string, Range[]>()

function rebuildRegionHighlight(): void {
  const highlights = (globalThis.CSS as unknown as { highlights?: Map<string, unknown> })?.highlights
  const HighlightCtor = (globalThis as unknown as { Highlight?: new () => { add: (r: Range) => void } })
    .Highlight
  if (!highlights || !HighlightCtor) return
  const all: Range[] = []
  for (const rs of highlightOwners.values()) for (const r of rs) all.push(r)
  if (all.length === 0) {
    highlights.delete('tb-region')
    return
  }
  const hl = new HighlightCtor()
  for (const r of all) hl.add(r)
  highlights.set('tb-region', hl as unknown)
}

export function paintRegionRanges(ownerKey: string, ranges: Range[]): () => void {
  if (ranges.length > 0) highlightOwners.set(ownerKey, ranges)
  else highlightOwners.delete(ownerKey)
  rebuildRegionHighlight()
  return () => {
    highlightOwners.delete(ownerKey)
    rebuildRegionHighlight()
  }
}

// Locate a quote (optionally disambiguated by its preceding prefix) in a container's rendered text
// and return a DOM Range spanning it - walking text nodes so a match that spans elements still
// resolves. Returns null when the quote isn't present in the shown content.
export function findQuoteRange(container: HTMLElement, exact: string, prefix?: string): Range | null {
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT)
  const nodes: Text[] = []
  const starts: number[] = []
  let full = ''
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    starts.push(full.length)
    nodes.push(n as Text)
    full += (n as Text).data
  }
  if (nodes.length === 0) return null
  let exactAt = -1
  if (prefix) {
    const withPrefix = full.indexOf(prefix + exact)
    if (withPrefix >= 0) exactAt = withPrefix + prefix.length
  }
  if (exactAt < 0) exactAt = full.indexOf(exact)
  if (exactAt < 0) return null
  const locate = (pos: number) => {
    for (let i = nodes.length - 1; i >= 0; i--) {
      if (starts[i] <= pos) return { node: nodes[i], offset: Math.min(pos - starts[i], nodes[i].length) }
    }
    return { node: nodes[0], offset: 0 }
  }
  const s = locate(exactAt)
  const e = locate(exactAt + exact.length)
  const range = document.createRange()
  try {
    range.setStart(s.node, s.offset)
    range.setEnd(e.node, e.offset)
  } catch {
    return null
  }
  return range
}
