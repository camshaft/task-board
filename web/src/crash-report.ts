import { api, type CrashReport } from './api'

// UI crash telemetry reporter (task_879, Slice B). Funnels uncaught errors -- window 'error' and
// 'unhandledrejection' events, plus the top-level ErrorBoundary's componentDidCatch -- into a
// single fire-and-forget POST to /api/crash-reports, where the backend dedups by stack signature
// and files/bumps one investigation task per distinct crash (so cameron's blank-screen crash would
// have auto-filed itself instead of needing a hand-pasted stack trace).
//
// Hard rule: the reporter must never itself throw or spam. Every path is wrapped so a failure to
// report is swallowed, and we both dedup identical signatures and cap total reports per page load
// (belt to the server's own open-task dedup) so a tight render-loop can't flood the network.

// Distinct crash signatures already reported this page load (client-side dedup), plus a hard cap
// so even novel-signature floods are bounded. The server dedups too; this just avoids the chatter.
const reported = new Set<string>()
let sentCount = 0
const MAX_REPORTS_PER_LOAD = 20

// The loaded app bundle, e.g. "index-DZQnJBiy.js", so a crash pins to a specific deploy. Parsed
// once from the module script tags; '' if it can't be found (never throws).
let cachedBuild: string | undefined
function buildHash(): string {
  if (cachedBuild != null) return cachedBuild
  cachedBuild = ''
  try {
    for (const s of document.querySelectorAll('script[src]')) {
      const src = (s as HTMLScriptElement).src
      const m = /\/(index-[^/]+\.js)(?:\?|$)/.exec(src)
      if (m) {
        cachedBuild = m[1]
        break
      }
    }
  } catch {
    /* leave as '' */
  }
  return cachedBuild
}

// A cheap client-side signature so we don't re-POST the identical crash. Mirrors the server's
// intent (build + first stack line, falling back to the message) without needing to match it.
function signatureOf(r: CrashReport): string {
  const firstFrame = (r.stack ?? r.component_stack ?? '').split('\n')[1]?.trim() ?? ''
  return `${r.build ?? ''}|${firstFrame}|${r.message}`
}

// Fire-and-forget. Fills in the ambient fields (build, url, user_agent, occurred_at), dedups, and
// POSTs; any failure (including the POST itself) is swallowed so reporting can't cascade.
export function reportCrash(partial: Omit<CrashReport, 'build' | 'user_agent' | 'occurred_at'>) {
  try {
    const report: CrashReport = {
      ...partial,
      build: buildHash(),
      url: partial.url ?? location.href,
      user_agent: navigator.userAgent,
      occurred_at: new Date().toISOString(),
    }
    const sig = signatureOf(report)
    if (reported.has(sig) || sentCount >= MAX_REPORTS_PER_LOAD) return
    reported.add(sig)
    sentCount++
    // .catch keeps a failed POST from surfacing as an unhandledrejection (which would re-enter here).
    void api.createCrashReport(report).catch(() => {})
  } catch {
    /* never let the reporter throw */
  }
}

// Normalize an arbitrary thrown value (which need not be an Error) to {message, stack}.
function describe(value: unknown): { message: string; stack?: string } {
  if (value instanceof Error) return { message: value.message || value.name, stack: value.stack }
  if (typeof value === 'string') return { message: value }
  try {
    return { message: JSON.stringify(value) }
  } catch {
    return { message: String(value) }
  }
}

// Install the global handlers. Idempotent-safe to call once at startup. Uses addEventListener
// (not window.onerror=) so we don't clobber anything and don't suppress the default console
// logging. Non-capturing, so resource-load failures (e.g. an ad-blocked <script> 404 -- cameron's
// ERR_NAME_NOT_RESOLVED red herring in task_876) do NOT bubble here and are never mis-reported as
// crashes; only uncaught JS errors and promise rejections are.
export function installCrashReporting() {
  window.addEventListener('error', (e: ErrorEvent) => {
    // A genuine script error carries .error or a message; ignore anything without either.
    if (!e.error && !e.message) return
    const { message, stack } = e.error ? describe(e.error) : { message: e.message, stack: undefined }
    reportCrash({ kind: 'error', message, stack })
  })
  window.addEventListener('unhandledrejection', (e: PromiseRejectionEvent) => {
    const { message, stack } = describe(e.reason)
    reportCrash({ kind: 'unhandledrejection', message, stack })
  })
}
