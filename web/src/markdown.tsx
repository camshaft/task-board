import type { ReactNode } from 'react'

// Minimal, dependency-free Markdown renderer. It emits a React element tree (never
// dangerouslySetInnerHTML), so all text is escaped by React and link hrefs are sanitized —
// safe for rendering agent-authored task descriptions and comments. Supports the common
// subset: ATX headings, fenced + inline code, bold, italic, links, blockquotes, ordered /
// unordered lists, horizontal rules, and paragraphs. Not a full CommonMark implementation;
// anything it doesn't recognize degrades to plain text.

// Only these href schemes render as links; anything else (javascript:, data:, …) falls back
// to plain text so a crafted link can't execute.
const SAFE_HREF = /^(https?:\/\/|mailto:|\/|#)/i

// Inline spans, in priority order: code (verbatim), links, bold, italic. Returns a mix of
// strings (React escapes them) and elements. `gen` yields globally-unique keys.
function inline(text: string, gen: () => number): ReactNode[] {
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
            {inline(m[1], gen)}
          </a>
        ) : (
          <span key={gen()}>{m[0]}</span>
        ),
    ],
    [/\*\*([^*]+)\*\*/, (m) => <strong key={gen()}>{inline(m[1], gen)}</strong>],
    [
      /\*([^*]+)\*|_([^_]+)_/,
      (m) => <em key={gen()}>{inline(m[1] ?? m[2], gen)}</em>,
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

function blocks(src: string): ReactNode[] {
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
      const buf: string[] = []
      i++
      while (i < lines.length && !lines[i].trim().startsWith('```')) {
        buf.push(lines[i])
        i++
      }
      if (i < lines.length) i++ // consume closing fence
      out.push(
        <pre
          key={k++}
          className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 font-mono text-xs"
        >
          <code>{buf.join('\n')}</code>
        </pre>,
      )
      continue
    }

    const h = /^(#{1,6})\s+(.*)$/.exec(line)
    if (h) {
      out.push(heading(h[1].length, inline(h[2], gen), k++))
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
          {blocks(buf.join('\n'))}
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
            <li key={j}>{inline(it, gen)}</li>
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
            <li key={j}>{inline(it, gen)}</li>
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
    out.push(<p key={k++}>{inline(buf.join(' '), gen)}</p>)
  }
  return out
}

// Render `source` as Markdown. The wrapper spaces block elements; callers set the text size.
export function Markdown({ source, className }: { source: string; className?: string }) {
  return <div className={`space-y-2 ${className ?? ''}`}>{blocks(source)}</div>
}
