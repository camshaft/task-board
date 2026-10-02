import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ComponentProps,
  type ReactNode,
} from 'react'
import { Link } from 'react-router-dom'
import ReactMarkdown, {
  defaultUrlTransform,
  type Components,
  type ExtraProps,
} from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { visit } from 'unist-util-visit'
import { findAndReplace } from 'mdast-util-find-and-replace'
import { toString as mdastToString } from 'mdast-util-to-string'
import type { Root as MdastRoot, Paragraph as MdastParagraph } from 'mdast'
import type { Element as HastElement } from 'hast'
import { api, ipfsUrl, type DocumentVersion } from './api'

// Markdown renderer: a real CommonMark parser (remark/unified via react-markdown + remark-gfm),
// extended with a small remark plugin for the board's own non-standard inline/block syntax
// ([[wiki-links]], ![[transclusion embeds]], @mentions, typed-ref #task_N / owner/repo#N
// linkify). The custom syntax is lowered to standard hast `a` (inline refs) or `div` (embed
// blocks) elements carrying a `boardKind` discriminant in their properties, so TypeScript's
// `Components` map only ever needs real tag-name keys — see AnchorRenderer / DivRenderer below.
// Never uses dangerouslySetInnerHTML for parsed markdown (all text is escaped by React); the one
// exception is Mermaid's own sanitized SVG output, unchanged from before.

// Resolves a [[wiki-path]] to the document filed there, or null when nothing is (a dangling
// link, rendered as a wiki "red link"). Provided app-wide from the live wiki listing; the
// default resolves nothing, so a Markdown rendered outside the provider degrades gracefully.
export type WikiResolver = (path: string) => { id: number; title: string } | null
export const WikiLinkContext = createContext<WikiResolver>(() => null)

// Resolve an @mention handle to a link, so a mention of a known agent OR person (OR an alias of
// either) auto-links and highlights, while an unknown @word stays plain text (no dead links).
// Returns the page `href` to link to plus the resolved `canonical` id (so an alias mention like
// @operator can show "@operator -> cameron"), or null when the handle names nothing linkable (the
// mention then degrades to plain text). Provided app-wide from the live agents + people + aliases;
// the default resolves nothing, so mentions degrade to plain text outside the provider. (task_1139
// extended this from agents-only to also cover people, who link to the people page.)
export interface MentionTarget {
  href: string
  canonical: string
}
export type AgentResolver = (id: string) => MentionTarget | null
export const AgentMentionContext = createContext<AgentResolver>(() => null)

// Transclusion recursion state: how deep we are and which paths are already on the embed chain,
// so ![[a]] → ![[b]] → ![[a]] (or an over-deep nest) stops with a placeholder instead of looping.
const MAX_EMBED_DEPTH = 4
const EmbedContext = createContext<{ depth: number; chain: string[] }>({ depth: 0, chain: [] })

// A block-level transclusion: ![[path]], ![[path@vN]] (pinned version), ![[path#region]], and an
// optional |label. Matched only when it's the whole paragraph (block construct, like an image
// embed) — see remarkEmbedBlocks.
const EMBED_RE =
  /^!\[\[\s*([^\]#@|]+?)\s*(?:@v(\d+))?\s*(?:#([^\]|]+?))?\s*(?:\|\s*([^\]]+?)\s*)?\]\]$/

// [[path]] or [[path|label]] — an internal wiki link.
const WIKILINK_RE = /\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g

// External GitHub PR/issue reference: <owner>/<repo>#<N> (e.g. camshaft/fleet#183) → the PR URL
// (GitHub redirects /pull/<N> to /issues/<N> when it's an issue). Repo-qualified ONLY — a bare
// #N is a board task (below). The lookbehind rejects a preceding path/word char (a '/' or letter)
// so it never matches inside a longer path or URL (e.g. ".com/owner/repo#5").
const GITHUB_REF_RE = /(?<![\w./@-])([A-Za-z0-9][\w.-]*)\/([A-Za-z0-9][\w.-]*)#(\d+)\b/g

// Internal typed resource id: #task_472 (canonical, task_869) or the hashless task_472 (tolerated) /
// doc_23 / project_16 / channel_123 → the board deep-link (task 504). The optional leading '#' is
// the canonical form cameron chose; the hashless form still deep-links for back-compat. The
// lookbehind rejects a leading word char / hyphen so "subtask_5" / "todoc_3" don't match (and a '#'
// is neither, so "#task_5" matches with the '#' captured as group 1); \b after the digits rejects
// "task_12ab".
const TYPED_REF_RE = /(?<![\w-])(#?)(task|doc|project|channel)_(\d+)\b/g

// NOTE (task_869): a BARE "#N" (e.g. "#123") is NO LONGER linkified. cameron hard-rejected the bare
// form at write time (it is ambiguous versus a GitHub owner/repo#N and the canonical #task_N), so
// new content cannot contain it; any surviving bare "#N" in old content renders as plain text rather
// than a misleading auto-link. The canonical board-task form is #task_N, matched above.

// @agent mention → the agent's page, but ONLY when it names a known agent (per the app-wide
// resolver, checked at render time in MentionRef); an unknown @word stays plain text (no dead
// links). The lookbehind rejects a leading word char / @ so emails (a@b.com) and @@ don't match.
// Agent ids may contain hyphens (e.g. v-board-ui).
const MENTION_RE = /(?<![\w@])@([a-z0-9][\w-]*)/gi

// Typed resource id prefix -> the client route it deep-links to (task 504).
const REF_ROUTE: Record<string, string> = {
  task: 'tasks',
  doc: 'documents',
  project: 'projects',
  channel: 'channels',
}

// Shared link styling (sky underline) — used by markdown links and the bare-URL / task-ref
// autolinkers below.
const LINK_CLS = 'text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300'
const RED_LINK_CLS =
  'text-rose-400/90 underline decoration-dotted underline-offset-2 hover:text-rose-300'

// remark plugin: recognizes a paragraph consisting of exactly one text node matching EMBED_RE and
// replaces it with a block-level embed node, lowered to a hast <div boardKind="embed-block" ...>
// via data.hName/hProperties (mdast-util-to-hast's generic extension point) so DivRenderer below
// can dispatch to the Embed component.
function remarkEmbedBlocks() {
  return (tree: MdastRoot) => {
    visit(tree, 'paragraph', (node: MdastParagraph, index, parent) => {
      if (!parent || index == null) return
      if (node.children.length !== 1 || node.children[0].type !== 'text') return
      const value = node.children[0].value.trim()
      const m = EMBED_RE.exec(value)
      if (!m) return
      // A custom mdast node type outside the standard union; data.hName above fully overrides
      // how mdast-util-to-hast renders it, so it never needs to satisfy a built-in node shape —
      // hence the `unknown` round-trip cast.
      parent.children[index] = {
        type: 'boardEmbed',
        data: {
          hName: 'div',
          hProperties: {
            boardKind: 'embed-block',
            path: m[1].trim(),
            versionNo: m[2] ? Number(m[2]) : null,
            region: m[3] ?? null,
            label: m[4] ?? null,
          },
        },
        children: [],
      } as unknown as MdastParagraph
    })
  }
}

// remark plugin: lowers the board's custom inline tokens ([[wiki-links]], owner/repo#N,
// #task_N/task_N/doc_N/..., @mentions) to hast <a boardKind="..." ...> elements (see above) so
// AnchorRenderer below can dispatch each to its own small component. Order matters only in that
// each pattern's own lookbehind/word-boundary guards already make them mutually exclusive on
// overlapping text (e.g. GITHUB_REF_RE's "owner/repo#N" is never reachable by TYPED_REF_RE, whose
// lookbehind rejects a word-char-preceded token) — native CommonMark constructs (links, emphasis,
// code spans, autolinks) are handled by remark-parse / remark-gfm and never reach this plugin. A
// bare "#N" is intentionally NOT lowered (task_869): it is no longer an auto-link.
function remarkBoardRefs() {
  return (tree: MdastRoot) => {
    findAndReplace(tree, [
      [
        WIKILINK_RE,
        (_full: string, pathRaw: string, labelRaw?: string) => ({
          type: 'boardRef',
          data: {
            hName: 'a',
            hProperties: {
              boardKind: 'wiki-link',
              path: pathRaw.trim(),
              label: (labelRaw ?? pathRaw).trim(),
            },
          },
        }),
      ],
      [
        GITHUB_REF_RE,
        (full: string, owner: string, repo: string, num: string) => ({
          type: 'boardRef',
          data: {
            hName: 'a',
            hProperties: { boardKind: 'github-ref', owner, repo, num: Number(num), raw: full },
          },
        }),
      ],
      [
        TYPED_REF_RE,
        (full: string, _hash: string, kind: string, num: string) => ({
          type: 'boardRef',
          data: {
            hName: 'a',
            hProperties: { boardKind: 'typed-ref', kind, num: Number(num), raw: full },
          },
        }),
      ],
      [
        MENTION_RE,
        (full: string, id: string) => ({
          type: 'boardRef',
          data: { hName: 'a', hProperties: { boardKind: 'mention', agentId: id, raw: full } },
        }),
      ],
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    ] as any)
  }
}

// remark plugin: assigns a GitHub-style slug id to each heading (same algorithm as before:
// lowercase, strip punctuation, spaces→hyphens, dedupe within one render via `-1`, `-2`, …), only
// when the Markdown caller opts in via `anchors` — see uniqueSlug / slugify.
function remarkHeadingSlugs(slugs: Map<string, number>) {
  return (tree: MdastRoot) => {
    visit(tree, 'heading', (node) => {
      const raw = mdastToString(node)
      const slug = uniqueSlug(raw, slugs)
      const data = (node.data ??= {})
      data.hProperties = { ...(data.hProperties ?? {}), id: slug }
    })
  }
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

// Intra-doc anchor click handler, shared by the heading marker and the `a` component's `#frag`
// branch. Two layers (task 706): 1. the href is an ABSOLUTE-path form (current path + search +
// fragment), NOT the bare `#frag`: the app injects a <base href> for sub-path (/board) proxying,
// against which a bare fragment resolves to the BASE url (a different path) and navigates away
// from the document -> a full SPA reload / blank screen, instead of scrolling. 2. onClick scrolls
// the target heading into view DIRECTLY (and reflects the fragment via replaceState, no
// navigation), so in-doc jumps never depend on base-href / proxy / router resolution at all. The
// href remains as a correct no-JS fallback.
function scrollToFragment(frag: string, e: React.MouseEvent) {
  let target: HTMLElement | null = null
  try {
    target = document.getElementById(decodeURIComponent(frag))
  } catch {
    target = document.getElementById(frag)
  }
  if (target) {
    e.preventDefault()
    target.scrollIntoView({ behavior: 'smooth', block: 'start' })
    history.replaceState(null, '', `${window.location.pathname}${window.location.search}#${frag}`)
  }
}

function fragmentHref(frag: string): string {
  return `${window.location.pathname}${window.location.search}#${frag}`
}

type BoardAnchorProps = ComponentProps<'a'> &
  ExtraProps & {
    boardKind?: string
    path?: string
    label?: string
    agentId?: string
    raw?: string
    kind?: string
    num?: number
    owner?: string
    repo?: string
  }

// Dispatches every `a` element: the board's own lowered ref kinds (wiki-link / github-ref /
// typed-ref / mention), plus the three cases a native markdown link or remark-gfm
// bare-URL autolink can produce (external, in-app absolute path, intra-doc fragment) — ported
// unchanged from the old inline() link branch.
function AnchorRenderer(props: BoardAnchorProps) {
  const { boardKind, path, label, agentId, raw, kind, num, owner, repo, href, children } = props
  const resolve = useContext(WikiLinkContext)
  const mentions = useContext(AgentMentionContext)

  if (boardKind === 'wiki-link' && path != null) {
    const hit = resolve(path)
    return hit ? (
      <Link to={`/documents/${hit.id}`} title={path} className={LINK_CLS}>
        {label}
      </Link>
    ) : (
      <Link to="/wiki" title={`No page filed at "${path}" yet`} className={RED_LINK_CLS}>
        {label}
      </Link>
    )
  }
  if (boardKind === 'github-ref' && owner != null && repo != null) {
    return (
      <a href={`https://github.com/${owner}/${repo}/pull/${num}`} className={LINK_CLS}>
        {raw}
      </a>
    )
  }
  if (boardKind === 'typed-ref' && kind != null) {
    return (
      <Link to={`/${REF_ROUTE[kind]}/${num}`} className={LINK_CLS}>
        {raw}
      </Link>
    )
  }
  if (boardKind === 'mention' && agentId != null) {
    const target = mentions(agentId)
    return target ? (
      <Link
        to={target.href}
        title={target.canonical !== agentId ? `alias: @${agentId} -> ${target.canonical}` : undefined}
        className={LINK_CLS}
      >
        {raw}
      </Link>
    ) : (
      <>{raw}</>
    )
  }

  // Native markdown link / remark-gfm bare-URL autolink.
  if (!href) return <span>{children}</span>
  if (/^(https?:\/\/|mailto:)/i.test(href)) {
    return (
      <a href={href} className={LINK_CLS}>
        {children}
      </a>
    )
  }
  // Links stay in the same tab; the operator opts into a new tab via cmd/ctrl/middle-click
  // (#453). External URLs use a plain <a>; an in-app path navigates client-side via <Link> so the
  // router basename (e.g. a /board sub-path) is preserved — a raw <a href="/documents/2"> would
  // drop the prefix and navigate to a broken URL (#185).
  if (href.startsWith('/') && !href.startsWith('//')) {
    return (
      <Link to={href} className={LINK_CLS}>
        {children}
      </Link>
    )
  }
  if (href.startsWith('#')) {
    const frag = href.slice(1)
    return (
      <a href={fragmentHref(frag)} className={LINK_CLS} onClick={(e) => scrollToFragment(frag, e)}>
        {children}
      </a>
    )
  }
  // Relative / unsafe scheme (javascript:, data:, protocol-relative //) → inert text.
  return <span>{children}</span>
}

// Heading sizes give a clear, readable hierarchy. scroll-mt keeps a deep-linked heading clear of
// the sticky header. With `anchors`, a monospace `#`×level marker precedes the text (from the id
// remarkHeadingSlugs assigned) and links to the section (doubles as a visible depth indicator).
const HEADING_CLS: Record<number, string> = {
  1: 'text-xl font-bold',
  2: 'text-lg font-semibold',
  3: 'text-base font-semibold',
  4: 'text-sm font-semibold',
  5: 'text-sm font-medium',
  6: 'text-sm font-medium text-[var(--color-muted)]',
}

function makeHeading(level: 1 | 2 | 3 | 4 | 5 | 6, anchors: boolean) {
  const Tag = `h${level}` as 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6'
  function Heading({ id, children }: ComponentProps<'h1'> & ExtraProps) {
    const marker =
      anchors && id ? (
        <a
          href={fragmentHref(id)}
          aria-label="Link to this section"
          className="mr-2 select-none font-mono font-normal text-[var(--color-muted)] opacity-50 hover:text-sky-300 hover:opacity-100"
        >
          {'#'.repeat(level)}
        </a>
      ) : null
    return (
      <Tag id={anchors ? id : undefined} className={`scroll-mt-16 ${HEADING_CLS[level]}`}>
        {marker}
        {children}
      </Tag>
    )
  }
  return Heading
}

// Fenced code: dispatches ```mermaid / ```vega-lite (or ```vega) to the matching lazy-loaded
// component, else a plain styled code block. Reads the raw text + language straight off the hast
// node (not the rendered `children`), so this never depends on how/whether `code` is overridden.
function PreBlock({ node }: ExtraProps) {
  const el = node as HastElement | undefined
  const codeNode = el?.children.find(
    (c): c is HastElement => c.type === 'element' && c.tagName === 'code',
  )
  const classNames = (codeNode?.properties?.className as string[] | undefined) ?? []
  const langClass = classNames.find((c) => c.startsWith('language-'))
  const lang = langClass ? langClass.slice('language-'.length).toLowerCase() : ''
  const textChild = codeNode?.children[0]
  const code = textChild && textChild.type === 'text' ? textChild.value : ''

  if (lang === 'mermaid') return <Mermaid code={code} />
  if (lang === 'vega-lite' || lang === 'vegalite' || lang === 'vega') return <VegaLite code={code} />
  return (
    <pre className="overflow-x-auto rounded-md bg-[var(--color-panel-2)] p-3 font-mono text-xs">
      <code>{code}</code>
    </pre>
  )
}

// Inline code span only (fenced blocks are fully handled by PreBlock, which never renders this).
function InlineCode({ children }: ComponentProps<'code'> & ExtraProps) {
  return (
    <code className="rounded bg-[var(--color-panel-2)] px-1 py-0.5 font-mono text-[0.85em]">
      {children}
    </code>
  )
}

type BoardDivProps = ComponentProps<'div'> &
  ExtraProps & {
    boardKind?: string
    path?: string
    versionNo?: number | null
    region?: string | null
    label?: string | null
  }

// Dispatches the lowered block-level ![[transclusion]] to the Embed component; any other `div`
// (markdown itself never produces one) passes through unchanged.
function DivRenderer({ boardKind, path, versionNo, region, label, children, className }: BoardDivProps) {
  if (boardKind === 'embed-block' && path != null) {
    return <Embed path={path} versionNo={versionNo ?? null} region={region ?? null} label={label ?? null} />
  }
  return <div className={className}>{children}</div>
}

function Table({ children }: ComponentProps<'table'> & ExtraProps) {
  return (
    <div className="overflow-x-auto">
      <table className="border-collapse text-sm">{children}</table>
    </div>
  )
}
function Th({ children }: ComponentProps<'th'> & ExtraProps) {
  return (
    <th className="border border-[var(--color-border)] px-2 py-1 text-left font-semibold">
      {children}
    </th>
  )
}
function Td({ children }: ComponentProps<'td'> & ExtraProps) {
  return <td className="border border-[var(--color-border)] px-2 py-1">{children}</td>
}

// A CID is base58 (CIDv0 Qm...) or base32 (CIDv1 bafy...) -- alphanumeric, no separators. This is a
// shape check to avoid treating a stray path as a CID, not a cryptographic validation (the backend
// resolves the real bytes); 40 chars is below the shortest real CID so it never clips a valid one.
function validCid(c: string): string | null {
  return /^[A-Za-z0-9]{40,}$/.test(c) ? c : null
}

// The IPFS CID an image src refers to, or null if it is not a board-internal IPFS reference
// (task_1185). Accepts the canonical ipfs://<cid> form and board-RELATIVE CAS paths (ipfs/<cid> or
// api/ipfs/<cid>, no scheme/host). An ABSOLUTE src -- any scheme other than ipfs:, or a //host --
// returns null and is blocked by the caller, so a public-gateway URL (even one containing /ipfs/)
// never fetches: confidentiality is enforced by the renderer, not by author convention. An embedded
// CID always resolves through ipfsUrl() to the board's internal /api/ipfs route.
export function ipfsCidFromSrc(src: string | undefined): string | null {
  if (!src) return null
  const s = src.trim()
  const scheme = /^ipfs:\/\/([^/?#]+)/i.exec(s)
  if (scheme) return validCid(scheme[1])
  // Reject anything else carrying a scheme (http:, https:, data:, ...) or a protocol-relative host.
  if (/^[a-z][a-z0-9+.-]*:/i.test(s) || s.startsWith('//')) return null
  const path = /(?:^|\/)(?:api\/)?ipfs\/([^/?#]+)/i.exec(s)
  return path ? validCid(path[1]) : null
}

// react-markdown's default urlTransform drops any scheme outside its safe list (http/https/mailto/
// tel/relative), which would blank an ipfs://<cid> src before our img component ever sees it. Let the
// ipfs: scheme through (the img component resolves it to the internal /api/ipfs route and the anchor
// renderer treats a non-navigable scheme as inert text); everything else keeps the default
// sanitization, so javascript:/data: are still stripped.
function boardUrlTransform(url: string): string {
  return /^ipfs:/i.test(url) ? url : defaultUrlTransform(url)
}

function makeComponents(anchors: boolean): Components {
  return {
    a: AnchorRenderer,
    div: DivRenderer,
    pre: PreBlock,
    code: InlineCode,
    ul: ({ children }) => <ul className="list-disc space-y-0.5 pl-5">{children}</ul>,
    ol: ({ children }) => <ol className="list-decimal space-y-0.5 pl-5">{children}</ol>,
    blockquote: ({ children }) => (
      <blockquote className="border-l-2 border-[var(--color-border)] pl-3 text-[var(--color-muted)]">
        {children}
      </blockquote>
    ),
    hr: () => <hr className="border-[var(--color-border)]" />,
    img: ({ src, alt }) => {
      // Only a board-internal IPFS CID renders, resolved to the internal /api/ipfs route via
      // ipfsUrl() (task_1185). A non-CID / absolute src is rendered inert -- never fetched -- so an
      // internal graph cannot leak and a prompt-injected external image URL cannot load.
      const cid = ipfsCidFromSrc(typeof src === 'string' ? src : undefined)
      if (!cid)
        return (
          <span className="text-sm text-[var(--color-muted)]">[image{alt ? `: ${alt}` : ''}]</span>
        )
      return <img src={ipfsUrl(cid)} alt={alt} className="max-h-[70vh] rounded" />
    },
    table: Table,
    th: Th,
    td: Td,
    h1: makeHeading(1, anchors),
    h2: makeHeading(2, anchors),
    h3: makeHeading(3, anchors),
    h4: makeHeading(4, anchors),
    h5: makeHeading(5, anchors),
    h6: makeHeading(6, anchors),
  }
}

// Render `source` as Markdown via remark/unified (react-markdown + remark-gfm), extended with the
// board's own syntax (see remarkBoardRefs / remarkEmbedBlocks above). [[wiki-links]] resolve
// against the app-wide WikiLinkContext (dangling → red-link). `anchors` (document viewer) makes
// headings linkable sections with a `#`-depth margin marker.
export function Markdown({
  source,
  className,
  anchors = false,
}: {
  source: string
  className?: string
  anchors?: boolean
}) {
  const components = useMemo(() => makeComponents(anchors), [anchors])
  // Rebuilt every render (not memoized): a fresh heading-slug Map per render matches the old
  // per-call `opts.slugs = new Map()` dedup scope — reusing one across a changed `source` would
  // leak dedupe suffixes from a previous document into a new one.
  const remarkPlugins: NonNullable<ComponentProps<typeof ReactMarkdown>['remarkPlugins']> = [
    remarkGfm,
    remarkEmbedBlocks,
    remarkBoardRefs,
  ]
  if (anchors) remarkPlugins.push([remarkHeadingSlugs, new Map<string, number>()])

  return (
    <div className={`space-y-2 ${className ?? ''}`}>
      <ReactMarkdown
        remarkPlugins={remarkPlugins}
        components={components}
        urlTransform={boardUrlTransform}
      >
        {source}
      </ReactMarkdown>
    </div>
  )
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
