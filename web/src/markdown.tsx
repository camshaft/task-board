import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import { Link } from 'react-router-dom'
import { api, ipfsUrl, type DocumentVersion } from './api'

// Minimal, dependency-free Markdown renderer. It emits a React element tree (never
// dangerouslySetInnerHTML), so all text is escaped by React and link hrefs are sanitized —
// safe for rendering agent-authored task descriptions and comments. Supports the common
// subset: ATX headings, fenced + inline code, bold, italic, links, [[wiki-links]], blockquotes,
// ordered / unordered lists, horizontal rules, and paragraphs. Not a full CommonMark
// implementation; anything it doesn't recognize degrades to plain text.

// Resolves a [[wiki-path]] to the document filed there, or null when nothing is (a dangling
// link, rendered as a wiki "red link"). Provided app-wide from the live wiki listing; the
// default resolves nothing, so a Markdown rendered outside the provider degrades gracefully.
export type WikiResolver = (path: string) => { id: number; title: string } | null
export const WikiLinkContext = createContext<WikiResolver>(() => null)

// Transclusion recursion state: how deep we are and which paths are already on the embed chain,
// so ![[a]] → ![[b]] → ![[a]] (or an over-deep nest) stops with a placeholder instead of looping.
const MAX_EMBED_DEPTH = 4
const EmbedContext = createContext<{ depth: number; chain: string[] }>({ depth: 0, chain: [] })

// A block-level transclusion: ![[path]], ![[path@vN]] (pinned version), ![[path#region]], and an
// optional |label. Matched only when it's the whole line (block construct, like an image embed).
const EMBED_RE =
  /^!\[\[\s*([^\]#@|]+?)\s*(?:@v(\d+))?\s*(?:#([^\]|]+?))?\s*(?:\|\s*([^\]]+?)\s*)?\]\]$/

// Shared link styling (sky underline) — used by markdown links and the bare-URL / task-ref
// autolinkers below.
const LINK_CLS = 'text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300'

// Inline spans, in priority order: code (verbatim), wiki-links, links, bold, italic, and then
// autolinkers for bare URLs + task refs. Returns a mix of strings (React escapes them) and
// elements. `gen` yields globally-unique keys; `resolve` maps a [[wiki-path]] to its document
// (or null → dangling red-link). The earliest match across all patterns wins, so an explicit
// [text](url) link (its `[` comes first) always beats the bare-URL autolinker on the same URL.
function inline(text: string, gen: () => number, resolve: WikiResolver): ReactNode[] {
  const patterns: [RegExp, (m: RegExpExecArray) => ReactNode][] = [
    [
      /`([^`]+)`/,
      (m) => (
        <code
          key={gen()}
          className="rounded bg-[var(--color-panel-2)] px-1 py-0.5 font-mono text-[0.85em]"
        >
          {m[1]}
        </code>
      ),
    ],
    [
      // [[path]] or [[path|label]] — an internal wiki link. Resolves to the doc filed at that
      // path; a dangling target renders as a distinct "red link" (like a real wiki).
      /\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/,
      (m) => {
        const path = m[1].trim()
        const label = (m[2] ?? m[1]).trim()
        const hit = resolve(path)
        return hit ? (
          <Link
            key={gen()}
            to={`/documents/${hit.id}`}
            title={path}
            className="text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
          >
            {label}
          </Link>
        ) : (
          <Link
            key={gen()}
            to={`/wiki`}
            title={`No page filed at "${path}" yet`}
            className="text-rose-400/90 underline decoration-dotted underline-offset-2 hover:text-rose-300"
          >
            {label}
          </Link>
        )
      },
    ],
    [
      /\[([^\]]+)\]\(([^)\s]+)\)/,
      (m) => {
        const href = m[2]
        const cls =
          'text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300'
        // External links open in a new tab; an in-app path navigates client-side via <Link> so
        // the router basename (e.g. a /board sub-path) is preserved and it stays in the same tab
        // — an <a href="/documents/2"> would drop the prefix and open a broken new tab (#185).
        if (/^(https?:\/\/|mailto:)/i.test(href)) {
          return (
            <a key={gen()} href={href} target="_blank" rel="noreferrer" className={cls}>
              {inline(m[1], gen, resolve)}
            </a>
          )
        }
        if (href.startsWith('/') && !href.startsWith('//')) {
          return (
            <Link key={gen()} to={href} className={cls}>
              {inline(m[1], gen, resolve)}
            </Link>
          )
        }
        if (href.startsWith('#')) {
          return (
            <a key={gen()} href={href} className={cls}>
              {inline(m[1], gen, resolve)}
            </a>
          )
        }
        // Relative / unsafe scheme (javascript:, data:, protocol-relative //) → inert text.
        return <span key={gen()}>{m[0]}</span>
      },
    ],
    [/\*\*([^*]+)\*\*/, (m) => <strong key={gen()}>{inline(m[1], gen, resolve)}</strong>],
    [
      /\*([^*]+)\*|_([^_]+)_/,
      (m) => <em key={gen()}>{inline(m[1] ?? m[2], gen, resolve)}</em>,
    ],
    [
      // Bare URL autolink. Only http/https, so the resulting href is always a safe scheme (no
      // javascript:/data:). The greedy body stops at whitespace; the final char class trims the
      // sentence punctuation that commonly trails a URL in prose (".", ")", ",", …).
      /https?:\/\/[^\s]+[^\s.,;:!?)\]}'"]/,
      (m) => (
        <a key={gen()} href={m[0]} target="_blank" rel="noreferrer" className={LINK_CLS}>
          {m[0]}
        </a>
      ),
    ],
    [
      // Bare task reference (#123) → the task redirect route (which resolves the task's project).
      // The lookbehind rejects a leading word char / another # / & so "abc#1", "##", and numeric
      // HTML entities like "&#123;" don't match; \b after the digits rejects "#12ab".
      /(?<![\w#&])#(\d+)\b/,
      (m) => (
        <Link key={gen()} to={`/tasks/${m[1]}`} className={LINK_CLS}>
          {m[0]}
        </Link>
      ),
    ],
  ]

  const nodes: ReactNode[] = []
  let remaining = text
  while (remaining.length > 0) {
    // Earliest match across all patterns wins, so bold (**) beats italic (*) at the same spot.
    let best: { idx: number; len: number; node: ReactNode } | null = null
    for (const [re, make] of patterns) {
      const m = re.exec(remaining)
      if (m && (best == null || m.index < best.idx)) {
        best = { idx: m.index, len: m[0].length, node: make(m) }
      }
    }
    if (!best) break
    if (best.idx > 0) nodes.push(remaining.slice(0, best.idx))
    nodes.push(best.node)
    remaining = remaining.slice(best.idx + best.len)
  }
  if (remaining) nodes.push(remaining)
  return nodes
}

// Block-rendering options threaded through blocks(): `anchors` turns headings into linkable
// sections (a slug id + a clickable `#`/`##` depth marker), and `slugs` dedupes ids within one
// render (a repeated heading text gets `-1`, `-2`, …). Only the document viewer opts in.
interface BlockOpts {
  anchors: boolean
  slugs: Map<string, number>
}

// GitHub-style heading slug: lowercase, drop punctuation/markdown markers, spaces→hyphens.
function slugify(text: string): string {
  return text
    .toLowerCase()
    .trim()
    .replace(/[^\w\s-]/g, '')
    .replace(/\s+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '')
}

function uniqueSlug(text: string, slugs: Map<string, number>): string {
  const base = slugify(text) || 'section'
  const n = slugs.get(base) ?? 0
  slugs.set(base, n + 1)
  return n === 0 ? base : `${base}-${n}`
}

// Heading sizes give a clear, readable hierarchy (bigger than the old flat text-sm). scroll-mt
// keeps a deep-linked heading clear of the sticky header. With opts.anchors, a monospace `#`×level
// marker precedes the text and links to the section (doubles as a visible depth indicator).
function heading(
  level: number,
  children: ReactNode,
  key: number,
  raw: string,
  opts: BlockOpts,
): ReactNode {
  const cls =
    level === 1
      ? 'text-xl font-bold'
      : level === 2
        ? 'text-lg font-semibold'
        : level === 3
          ? 'text-base font-semibold'
          : level === 4
            ? 'text-sm font-semibold'
            : level === 5
              ? 'text-sm font-medium'
              : 'text-sm font-medium text-[var(--color-muted)]'
  const slug = opts.anchors ? uniqueSlug(raw, opts.slugs) : undefined
  // Use an ABSOLUTE-path href (current path + #slug), not a bare `#slug`: the app injects a
  // <base href> for sub-path proxying, and a bare fragment link resolves against the base (→ the
  // root), navigating away from the doc. An absolute path is left alone by <base>, and since the
  // path is unchanged it's a same-document fragment scroll (no reload, so react-router is fine).
  const marker =
    opts.anchors && slug ? (
      <a
        href={`${window.location.pathname}${window.location.search}#${slug}`}
        aria-label="Link to this section"
        className="mr-2 select-none font-mono font-normal text-[var(--color-muted)] opacity-50 hover:text-sky-300 hover:opacity-100"
      >
        {'#'.repeat(level)}
      </a>
    ) : null
  const full = `scroll-mt-16 ${cls}`
  switch (level) {
    case 1:
      return <h1 key={key} id={slug} className={full}>{marker}{children}</h1>
    case 2:
      return <h2 key={key} id={slug} className={full}>{marker}{children}</h2>
    case 3:
      return <h3 key={key} id={slug} className={full}>{marker}{children}</h3>
    case 4:
      return <h4 key={key} id={slug} className={full}>{marker}{children}</h4>
    case 5:
      return <h5 key={key} id={slug} className={full}>{marker}{children}</h5>
    default:
      return <h6 key={key} id={slug} className={full}>{marker}{children}</h6>
  }
}

function isBlockStart(line: string): boolean {
  const t = line.trim()
  return (
    t.startsWith('```') ||
    EMBED_RE.test(t) ||
    /^#{1,6}\s/.test(line) ||
    /^>\s?/.test(line) ||
    /^\s*[-*+]\s+/.test(line) ||
    /^\s*\d+\.\s+/.test(line) ||
    /^(---+|\*\*\*+|___+)$/.test(t)
  )
}

function blocks(src: string, resolve: WikiResolver, opts: BlockOpts): ReactNode[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const out: ReactNode[] = []
  let i = 0
  let k = 0
  let ikey = 0
  const gen = () => ikey++

  while (i < lines.length) {
    const line = lines[i]
    if (line.trim() === '') {
      i++
      continue
    }
    const t = line.trim()

    const embed = EMBED_RE.exec(t)
    if (embed) {
      out.push(
        <Embed
          key={k++}
          path={embed[1]}
          versionNo={embed[2] ? Number(embed[2]) : null}
          region={embed[3] ?? null}
          label={embed[4] ?? null}
        />,
      )
      i++
      continue
    }

    if (t.startsWith('```')) {
      const lang = t.slice(3).trim().toLowerCase()
      const buf: string[] = []
      i++
      while (i < lines.length && !lines[i].trim().startsWith('```')) {
        buf.push(lines[i])
        i++
      }
      if (i < lines.length) i++ // consume closing fence
      // ```mermaid → diagram, ```vega-lite / ```vega → chart (both lazy-loaded); else code block.
      const code = buf.join('\n')
      out.push(
        lang === 'mermaid' ? (
          <Mermaid key={k++} code={code} />
        ) : lang === 'vega-lite' || lang === 'vegalite' || lang === 'vega' ? (
          <VegaLite key={k++} code={code} />
        ) : (
          <pre
            key={k++}
            className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 font-mono text-xs"
          >
            <code>{code}</code>
          </pre>
        ),
      )
      continue
    }

    const h = /^(#{1,6})\s+(.*)$/.exec(line)
    if (h) {
      out.push(heading(h[1].length, inline(h[2], gen, resolve), k++, h[2], opts))
      i++
      continue
    }

    if (/^(---+|\*\*\*+|___+)$/.test(t)) {
      out.push(<hr key={k++} className="border-[var(--color-border)]" />)
      i++
      continue
    }

    if (/^>\s?/.test(line)) {
      const buf: string[] = []
      while (i < lines.length && /^>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^>\s?/, ''))
        i++
      }
      out.push(
        <blockquote
          key={k++}
          className="border-l-2 border-[var(--color-border)] pl-3 text-[var(--color-muted)]"
        >
          {blocks(buf.join('\n'), resolve, opts)}
        </blockquote>,
      )
      continue
    }

    if (/^\s*[-*+]\s+/.test(line)) {
      const items: string[] = []
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*[-*+]\s+/, ''))
        i++
      }
      out.push(
        <ul key={k++} className="list-disc space-y-0.5 pl-5">
          {items.map((it, j) => (
            <li key={j}>{inline(it, gen, resolve)}</li>
          ))}
        </ul>,
      )
      continue
    }

    if (/^\s*\d+\.\s+/.test(line)) {
      const items: string[] = []
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*\d+\.\s+/, ''))
        i++
      }
      out.push(
        <ol key={k++} className="list-decimal space-y-0.5 pl-5">
          {items.map((it, j) => (
            <li key={j}>{inline(it, gen, resolve)}</li>
          ))}
        </ol>,
      )
      continue
    }

    // Paragraph: gather consecutive non-blank lines that don't start a new block.
    const buf: string[] = []
    while (i < lines.length && lines[i].trim() !== '' && !isBlockStart(lines[i])) {
      buf.push(lines[i])
      i++
    }
    out.push(<p key={k++}>{inline(buf.join(' '), gen, resolve)}</p>)
  }
  return out
}

// Render `source` as Markdown. The wrapper spaces block elements; callers set the text size.
// [[wiki-links]] resolve against the app-wide WikiLinkContext (dangling → red-link).
// `anchors` (document viewer) makes headings linkable sections with a `#`-depth margin marker.
export function Markdown({
  source,
  className,
  anchors = false,
}: {
  source: string
  className?: string
  anchors?: boolean
}) {
  const resolve = useContext(WikiLinkContext)
  const opts: BlockOpts = { anchors, slugs: new Map() }
  return <div className={`space-y-2 ${className ?? ''}`}>{blocks(source, resolve, opts)}</div>
}

// A Mermaid diagram, rendered client-side to SVG. mermaid.js is a heavy dep, so it's loaded
// lazily (dynamic import → its own chunk) only when a diagram actually appears — the base
// bundle stays small. securityLevel 'strict' sanitizes labels and disables click handlers /
// inline HTML (the source is agent-authored). Falls back to the raw source on any error.
export function Mermaid({ code }: { code: string }) {
  const [svg, setSvg] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)
  useEffect(() => {
    let cancelled = false
    import('mermaid')
      .then(async ({ default: mermaid }) => {
        mermaid.initialize({ startOnLoad: false, theme: 'dark', securityLevel: 'strict' })
        const id = `mmd-${Math.random().toString(36).slice(2)}`
        const { svg } = await mermaid.render(id, code)
        if (!cancelled) setSvg(svg)
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [code])

  if (failed) {
    return (
      <pre className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 font-mono text-xs">
        <code>{code}</code>
      </pre>
    )
  }
  if (svg == null) {
    return <div className="p-3 text-xs text-[var(--color-muted)]">Rendering diagram…</div>
  }
  // mermaid's SVG output (sanitized in strict mode); injected as markup since it's not JSX.
  return (
    <div
      className="overflow-x-auto rounded-md border border-[var(--color-border)] bg-white/95 p-3"
      // eslint-disable-next-line react-dom/no-dangerously-set-innerhtml
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  )
}

// A Vega-Lite chart, rendered from its declarative JSON spec (the chart IS the document — no
// chart-builder UI). vega/vega-lite are heavy, so vega-embed is loaded lazily (its own chunk).
// Rendered to SVG with the toolbar disabled; falls back to the raw spec on a parse/render error.
export function VegaLite({ code }: { code: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const [renderError, setRenderError] = useState<string | null>(null)
  // Parse the spec during render (deterministic from `code`), so an invalid spec doesn't need
  // an effect + setState. The effect only runs the async embed.
  const parsed = useMemo<{ spec?: unknown; error?: string }>(() => {
    try {
      return { spec: JSON.parse(code) }
    } catch {
      return { error: 'invalid Vega-Lite JSON spec' }
    }
  }, [code])

  useEffect(() => {
    if (parsed.spec === undefined) return
    let cancelled = false
    let finalize: (() => void) | undefined
    import('vega-embed')
      .then(async ({ default: vegaEmbed }) => {
        if (cancelled || !ref.current) return
        const result = await vegaEmbed(ref.current, parsed.spec as never, {
          actions: false,
          renderer: 'svg',
        })
        if (cancelled) result.view.finalize()
        else finalize = () => result.view.finalize()
      })
      .catch((e) => {
        if (!cancelled) setRenderError((e as Error).message || 'failed to render chart')
      })
    return () => {
      cancelled = true
      finalize?.()
    }
  }, [parsed])

  const error = parsed.error ?? renderError
  if (error) {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] p-3">
        <p className="mb-1 text-xs text-rose-300">Couldn't render chart: {error}</p>
        <pre className="overflow-x-auto font-mono text-xs">
          <code>{code}</code>
        </pre>
      </div>
    )
  }
  // vega renders default (light) colors, so give it a light card for legibility on the dark UI.
  return (
    <div
      ref={ref}
      className="overflow-x-auto rounded-md border border-[var(--color-border)] bg-white/95 p-3"
    />
  )
}

// Renderer family for an embedded version's content_type (a focused subset of the doc viewer's
// dispatch — enough for transclusion).
function embedKindOf(ct: string | null): 'markdown' | 'mermaid' | 'vega' | 'image' | 'text' | 'other' {
  const t = (ct ?? 'text/markdown').toLowerCase().split(';')[0].trim()
  if (t.startsWith('image/')) return 'image'
  if (t === 'text/vnd.mermaid' || t === 'text/x-mermaid') return 'mermaid'
  if (t === 'application/vnd.vegalite+json' || t === 'application/vnd.vega+json') return 'vega'
  if (t === 'text/markdown' || t === 'text/x-markdown' || t === '') return 'markdown'
  if (t.startsWith('text/') || t === 'application/json') return 'text'
  return 'other'
}

type EmbedBody =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'text'; kind: 'markdown' | 'mermaid' | 'vega' | 'text'; text: string }
  | { status: 'url'; kind: 'image' | 'other'; url: string }

// A block-level ![[transclusion]]: renders another document's content inline, dispatched by that
// version's content_type, resolved through the same-origin gateway. Recurses through Markdown for
// embedded markdown (so a doc can embed a doc), bounded by MAX_EMBED_DEPTH + a visited-path chain
// to break cycles. Pinned (@vN) shows that immutable version; otherwise the current one.
export function Embed({
  path,
  versionNo,
  region,
  label,
}: {
  path: string
  versionNo: number | null
  region: string | null
  label: string | null
}) {
  const resolve = useContext(WikiLinkContext)
  const { depth, chain } = useContext(EmbedContext)
  const hit = resolve(path)
  const blocked = !hit || chain.includes(path) || depth >= MAX_EMBED_DEPTH
  const [body, setBody] = useState<EmbedBody>({ status: 'loading' })

  useEffect(() => {
    if (blocked || !hit) return
    let cancelled = false
    ;(async () => {
      try {
        const doc = await api.getDocument(hit.id)
        const v: DocumentVersion | null =
          versionNo != null
            ? (doc.versions.find((x) => x.version_no === versionNo) ?? null)
            : doc.current_version
        if (!v) {
          if (!cancelled) setBody({ status: 'error', message: `v${versionNo} not found` })
          return
        }
        const kind = embedKindOf(v.content_type)
        const url = ipfsUrl(v.cid, v.content_type)
        if (kind === 'image' || kind === 'other') {
          if (!cancelled) setBody({ status: 'url', kind, url })
          return
        }
        const res = await fetch(url)
        if (!res.ok) throw new Error(`gateway ${res.status}`)
        const text = await res.text()
        if (!cancelled) setBody({ status: 'text', kind, text })
      } catch (e) {
        if (!cancelled) setBody({ status: 'error', message: (e as Error).message })
      }
    })()
    return () => {
      cancelled = true
    }
  }, [blocked, hit?.id, versionNo])

  const title = label ?? hit?.title ?? path

  // Dangling: nothing filed at this path — a transclusion "red link".
  if (!hit) {
    return (
      <div className="rounded-md border border-rose-500/40 bg-rose-500/5 px-3 py-2 text-sm">
        <span className="text-[var(--color-muted)]">⧉ embed </span>
        <Link to="/wiki" className="text-rose-400/90 hover:text-rose-300" title={`No page filed at "${path}"`}>
          {title}
        </Link>
        <span className="text-[var(--color-muted)]"> — no page filed</span>
      </div>
    )
  }

  const header = (
    <div className="mb-2 flex items-center gap-2 text-xs text-[var(--color-muted)]">
      <span>⧉ embedded</span>
      <Link to={`/documents/${hit.id}`} className="text-sky-400 hover:text-sky-300">
        {title}
      </Link>
      {versionNo != null && <span className="uppercase">· v{versionNo}</span>}
      {region && <span className="font-mono">· #{region}</span>}
    </div>
  )

  // Cycle or too deep: show the reference but not the content, so the page can't loop/hang.
  if (chain.includes(path) || depth >= MAX_EMBED_DEPTH) {
    return (
      <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-3 py-2">
        {header}
        <p className="text-xs text-[var(--color-muted)]">
          content omitted — {chain.includes(path) ? 'embed cycle' : 'nested too deep'}
        </p>
      </div>
    )
  }

  let content: ReactNode
  if (body.status === 'loading') {
    content = <p className="text-xs text-[var(--color-muted)]">Loading embedded content…</p>
  } else if (body.status === 'error') {
    content = (
      <p className="text-xs text-[var(--color-muted)]">Couldn't load embedded content ({body.message}).</p>
    )
  } else if (body.status === 'url') {
    content =
      body.kind === 'image' ? (
        <img src={body.url} alt={title} className="max-h-[50vh] rounded" />
      ) : (
        <a
          href={body.url}
          target="_blank"
          rel="noreferrer"
          className="text-xs text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
        >
          open embedded content
        </a>
      )
  } else if (body.kind === 'markdown') {
    // Recurse — deeper embeds see an incremented depth + this path on the chain.
    content = (
      <EmbedContext.Provider value={{ depth: depth + 1, chain: [...chain, path] }}>
        <Markdown source={body.text} className="text-sm" />
      </EmbedContext.Provider>
    )
  } else if (body.kind === 'mermaid') {
    content = <Mermaid code={body.text} />
  } else if (body.kind === 'vega') {
    content = <VegaLite code={body.text} />
  } else {
    content = (
      <pre className="overflow-x-auto rounded bg-[var(--color-panel-2)] p-2 font-mono text-xs">
        <code>{body.text}</code>
      </pre>
    )
  }

  return (
    <div className="rounded-md border border-[var(--color-border)] border-l-2 border-l-sky-500/40 bg-[var(--color-panel)] px-3 py-2">
      {header}
      {content}
    </div>
  )
}
