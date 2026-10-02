import { useState } from 'react'
import { useBoardContext } from './Layout'
import { type ThemePref } from './theme'

// The user / settings page (task 520): home for the per-user identity and the theme preference,
// and the place future per-user settings land. Identity + theme are client-local today
// (localStorage); a phase-2 backend preference API would let them follow the user server-side.
const THEME_OPTIONS: { value: ThemePref; label: string; hint: string }[] = [
  { value: 'system', label: 'System', hint: 'Follow your OS appearance' },
  { value: 'light', label: 'Light', hint: 'Always light' },
  { value: 'dark', label: 'Dark', hint: 'Always dark' },
]

export default function Settings() {
  const { actor, setActor, forcedUser, theme } = useBoardContext()
  const [draftActor, setDraftActor] = useState(actor)
  // On a trusted front-door host the server injects + enforces the identity, so the username is
  // read-only here; on localhost / a permissive host it stays a client-local, editable id.
  const locked = forcedUser != null

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">Settings</h1>
        <p className="mt-0.5 text-xs text-[var(--color-muted)]">
          Your identity and appearance on this board. Stored in this browser.
        </p>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        <div className="max-w-lg space-y-8">
          {/* Identity — the actor attributed to your actions (comments, status changes). */}
          <section>
            <h2 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Identity
            </h2>
            <p className="mb-2 text-xs text-[var(--color-muted)]">
              {locked
                ? 'Set by your authenticated session and enforced by the server. It cannot be changed here.'
                : 'The id your actions are attributed to. Trust-on-first-use, no auth.'}
            </p>
            <div className="flex items-center gap-2">
              <input
                value={locked ? actor : draftActor}
                onChange={(e) => !locked && setDraftActor(e.target.value)}
                onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
                onBlur={() =>
                  !locked && draftActor.trim() && draftActor.trim() !== actor && setActor(draftActor)
                }
                disabled={locked}
                aria-readonly={locked}
                className="w-56 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1.5 font-mono text-sm outline-none focus:border-sky-500/50 disabled:cursor-not-allowed disabled:opacity-60"
              />
              {!locked && (
                <button
                  onClick={() => draftActor.trim() && setActor(draftActor)}
                  disabled={!draftActor.trim() || draftActor.trim() === actor}
                  className="rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white disabled:opacity-40"
                >
                  Save
                </button>
              )}
            </div>
            <p className="mt-1 text-[11px] text-[var(--color-muted)]">
              You are <span className="font-mono text-[var(--color-ink)]">{actor}</span>
              {locked && ' (signed in)'}.
            </p>
          </section>

          {/* Appearance — theme preference. System follows prefers-color-scheme live. */}
          <section>
            <h2 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
              Appearance
            </h2>
            <p className="mb-2 text-xs text-[var(--color-muted)]">
              Theme. System follows your OS light/dark setting.
            </p>
            <div className="inline-flex overflow-hidden rounded-md ring-1 ring-inset ring-[var(--color-border)]">
              {THEME_OPTIONS.map((opt) => (
                <button
                  key={opt.value}
                  onClick={() => theme.setPref(opt.value)}
                  title={opt.hint}
                  aria-pressed={theme.pref === opt.value}
                  className={`px-3 py-1.5 text-sm transition ${
                    theme.pref === opt.value
                      ? 'bg-sky-500/20 font-medium text-sky-200'
                      : 'text-[var(--color-muted)] hover:bg-[var(--color-panel-2)]'
                  }`}
                >
                  {opt.label}
                </button>
              ))}
            </div>
          </section>
        </div>
      </div>
    </main>
  )
}
