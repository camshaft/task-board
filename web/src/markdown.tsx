import { createContext, useContext, useEffect, useState, type ReactNode } from 'react'
import { Link } from 'react-router-dom'

// Minimal, dependency-free Markdown renderer. It emits a React element tree (never
// dangerouslySetInnerHTML), so all text is escaped by React and link hrefs are sanitized —
// safe for rendering agent-authored task descriptions and comments. Supports the common
// subset: ATX headings, fenced + inline code, bold, italic, links, [[wiki-links]], blockquotes,
// ordered / unordered lists, horizontal rules, and paragraphs. Not a full CommonMark
// implementation; anything it doesn't recognize degrades to plain text.

// Only these href schemes render as links; anything else (javascript:, data:, …) falls back
// to plain text so a crafted link can't execute.
const SAFE_HREF = /^(https?:\/\/|mailto:|\/|#)/i

// Resolves a [[wiki-path]] to the document filed there, or null when nothing is (a dangling
// link, rendered as a wiki "red link"). Provided app-wide from the live wiki listing; the
// default resolves nothing, so a Markdown rendered outside the provider degrades gracefully.
export type WikiResolver = (path: string) => { id: number; title: string } | null
export const WikiLinkContext = createContext<WikiResolver>(() => null)

// Inline spans, in priority order: code (verbatim), wiki-links, links, bold, italic. Returns a
// mix of strings (React escapes them) and elements. `gen` yields globally-unique keys;
// `resolve` maps a [[wiki-path]] to its document (or null → dangling red-link).
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
      (m) =>
        SAFE_HREF.test(m[2]) ? (
          <a
            key={gen()}
            href={m[2]}
            target="_blank"
            rel="noreferrer"
            className="text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
          >
            {inline(m[1], gen, resolve)}
          </a>
        ) : (
          <span key={gen()}>{m[0]}</span>
        ),
    ],
    [/\*\*([^*]+)\*\*/, (m) => <strong key={gen()}>{inline(m[1], gen, resolve)}</strong>],
    [
      /\*([^*]+)\*|_([^_]+)_/,
      (m) => <em key={gen()}>{inline(m[1] ?? m[2], gen, resolve)}</em>,
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

function heading(level: number, children: ReactNode, key: number): ReactNode {
  const cls =
    level <= 1 ? 'text-base font-semibold' : level === 2 ? 'text-sm font-semibold' : 'text-sm font-medium'
  switch (level) {
    case 1:
      return <h1 key={key} className={cls}>{children}</h1>
    case 2:
      return <h2 key={key} className={cls}>{children}</h2>
    case 3:
      return <h3 key={key} className={cls}>{children}</h3>
    case 4:
      return <h4 key={key} className={cls}>{children}</h4>
    case 5:
      return <h5 key={key} className={cls}>{children}</h5>
    default:
      return <h6 key={key} className={cls}>{children}</h6>
  }
}

function isBlockStart(line: string): boolean {
  const t = line.trim()
  return (
    t.startsWith('```') ||
    /^#{1,6}\s/.test(line) ||
    /^>\s?/.test(line) ||
    /^\s*[-*+]\s+/.test(line) ||
    /^\s*\d+\.\s+/.test(line) ||
    /^(---+|\*\*\*+|___+)$/.test(t)
  )
}

function blocks(src: string, resolve: WikiResolver): ReactNode[] {
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

    if (t.startsWith('```')) {
      const lang = t.slice(3).trim().toLowerCase()
      const buf: string[] = []
      i++
      while (i < lines.length && !lines[i].trim().startsWith('```')) {
        buf.push(lines[i])
        i++
      }
      if (i < lines.length) i++ // consume closing fence
      // A ```mermaid block renders as a diagram (lazy-loaded); anything else is a code block.
      out.push(
        lang === 'mermaid' ? (
          <Mermaid key={k++} code={buf.join('\n')} />
        ) : (
          <pre
            key={k++}
            className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 font-mono text-xs"
          >
            <code>{buf.join('\n')}</code>
          </pre>
        ),
      )
      continue
    }

    const h = /^(#{1,6})\s+(.*)$/.exec(line)
    if (h) {
      out.push(heading(h[1].length, inline(h[2], gen, resolve), k++))
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
          {blocks(buf.join('\n'), resolve)}
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
export function Markdown({ source, className }: { source: string; className?: string }) {
  const resolve = useContext(WikiLinkContext)
  return <div className={`space-y-2 ${className ?? ''}`}>{blocks(source, resolve)}</div>
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
