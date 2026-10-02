// Live updates: open a Server-Sent Events connection to the board's activity feed and pipe
// every event through applyStreamEvent, which invalidates the affected resources. Mount this
// once (in Layout) and the whole UI becomes live — any change made by another client or an
// MCP agent refreshes the relevant panels automatically. EventSource handles reconnection and
// replays missed events via Last-Event-ID (the server keys events by their log seq).
//
// Idle-pause (task_1314): holding an SSE connection open on a backgrounded tab wastes a server
// slot + keep-alive traffic for a UI nobody is looking at. So we release the stream once the tab
// has been hidden for a grace period, and reopen it when the tab becomes visible again. Brief
// tab-switches keep the stream (the grace timer avoids churn); only sustained idle releases it.
// A reopened stream is a *fresh* EventSource, so it does NOT carry the previous Last-Event-ID and
// the server won't replay events emitted while we were paused — so on resume we also invalidate
// the live resources to resync the UI to current state.

import { useEffect } from 'react'
import { api } from './api'
import { applyStreamEvent, type StreamEvent } from './resources'
import { invalidateMatching, setStreamConnected } from './store'

// Keep the stream through a quick glance away; release it only after the tab has stayed hidden
// this long. Long enough that normal tab-flipping never tears the connection down, short enough
// that a genuinely idle tab stops holding a slot promptly.
const HIDDEN_GRACE_MS = 15000

export function useLiveUpdates() {
  useEffect(() => {
    let es: EventSource | null = null
    let graceTimer: ReturnType<typeof setTimeout> | null = null

    const open = () => {
      if (es) return
      es = new EventSource(api.streamUrl())
      es.onopen = () => setStreamConnected(true)
      es.onmessage = (e) => {
        try {
          applyStreamEvent(JSON.parse(e.data) as StreamEvent)
        } catch {
          // A keep-alive comment or malformed frame — ignore; the next real event will refresh.
        }
      }
      // On error EventSource auto-reconnects (resuming from the last event id); nothing to do but
      // let it. Surface the drop as part of connection health (task 549) so the UI can show a
      // "reconnecting" hint; onopen clears it once the stream is back.
      es.onerror = () => setStreamConnected(false)
    }

    const pause = () => {
      if (!es) return
      es.close()
      es = null
      // A deliberate idle-pause isn't a real outage — don't leave the health signal stuck "down"
      // (same reasoning as unmount cleanup below).
      setStreamConnected(true)
    }

    const onVisibility = () => {
      if (document.visibilityState === 'hidden') {
        // Don't tear the stream down for a quick tab-switch; release it only after sustained idle.
        if (es && !graceTimer) {
          graceTimer = setTimeout(() => {
            graceTimer = null
            pause()
          }, HIDDEN_GRACE_MS)
        }
      } else {
        if (graceTimer) {
          // Came back before the grace period elapsed — the stream never closed, carry on.
          clearTimeout(graceTimer)
          graceTimer = null
        }
        if (!es) {
          // Stream was released while idle: reopen and resync (a fresh EventSource won't replay
          // the events emitted while we were paused).
          open()
          invalidateMatching('')
        }
      }
    }

    if (document.visibilityState !== 'hidden') open()
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      document.removeEventListener('visibilitychange', onVisibility)
      if (graceTimer) clearTimeout(graceTimer)
      es?.close()
      es = null
      // Unmounting isn't a real outage — don't leave the health signal stuck "down".
      setStreamConnected(true)
    }
  }, [])
}
