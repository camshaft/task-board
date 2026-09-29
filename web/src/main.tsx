import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Route, Routes } from 'react-router-dom'
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
import Search from './Search.tsx'
import Wiki from './Wiki.tsx'
import { TaskDrawer } from './TaskDrawer.tsx'

// The app may be served under a reverse-proxy sub-path (e.g. /board), which the backend
// signals via the <base href> it injects from X-Forwarded-Prefix. document.baseURI
// reflects that, so we derive the router basename from it — the same build works at the
// origin root or any sub-path with no build-time config. "/board/" -> "/board"; "/" -> "/".
const basename = new URL(document.baseURI).pathname.replace(/\/$/, '') || '/'

// Every piece of view state is in the URL: the selected project, and any open task. The
// layout renders the persistent chrome (header, sidebar, activity feed) around nested
// routes — the board for a project, and the task drawer layered over it.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter basename={basename}>
      <Routes>
        <Route element={<Layout />}>
          <Route index element={<Home />} />
          <Route path="search" element={<Search />} />
          <Route path="documents" element={<Documents />} />
          <Route path="documents/:documentId" element={<DocumentView />} />
          <Route path="wiki" element={<Wiki />} />
          <Route path="agents" element={<Agents />} />
          <Route path="agents/:agentId" element={<AgentView />} />
          <Route path="channels" element={<Channels />} />
          <Route path="channels/:channelId" element={<ChannelView />} />
          <Route path="projects/:projectId" element={<Board />}>
            <Route path="tasks/:taskId" element={<TaskDrawer />} />
          </Route>
        </Route>
      </Routes>
    </BrowserRouter>
  </StrictMode>,
)
