import { Navigate } from 'react-router-dom'
import { useProjects } from './resources'

// The index route (`/`). With projects loaded, redirect to the first one so the board is
// never blank on entry; `replace` keeps this bounce out of history. While they load show
// nothing; once loaded with none, prompt to create one.
export default function Home() {
  const { data: projects, loading } = useProjects()
  if (projects?.length) return <Navigate to={`projects/${projects[0].id}`} replace />
  if (loading || !projects) return <main className="flex-1" />
  return (
    <main className="flex flex-1 items-center justify-center text-sm text-[var(--color-muted)]">
      No projects yet — create one from the sidebar.
    </main>
  )
}
