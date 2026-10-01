import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Link, Navigate, Route, Routes, useLocation } from 'react-router-dom'
import './index.css'
import AgentView from './AgentView.tsx'
import Agents from './Agents.tsx'
import Board from './Board.tsx'
import ChannelView from './ChannelView.tsx'
import Channels from './Channels.tsx'
import Documents from './Documents.tsx'
import DocumentView from './DocumentView.tsx'
import Home from './Home.tsx'
import Layout from './Layout.tsx'
import People from './People.tsx'
import ReviewView from './ReviewView.tsx'
import Reviews from './Reviews.tsx'
import Search from './Search.tsx'
import SecretSubmit from './SecretSubmit.tsx'
import Settings from './Settings.tsx'
import Wiki from './Wiki.tsx'
import { TaskDrawer, TaskRedirect } from './TaskDrawer.tsx'

// The app may be served under a reverse-proxy sub-path (e.g. /board), which the backend
// signals via the <base href> it injects from X-Forwarded-Prefix. document.baseURI
// reflects that, so we derive the router basename from it — the same build works at the
// origin root or any sub-path with no build-time config. "/board/" -> "/board"; "/" -> "/".
const basename = new URL(document.baseURI).pathname.replace(/\/$/, '') || '/'

// An absolute link built with the sub-path baked into its root (e.g. an external auto-linkify
// emitting "https://host/board/documents/17") doubles the prefix when opened through a /board
// deployment: the browser lands on literal "/board/board/documents/17", react-router strips the
// basename once leaving the basename-relative "/board/documents/17", which matches no route (task
// 790 -- reported as a blank "No routes matched" page). Recover instead of staying blank: strip
// the repeated segment once and redirect to the de-duped path. A genuine unmatched path (no
// repeated-basename pattern) falls through to a plain not-found message instead of a blank page.
function NotFound() {
  const location = useLocation()
  if (basename !== '/' && location.pathname.startsWith(`${basename}/`)) {
    const fixed = location.pathname.slice(basename.length) || '/'
    return <Navigate to={`${fixed}${location.search}${location.hash}`} replace />
  }
  return (
    <div className="p-6 text-sm text-[var(--color-muted)]">
      <p className="mb-2">Page not found.</p>
      <Link
        to="/"
        className="text-sky-400 underline decoration-dotted underline-offset-2 hover:text-sky-300"
      >
        Back to home
      </Link>
    </div>
  )
}

// Every piece of view state is in the URL: the selected project, and any open task. The
// layout renders the persistent chrome (header, sidebar, activity feed) around nested
// routes — the board for a project, and the task drawer layered over it.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter basename={basename}>
      <Routes>
        {/* Standalone secret-submission page — no board chrome; reached via a capability link. */}
        <Route path="secret-requests/:id" element={<SecretSubmit />} />
        <Route element={<Layout />}>
          <Route index element={<Home />} />
          <Route path="search" element={<Search />} />
          <Route path="settings" element={<Settings />} />
          <Route path="documents" element={<Documents />} />
          <Route path="documents/:documentId" element={<DocumentView />} />
          <Route path="wiki" element={<Wiki />} />
          <Route path="reviews" element={<Reviews />} />
          <Route path="reviews/:reviewId" element={<ReviewView />} />
          <Route path="agents" element={<Agents />} />
          <Route path="agents/:agentId" element={<AgentView />} />
          <Route path="people" element={<People />} />
          <Route path="channels" element={<Channels />} />
          <Route path="channels/:channelId" element={<ChannelView />} />
          {/* Bare task deep-link: resolves the task's project and redirects to the nested URL. */}
          <Route path="tasks/:taskId" element={<TaskRedirect />} />
          <Route path="projects/:projectId" element={<Board />}>
            <Route path="tasks/:taskId" element={<TaskDrawer />} />
          </Route>
          <Route path="*" element={<NotFound />} />
        </Route>
      </Routes>
    </BrowserRouter>
  </StrictMode>,
)
