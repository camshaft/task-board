// Live updates: open a Server-Sent Events connection to the board's activity feed and pipe
// every event through applyStreamEvent, which invalidates the affected resources. Mount this
// once (in Layout) and the whole UI becomes live — any change made by another client or an
// MCP agent refreshes the relevant panels automatically. EventSource handles reconnection and
// replays missed events via Last-Event-ID (the server keys events by their log seq).

import { useEffect } from 'react'
import { api } from './api'
import { applyStreamEvent, type StreamEvent } from './resources'

export function useLiveUpdates() {
  useEffect(() => {
    const es = new EventSource(api.streamUrl())
    es.onmessage = (e) => {
      try {
        applyStreamEvent(JSON.parse(e.data) as StreamEvent)
      } catch {
        // A keep-alive comment or malformed frame — ignore; the next real event will refresh.
      }
    }
    // On error EventSource auto-reconnects (resuming from the last event id); nothing to do
    // but let it. A closed connection just means updates pause until it's back.
    return () => es.close()
  }, [])
}
