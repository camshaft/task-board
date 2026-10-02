import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { type ProjectRole } from './api'
import { useBoardContext } from './Layout'
import { useScrollRestoration } from './scrollRestore'
import {
  attachProjectTeam,
  detachProjectTeam,
  useProjectTeams,
  useProjects,
  useTeams,
} from './resources'

const ROLES: ProjectRole[] = ['admin', 'read-write', 'read']

// Role -> chip classes. admin is the strongest (amber), read-write mid (sky), read weakest (muted).
const ROLE_CHIP: Record<string, string> = {
  admin: 'bg-amber-100 text-amber-800 ring-amber-500/30 dark:bg-amber-500/15 dark:text-amber-300',
  'read-write': 'bg-sky-100 text-sky-800 ring-sky-500/30 dark:bg-sky-500/15 dark:text-sky-300',
  read: 'bg-zinc-100 text-zinc-700 ring-zinc-500/30 dark:bg-zinc-500/15 dark:text-zinc-300',
}

function RoleChip({ role }: { role: string }) {
  const chip =
    ROLE_CHIP[role] ?? 'bg-zinc-100 text-zinc-700 ring-zinc-500/30 dark:bg-zinc-500/15 dark:text-zinc-300'
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${chip}`}
    >
      {role}
    </span>
  )
}

// Project visibility + roles (/projects/:id/access, task 542 Phase 3). Shows and edits the team
// grants on a project and the resolved per-principal access map. Record-only: the board surfaces and
// records grants but does NOT enforce them yet (enforcement is Phase 3 Part B), so the page says so.
export default function ProjectAccess() {
  const { projectId } = useParams()
  const pid = Number(projectId)
  const scrollRef = useScrollRestoration()
  const { actor } = useBoardContext()
  const { data, loading, error } = useProjectTeams(pid)
  const { data: teams = [] } = useTeams()
  const { data: projects = [] } = useProjects()
  const projectName = projects.find((p) => p.id === pid)?.name ?? `#${pid}`

  const grants = data?.teams ?? []
  const access = data?.access ?? {}
  const grantedTeamIds = new Set(grants.map((g) => g.team_id))

  const [teamId, setTeamId] = useState('')
  const [role, setRole] = useState<ProjectRole>('read')
  const [cascade, setCascade] = useState(true)
  const [busy, setBusy] = useState(false)
  const [opError, setOpError] = useState<string | null>(null)

  async function run(fn: () => Promise<unknown>) {
    setBusy(true)
    setOpError(null)
    try {
      await fn()
    } catch (e) {
      setOpError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  function grant() {
    const t = teamId.trim()
    if (!t || busy) return
    void run(async () => {
      await attachProjectTeam(pid, { team_id: t, role, cascade, principal: actor })
      setTeamId('')
    })
  }

  // Teams available to grant: all teams, with the ones already granted marked (attach is idempotent
  // and also serves as a role/cascade update, so granted teams stay selectable).
  const accessEntries = Object.entries(access).sort((a, b) => a[0].localeCompare(b[0]))

  return (
    <main ref={scrollRef} className="min-w-0 flex-1 overflow-y-auto">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <Link to={`/projects/${pid}`} className="text-xs text-[var(--color-muted)] hover:text-sky-300">
          ← Board
        </Link>
        <h1 className="truncate text-sm font-semibold">Access — {projectName}</h1>
      </div>

      {/* Record-only notice: grants are surfaced + editable but not enforced yet. */}
      <div className="border-b border-amber-500/30 bg-amber-500/10 px-5 py-2 text-xs text-amber-200">
        Recording only — these grants are stored and shown, but access is not enforced yet
        (enforcement lands in a later phase). Editing here does not change what anyone can see or do.
      </div>

      {error && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {error.message}
        </div>
      )}
      {opError && (
        <div className="border-b border-rose-500/30 bg-rose-500/10 px-5 py-2 text-sm text-rose-300">
          {opError}
        </div>
      )}
      {loading && !data && <p className="px-5 py-3 text-sm text-[var(--color-muted)]">Loading…</p>}

      <div className="grid gap-6 px-5 py-4 lg:grid-cols-2">
        {/* Team grants + editor. */}
        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Team grants ({grants.length})
          </h2>

          <form
            onSubmit={(e) => {
              e.preventDefault()
              grant()
            }}
            className="mb-3 flex flex-wrap items-center gap-1.5"
          >
            <select
              value={teamId}
              onChange={(e) => setTeamId(e.target.value)}
              className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs"
            >
              <option value="">team to grant…</option>
              {teams.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.display_name || t.id}
                  {grantedTeamIds.has(t.id) ? ' (update)' : ''}
                </option>
              ))}
            </select>
            <select
              value={role}
              onChange={(e) => setRole(e.target.value as ProjectRole)}
              className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs"
            >
              {ROLES.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
            <label className="flex items-center gap-1 text-xs text-[var(--color-muted)]">
              <input type="checkbox" checked={cascade} onChange={(e) => setCascade(e.target.checked)} />
              cascade
            </label>
            <button
              type="submit"
              disabled={!teamId.trim() || busy}
              className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-600 disabled:opacity-40"
            >
              Grant
            </button>
          </form>

          {grants.length === 0 ? (
            <p className="text-sm text-[var(--color-muted)]">
              No team grants yet. The project creator always has admin.
            </p>
          ) : (
            <ul className="flex flex-col gap-1">
              {grants.map((g) => (
                <li
                  key={g.team_id}
                  className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-2.5 py-1.5"
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-sm">{g.team_id}</span>
                  {!g.cascade && (
                    <span className="rounded bg-[var(--color-panel-2)] px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-[var(--color-muted)]">
                      direct only
                    </span>
                  )}
                  <RoleChip role={g.role} />
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() =>
                      void run(() => detachProjectTeam(pid, { team_id: g.team_id, principal: actor }))
                    }
                    aria-label={`Revoke ${g.team_id}`}
                    title={`Revoke ${g.team_id}`}
                    className="shrink-0 rounded px-1.5 text-[var(--color-muted)] hover:bg-rose-500/20 hover:text-rose-300 disabled:opacity-40"
                  >
                    x
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        {/* Resolved access map. */}
        <section>
          <h2 className="mb-2 text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Resolved access ({accessEntries.length})
            <span className="ml-1 font-normal normal-case">— strongest role wins</span>
          </h2>
          {accessEntries.length === 0 ? (
            <p className="text-sm text-[var(--color-muted)]">No one resolves to access yet.</p>
          ) : (
            <ul className="flex flex-col gap-1">
              {accessEntries.map(([principal, a]) => (
                <li
                  key={principal}
                  className="flex items-center gap-2 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] px-2.5 py-1.5"
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-sm">{principal}</span>
                  <span className="text-[11px] text-[var(--color-muted)]">{a.kind}</span>
                  <span className="hidden text-[11px] text-[var(--color-muted)] sm:inline" title="granted via">
                    via {a.via}
                  </span>
                  <RoleChip role={a.role} />
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>
    </main>
  )
}
