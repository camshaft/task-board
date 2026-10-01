import cidManifest from './ui-element-cids.json'
import elementSet from './ui-elements.json'

// The CID-keyed UI-element registry (task_629, doc_33 v16). An operator-question's `ui` block names
// its element by content id (ui.element_schema_cid); that CID is the element's canonical build-time
// identity -- v-task-board's seeder pins each element's props_schema to the CAS and commits the
// name->CID manifest (ui-element-cids.json). The web joins that manifest with the authored element
// set (ui-elements.json) and branches on the CID. No CID is resolved at runtime: the mapping is
// baked at build time, so a question stamping a known CID renders its element directly.

export interface ElementMeta {
  title: string
  description: string
  component: string
}

// cid -> element name, inverted from the committed manifest.
const nameByCid: Record<string, string> = {}
for (const [name, cid] of Object.entries(cidManifest as Record<string, string>)) {
  nameByCid[cid] = name
}

const metaByName = (elementSet as { elements: Record<string, ElementMeta> }).elements

// The element name a CID refers to, or undefined for an unknown CID (a question stamped with an
// element this build does not know -- the consumer then falls back to its inline response_schema /
// legacy kind).
export function elementNameForCid(cid: string | null | undefined): string | undefined {
  return cid ? nameByCid[cid] : undefined
}

export function elementMeta(name: string | undefined): ElementMeta | undefined {
  return name ? metaByName[name] : undefined
}
