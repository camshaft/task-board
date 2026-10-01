import { useState } from 'react'
import { Link } from 'react-router-dom'
import { useBoardContext } from './Layout'
import { QuestionComment } from './questions'
import {
  answerQuestion,
  cancelQuestion,
  declineQuestion,
  supersedeQuestion,
  useAwaiting,
  useExternalNameResolver,
} from './resources'
import { useScrollRestoration } from './scrollRestore'
import { relTime, StatusChip } from './ui'

// The operator "awaiting you" view (task_860): one place aggregating everything awaiting the current
// actor's decision -- tasks blocked_on them and open blocking questions routed to them (team-expanded,
// assignee-independent, from GET /api/tasks/awaiting). Questions render as their real element controls
// and are answerable INLINE (reusing QuestionComment), so the operator never has to open each task to
// find and answer what is waiting. Replaces the assignee-keyed "blocked on me" shortcut that surfaced
// only the one task actually assigned to the operator.
export default function Awaiting() {
  const scrollRef = useScrollRestoration<HTMLElement>()
  const { actor } = useBoardContext()
  const { data: awaiting = [], loading } = useAwaiting(actor)
  const resolveExternal = useExternalNameResolver()
  const [busyId, setBusyId] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function run(commentId: number, fn: () => Promise<unknown>) {
    setBusyId(commentId)
    setError(null)
    try {
      await fn()
    } catch (e) {
      setError((e as Error).message)
    } finally {
      setBusyId(null)
    }
  }
  const answerQ = (taskId: number, commentId: number, shape: string, value: unknown) =>
    void run(commentId, () => answerQuestion(taskId, commentId, { shape, value, actor }))
  const declineQ = (taskId: number, commentId: number) => {
    const feedback = window.prompt('Decline this question — a short note on why / what to do instead:')
    if (feedback == null) return
    void run(commentId, () => declineQuestion(taskId, commentId, { feedback, actor }))
  }
  const cancelQ = (taskId: number, commentId: number) => {
    if (!window.confirm('Cancel this question? It will be marked cancelled.')) return
    void run(commentId, () => cancelQuestion(taskId, commentId, { actor }))
  }
  const supersedeQ = (taskId: number, commentId: number) => {
    const new_prompt = window.prompt(
      'Re-pose this question with a new prompt (the old one is kept and linked):',
    )
    if (new_prompt == null || !new_prompt.trim()) return
    void run(commentId, () => supersedeQuestion(taskId, commentId, { new_prompt, actor }))
  }

  const totalQuestions = awaiting.reduce((a, t) => a + t.questions.length, 0)

  return (
    <main ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto p-5">
      <h1 className="mb-1 text-sm font-semibold">
        Awaiting you — <span className="font-mono font-normal">{actor}</span>
      </h1>
      <p className="mb-4 text-xs text-[var(--color-muted)]">
        Everything awaiting your decision: tasks blocked on you and open questions routed to you
        (including any team you are on). Answer questions right here.
      </p>

      {error && (
        <p className="mb-3 rounded-md border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-xs text-rose-300">
          {error}
        </p>
      )}

      {awaiting.length === 0 ? (
        <p className="text-sm text-[var(--color-muted)]">
          {loading ? 'Loading…' : 'Nothing awaiting you right now.'}
        </p>
      ) : (
        <>
          <p className="mb-3 text-xs text-[var(--color-muted)]">
            {awaiting.length} task{awaiting.length === 1 ? '' : 's'}
            {totalQuestions > 0 && `, ${totalQuestions} open question${totalQuestions === 1 ? '' : 's'}`}
            .
          </p>
          <ul className="space-y-3">
            {awaiting.map((t) => (
              <li
                key={t.task_id}
                className="rounded-lg border border-amber-500/30 bg-amber-500/5 p-3"
              >
                <div className="mb-2 flex items-center gap-2">
                  <StatusChip status={t.status} />
                  <Link
                    to={`/tasks/${t.task_id}`}
                    className="min-w-0 flex-1 truncate text-sm font-medium hover:text-sky-300"
                  >
                    {t.task_title}
                  </Link>
                  {t.updated_at && (
                    <span className="hidden text-[11px] text-[var(--color-muted)] sm:inline">
                      {relTime(t.updated_at)}
                    </span>
                  )}
                  <span className="font-mono text-[11px] text-[var(--color-muted)]">
                    task_{t.task_id}
                  </span>
                </div>

                {t.blocked_on_principal && (
                  <p className="mb-2 text-xs text-amber-200/90">
                    Blocked on you
                    {t.blocked_on_note ? `: ${t.blocked_on_note}` : '.'}
                  </p>
                )}

                {t.questions.length > 0 && (
                  <div className="space-y-3">
                    {t.questions.map((q) => (
                      <QuestionComment
                        key={q.id}
                        comment={q}
                        answers={[]}
                        resolveExternal={resolveExternal}
                        actor={actor}
                        busy={busyId === q.id}
                        onAnswer={(shape, value) => answerQ(t.task_id, q.id, shape, value)}
                        onDecline={() => declineQ(t.task_id, q.id)}
                        onCancel={() => cancelQ(t.task_id, q.id)}
                        onSupersede={() => supersedeQ(t.task_id, q.id)}
                      />
                    ))}
                  </div>
                )}

                {t.questions.length === 0 && (
                  <Link
                    to={`/tasks/${t.task_id}`}
                    className="text-xs text-sky-400 hover:text-sky-300"
                  >
                    Open task →
                  </Link>
                )}
              </li>
            ))}
          </ul>
        </>
      )}
    </main>
  )
}
