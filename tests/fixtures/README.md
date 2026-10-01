# wiki-refs fixture

Shared anti-drift corpus for the board's `[[wiki-link]]` / `![[embed]]` syntax, agreed between
the server-side extractor (`extract_wiki_edges` in `src/core.rs`, task_785 -- swapping its raw-byte
regex scan for a pulldown-cmark event-stream walk) and the frontend renderer
(`web/src/markdown.tsx`, task_770 -- remark/unified). Both sides should treat `wiki-refs.md`
identically; `wiki-refs.expected.json` is the server's expected `extract_wiki_edges` output in
insertion order (first-seen-per-(kind,path) wins, matching the existing dedup rule).

## Semantics this fixture pins down

- A `[[path]]` / `[[path|label]]` or `![[path@vN#region|label]]` written inside an inline code
  span or a fenced code block is NOT extracted as an edge -- the extraction only looks at text,
  never code. (`gamma` in the fixture appears only inside code and has zero edges.)
- `![[path]]` is an embed edge ONLY when it is the whole paragraph on its own (block form). An
  inline `![[path]]` occurring mid-sentence demotes to a plain LINK edge, same as a frontend
  inline occurrence never renders a transclusion component. (`beta` in the fixture is inline-only
  and expects a `link` edge, not `embed`.)
- Dedup is per `(kind, path)`, first occurrence wins -- a later occurrence of the same kind+path
  (even with a different label/version/region) does not change the already-recorded edge.
  (`alpha` appears three times: the labeled link form is recorded first and wins over the bare
  repeat; the pinned+labeled embed form is recorded first and wins over the later bare `![[alpha]]`.)
- A dangling link (no document filed at that path) still gets a `link` edge -- resolution/display
  of a dangling target is a renderer concern (the frontend's red-link styling), not an extraction
  concern. (`nowhere` has no filed target but still produces an edge.)

## Frontend rendering notes (no automated test yet -- web/ has no test harness)

For the same `wiki-refs.md`, `web/src/markdown.tsx` renders:

- `[[alpha|Alpha Doc]]` and the later bare `[[alpha]]` each render their OWN link element (no
  frontend-side dedup -- every occurrence is a separate clickable `<a>`), styled resolved or
  dangling per the live `WikiLinkContext`.
- The standalone `![[alpha@v2#intro|Alpha intro]]` paragraph renders as a transclusion (the
  `Embed` component), labeled "Alpha intro", pinned to v2 (`#intro` is accepted but not yet used
  to slice the embedded content -- a pre-existing gap, out of scope for both task_770 and task_785).
- The standalone `![[alpha]]` paragraph renders as a second, separate transclusion of the same
  target (frontend does not dedup repeated embeds either).
- The inline `see ![[beta]] right here` renders the `!` as literal text followed by a plain
  wiki-link for `beta` -- never a transclusion, since embed detection requires the whole paragraph
  to be just the `![[...]]` token.
- `[[nowhere]]` renders as a dangling "red link" (no document filed at that path).
- Everything inside the inline code span and the fenced block (`gamma`) renders as literal code
  text -- no links, no embeds.
