import { useState } from 'react'
import { useScrollRestoration } from './scrollRestore'
import type { TeamMember } from './api'
import { useBoardContext } from './Layout'
import {
  addTeamMember,
  createPerson,
  createTeam,
  deletePerson,
  deleteTeam,
  removeTeamMember,
  useAgents,
  useAllTeamDetails,
  useIdentityAliases,
  usePeople,
  useTeam,
  useTeams,
} from './resources'

// Multi-operator people and teams surface (doc_26, task 595 - the first incremental UI slice over
// the live Phase-1 REST contract). People are first-class human identities; teams are addressable
// groups whose members are people or other teams. This view lists and creates both, manages team
// membership (person or nested team), and shows each team's fully-resolved person set (nested
// teams expanded server-side). Visibility/roles and server-side preferences come in later phases.
export default function People() {
  const { actor } = useBoardContext()
  const scrollRef = useScrollRestoration()
  const { data: people = [], loading: peopleLoading } = usePeople()
  const { data: teams = [], loading: teamsLoading } = useTeams()
  // A single selection across both columns: a person (show the teams they are in) or a team (manage
  // its members). The right column is the detail panel for whichever is selected.
  const [selected, setSelected] = useState<{ kind: 'person' | 'team'; id: string } | null>(null)

  const selectionExists =
    selected != null &&
    (selected.kind === 'team'
      ? teams.some((t) => t.id === selected.id)
      : people.some((p) => p.id === selected.id))

  return (
    <main className="flex min-w-0 flex-1 flex-col">
      <div className="flex items-center gap-3 border-b border-[var(--color-border)] px-5 py-3">
        <h1 className="text-sm font-semibold">People and teams</h1>
        <span className="text-xs text-[var(--color-muted)]">
          {people.length} {people.length === 1 ? 'person' : 'people'} · {teams.length}{' '}
          {teams.length === 1 ? 'team' : 'teams'}
        </span>
      </div>
      <div
        ref={scrollRef}
        className="grid min-h-0 flex-1 grid-cols-1 gap-5 overflow-y-auto px-5 py-4 lg:grid-cols-3"
      >
        {/* People */}
        <section className="flex min-w-0 flex-col gap-2">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            People
          </h2>
          <CreateForm
            idPlaceholder="person id (e.g. cameron)"
            onCreate={(id, display_name) => createPerson({ id, display_name, principal: actor })}
          />
          {peopleLoading && people.length === 0 && (
            <p className="text-sm text-[var(--color-muted)]">Loading...</p>
          )}
          {!peopleLoading && people.length === 0 && (
            <p className="text-sm text-[var(--color-muted)]">No people yet.</p>
          )}
          <ul className="flex flex-col gap-1">
            {people.map((p) => {
              const isSelected = selected?.kind === 'person' && selected.id === p.id
              return (
                <li
                  key={p.id}
                  className={`flex items-center gap-2 rounded-md border px-2.5 py-1.5 ${
                    isSelected
                      ? 'border-sky-500/50 bg-sky-500/10'
                      : 'border-[var(--color-border)] bg-[var(--color-panel)] hover:border-sky-500/40'
                  }`}
                >
                  <button
                    type="button"
                    onClick={() => setSelected({ kind: 'person', id: p.id })}
                    aria-pressed={isSelected}
                    className="min-w-0 flex-1 truncate text-left text-sm"
                  >
                    {p.display_name || p.id}
                    {p.display_name && p.display_name !== p.id && (
                      <span className="ml-1.5 font-mono text-[11px] text-[var(--color-muted)]">
                        {p.id}
                      </span>
                    )}
                  </button>
                  <DeleteButton
                    label={`person ${p.id}`}
                    onDelete={async () => {
                      await deletePerson(p.id)
                      if (selected?.kind === 'person' && selected.id === p.id) setSelected(null)
                    }}
                  />
                </li>
              )
            })}
          </ul>
        </section>

        {/* Teams */}
        <section className="flex min-w-0 flex-col gap-2">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Teams
          </h2>
          <CreateForm
            idPlaceholder="team id (e.g. operator)"
            onCreate={(id, display_name) => createTeam({ id, display_name, principal: actor })}
          />
          {teamsLoading && teams.length === 0 && (
            <p className="text-sm text-[var(--color-muted)]">Loading...</p>
          )}
          {!teamsLoading && teams.length === 0 && (
            <p className="text-sm text-[var(--color-muted)]">No teams yet.</p>
          )}
          <ul className="flex flex-col gap-1">
            {teams.map((t) => {
              const isSelected = selected?.kind === 'team' && selected.id === t.id
              return (
                <li
                  key={t.id}
                  className={`flex items-center gap-2 rounded-md border px-2.5 py-1.5 ${
                    isSelected
                      ? 'border-sky-500/50 bg-sky-500/10'
                      : 'border-[var(--color-border)] bg-[var(--color-panel)] hover:border-sky-500/40'
                  }`}
                >
                  <button
                    type="button"
                    onClick={() => setSelected({ kind: 'team', id: t.id })}
                    aria-pressed={isSelected}
                    className="min-w-0 flex-1 truncate text-left text-sm"
                  >
                    {t.display_name || t.id}
                    {t.display_name && t.display_name !== t.id && (
                      <span className="ml-1.5 font-mono text-[11px] text-[var(--color-muted)]">
                        {t.id}
                      </span>
                    )}
                  </button>
                  <DeleteButton
                    label={`team ${t.id}`}
                    onDelete={async () => {
                      await deleteTeam(t.id)
                      if (selected?.kind === 'team' && selected.id === t.id) setSelected(null)
                    }}
                  />
                </li>
              )
            })}
          </ul>
        </section>

        {/* Detail for the current selection: a team's membership, or the teams a person is in. */}
        <section className="flex min-w-0 flex-col gap-2">
          <h2 className="text-xs font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            {selectionExists && selected?.kind === 'person' ? 'Person' : 'Membership'}
          </h2>
          {selectionExists && selected?.kind === 'team' ? (
            <TeamDetailPanel teamId={selected.id} actor={actor} />
          ) : selectionExists && selected?.kind === 'person' ? (
            <PersonDetailPanel personId={selected.id} onSelectTeam={(id) => setSelected({ kind: 'team', id })} />
          ) : (
            <p className="text-sm text-[var(--color-muted)]">
              Select a person to see their teams, or a team to manage its members.
            </p>
          )}
        </section>
      </div>
    </main>
  )
}

// A small "id + optional display name" create form shared by the People and Teams columns.
function CreateForm({
  idPlaceholder,
  onCreate,
}: {
  idPlaceholder: string
  onCreate: (id: string, displayName: string | undefined) => Promise<unknown>
}) {
  const [id, setId] = useState('')
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)

  async function submit() {
    const trimmed = id.trim()
    if (!trimmed || busy) return
    setBusy(true)
    try {
      await onCreate(trimmed, name.trim() || undefined)
      setId('')
      setName('')
    } catch (e) {
      window.alert((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault()
        void submit()
      }}
      className="flex flex-wrap gap-1.5"
    >
      <input
        value={id}
        onChange={(e) => setId(e.target.value)}
        placeholder={idPlaceholder}
        className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono text-xs"
      />
      <input
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="display name (optional)"
        className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs"
      />
      <button
        type="submit"
        disabled={!id.trim() || busy}
        className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-600 disabled:opacity-40"
      >
        Add
      </button>
    </form>
  )
}

// The membership panel for one selected team: its direct members (person or nested team), its
// fully-resolved person set, and controls to add/remove a member.
function TeamDetailPanel({ teamId, actor }: { teamId: string; actor: string }) {
  const { data: team, loading, error } = useTeam(teamId)
  const { data: people = [] } = usePeople()
  const { data: teams = [] } = useTeams()
  const { data: agents = [] } = useAgents()
  const [memberKind, setMemberKind] = useState<'person' | 'team' | 'agent'>('person')
  const [memberId, setMemberId] = useState('')
  const [busy, setBusy] = useState(false)

  async function add() {
    const id = memberId.trim()
    if (!id || busy) return
    setBusy(true)
    try {
      await addTeamMember(teamId, { member_id: id, member_kind: memberKind, principal: actor })
      setMemberId('')
    } catch (e) {
      window.alert((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function remove(m: TeamMember) {
    try {
      await removeTeamMember(teamId, { member_id: m.member_id, member_kind: m.member_kind })
    } catch (e) {
      window.alert((e as Error).message)
    }
  }

  if (loading && !team) return <p className="text-sm text-[var(--color-muted)]">Loading...</p>
  if (error) return <p className="text-sm text-rose-700 dark:text-rose-300">{error.message}</p>
  if (!team) return null

  // Suggest ids for the add control, scoped to the chosen member kind: people, other teams (to
  // nest), or registered agents.
  const suggestions =
    memberKind === 'person'
      ? people.map((p) => p.id)
      : memberKind === 'agent'
        ? agents.map((a) => a.id)
        : teams.filter((t) => t.id !== teamId).map((t) => t.id)

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-[var(--color-border)] bg-[var(--color-panel)] p-3">
      <div className="text-sm font-medium">
        {team.display_name || team.id}
        {team.display_name && team.display_name !== team.id && (
          <span className="ml-1.5 font-mono text-[11px] text-[var(--color-muted)]">{team.id}</span>
        )}
      </div>

      <div>
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Direct members
        </div>
        {team.members.length === 0 ? (
          <p className="text-xs text-[var(--color-muted)]">No members yet.</p>
        ) : (
          <ul className="flex flex-wrap gap-1.5">
            {team.members.map((m) => (
              <li
                key={`${m.member_kind}:${m.member_id}`}
                className="flex items-center gap-1 rounded-full border border-[var(--color-border)] bg-[var(--color-panel-2)] py-0.5 pl-2 pr-1 text-xs"
              >
                <span
                  className={`font-mono ${m.member_kind === 'team' ? 'text-violet-700 dark:text-violet-300' : m.member_kind === 'agent' ? 'text-sky-700 dark:text-sky-300' : ''}`}
                  title={m.member_kind}
                >
                  {m.member_kind === 'team' ? `@${m.member_id}` : m.member_id}
                </span>
                <button
                  type="button"
                  onClick={() => void remove(m)}
                  aria-label={`Remove ${m.member_kind} ${m.member_id}`}
                  className="rounded-full px-1 text-[var(--color-muted)] hover:bg-rose-500/20 hover:text-rose-800 dark:hover:text-rose-300"
                >
                  x
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div>
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Resolved people
          <span className="ml-1 font-normal normal-case">(nested teams expanded)</span>
        </div>
        {team.resolved_people.length === 0 ? (
          <p className="text-xs text-[var(--color-muted)]">None.</p>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {team.resolved_people.map((pid) => (
              <span
                key={pid}
                className="rounded-full bg-emerald-500/15 px-2 py-0.5 font-mono text-xs text-emerald-700 dark:text-emerald-300"
              >
                {pid}
              </span>
            ))}
          </div>
        )}
      </div>

      <div>
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Resolved agents
          <span className="ml-1 font-normal normal-case">(nested teams expanded)</span>
        </div>
        {team.resolved_agents.length === 0 ? (
          <p className="text-xs text-[var(--color-muted)]">None.</p>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {team.resolved_agents.map((aid) => (
              <span
                key={aid}
                className="rounded-full bg-sky-500/15 px-2 py-0.5 font-mono text-xs text-sky-700 dark:text-sky-300"
              >
                {aid}
              </span>
            ))}
          </div>
        )}
      </div>

      <form
        onSubmit={(e) => {
          e.preventDefault()
          void add()
        }}
        className="flex flex-wrap gap-1.5 border-t border-[var(--color-border)] pt-2"
      >
        <select
          value={memberKind}
          onChange={(e) => setMemberKind(e.target.value as 'person' | 'team' | 'agent')}
          className="rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 text-xs"
        >
          <option value="person">person</option>
          <option value="team">team</option>
          <option value="agent">agent</option>
        </select>
        <input
          value={memberId}
          onChange={(e) => setMemberId(e.target.value)}
          placeholder={
            memberKind === 'person'
              ? 'person id to add'
              : memberKind === 'agent'
                ? 'agent id to add'
                : 'team id to nest'
          }
          list="people-teams-member-ids"
          className="min-w-0 flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono text-xs"
        />
        <datalist id="people-teams-member-ids">
          {suggestions.map((s) => (
            <option key={s} value={s} />
          ))}
        </datalist>
        <button
          type="submit"
          disabled={!memberId.trim() || busy}
          className="rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white hover:bg-sky-600 disabled:opacity-40"
        >
          Add member
        </button>
      </form>
    </div>
  )
}

// The detail panel for one selected person: the names they are addressed by (identity aliases that
// resolve to them) and which teams they belong to. The backend has no reverse lookup for either, so
// this inverts the alias map and every team's detail client-side -- team memberships split into
// direct (the person is a listed member) and transitive (resolves in through a nested team).
function PersonDetailPanel({
  personId,
  onSelectTeam,
}: {
  personId: string
  onSelectTeam: (teamId: string) => void
}) {
  const { data: teams = [], loading, error } = useAllTeamDetails()
  const { data: people = [] } = usePeople()
  const { data: aliases = [] } = useIdentityAliases()
  const person = people.find((p) => p.id === personId)
  // Reverse of the alias map: the names that resolve to this person (e.g. operator -> cameron).
  const personAliases = aliases.filter((a) => a.canonical === personId).map((a) => a.alias)

  const directTeams = teams
    .filter((t) => t.members.some((m) => m.member_kind === 'person' && m.member_id === personId))
    .map((t) => t.id)
  const directSet = new Set(directTeams)
  // Resolved-but-not-direct: the person is in the team's resolved_people only via a nested team.
  const viaTeams = teams
    .filter((t) => !directSet.has(t.id) && t.resolved_people.includes(personId))
    .map((t) => t.id)

  if (loading && teams.length === 0)
    return <p className="text-sm text-[var(--color-muted)]">Loading...</p>
  if (error) return <p className="text-sm text-rose-700 dark:text-rose-300">{error.message}</p>

  const teamName = (id: string) => teams.find((t) => t.id === id)?.display_name || id

  function TeamChip({ id }: { id: string }) {
    return (
      <button
        type="button"
        onClick={() => onSelectTeam(id)}
        title={`Open team ${id}`}
        className="rounded-full bg-violet-500/15 px-2 py-0.5 font-mono text-xs text-violet-700 dark:text-violet-300 hover:bg-violet-500/25"
      >
        {teamName(id)}
      </button>
    )
  }

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-[var(--color-border)] bg-[var(--color-panel)] p-3">
      <div className="text-sm font-medium">
        {person?.display_name || personId}
        {person?.display_name && person.display_name !== personId && (
          <span className="ml-1.5 font-mono text-[11px] text-[var(--color-muted)]">{personId}</span>
        )}
      </div>

      {personAliases.length > 0 && (
        <div>
          <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Also known as
            <span className="ml-1 font-normal normal-case">(aliases)</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {personAliases.map((alias) => (
              <span
                key={alias}
                title={`${alias} resolves to ${personId}`}
                className="rounded-full bg-[var(--color-panel-2)] px-2 py-0.5 font-mono text-xs text-[var(--color-muted)]"
              >
                {alias}
              </span>
            ))}
          </div>
        </div>
      )}

      <div>
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
          Member of
          <span className="ml-1 font-normal normal-case">(direct)</span>
        </div>
        {directTeams.length === 0 ? (
          <p className="text-xs text-[var(--color-muted)]">No direct team memberships.</p>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {directTeams.map((id) => (
              <TeamChip key={id} id={id} />
            ))}
          </div>
        )}
      </div>

      {viaTeams.length > 0 && (
        <div>
          <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
            Also in
            <span className="ml-1 font-normal normal-case">(via a nested team)</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {viaTeams.map((id) => (
              <TeamChip key={id} id={id} />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

// A compact delete control with a confirm, shared by the people and teams lists.
function DeleteButton({ label, onDelete }: { label: string; onDelete: () => Promise<unknown> }) {
  const [busy, setBusy] = useState(false)
  return (
    <button
      type="button"
      disabled={busy}
      aria-label={`Delete ${label}`}
      title={`Delete ${label}`}
      onClick={async (e) => {
        // The teams list wraps this button in a selectable row button; don't select on delete.
        e.preventDefault()
        e.stopPropagation()
        if (!window.confirm(`Delete ${label}?`)) return
        setBusy(true)
        try {
          await onDelete()
        } catch (err) {
          window.alert((err as Error).message)
        } finally {
          setBusy(false)
        }
      }}
      className="shrink-0 rounded px-1.5 text-[var(--color-muted)] hover:bg-rose-500/20 hover:text-rose-800 dark:hover:text-rose-300 disabled:opacity-40"
    >
      x
    </button>
  )
}
