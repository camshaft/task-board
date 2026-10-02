import { Component, type ErrorInfo, type ReactNode } from 'react'
import { reportCrash } from './crash-report'

// A top-level error boundary so an uncaught render error degrades to a recoverable screen instead
// of the blank white page cameron hit on /awaiting (task_876). This is the capture half of UI
// crash telemetry (task_879): componentDidCatch is where the crash reporter will POST the error to
// the board's crash-ingest endpoint once that endpoint lands (owned by v-task-board). Kept
// dependency-free and router-independent on purpose — it wraps the router, so the fallback cannot
// assume react-router context is available (the crash may be in the router itself).
interface Props {
  children: ReactNode
}

interface State {
  error: Error | null
}

export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('Uncaught UI error:', error, info.componentStack)
    // Auto-file the crash for investigation (task_879). Fire-and-forget; reportCrash swallows its
    // own failures, so a reporting error can't compound the one we're already handling.
    reportCrash({
      kind: 'error',
      message: error.message || error.name,
      stack: error.stack,
      component_stack: info.componentStack ?? undefined,
    })
  }

  render() {
    const { error } = this.state
    if (!error) return this.props.children

    // document.baseURI is the app root honoring any reverse-proxy <base href> sub-path, so a plain
    // anchor here lands on the home page without needing router context.
    return (
      <div className="flex min-h-screen flex-col items-center justify-center p-6 text-center">
        <div className="w-full max-w-md space-y-3">
          <h1 className="text-base font-semibold text-rose-300">Something went wrong</h1>
          <p className="text-sm text-[var(--color-muted)]">
            This page hit an unexpected error. Reloading usually fixes it.
          </p>
          {error.message && (
            <pre className="overflow-x-auto whitespace-pre-wrap rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-3 text-left text-xs text-[var(--color-muted)]">
              {error.message}
            </pre>
          )}
          <div className="flex items-center justify-center gap-3">
            <button
              type="button"
              onClick={() => window.location.reload()}
              // Primary buttons use sky-700 (not sky-600): white-on-sky-600 is only ~4.02:1, under
              // the 4.5:1 AA floor for normal text, while white-on-sky-700 is ~5.85:1 (task_1209).
              className="rounded-md bg-sky-700 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-600"
            >
              Reload
            </button>
            <a
              href={document.baseURI}
              className="text-sm text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
            >
              Back to home
            </a>
          </div>
        </div>
      </div>
    )
  }
}
