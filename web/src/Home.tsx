import { Navigate } from 'react-router-dom'
import { useBoardContext } from './Layout'

// The index route (`/`). With projects loaded, redirect to the first one so the board is
// never blank on entry; `replace` keeps this bounce out of history. Until they load (or if
// there are none), show a prompt.
export default function Home() {
  const { projects } = useBoardContext()
  if (projects.length) return <Navigate to={`projects/${projects[0].id}`} replace />
  return (
    <main className="flex flex-1 items-center justify-center text-sm text-[var(--color-muted)]">
      No projects yet — create one from the sidebar.
    </main>
  )
}
