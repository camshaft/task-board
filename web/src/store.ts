// A reference-counted resource store. Each resource is identified by a string key and a
// fetcher; the store keeps at most one cache entry per key, shared by every component that
// asks for it. Components subscribe via useResource (below) — mounting bumps the key's
// refcount and registers a listener, unmounting drops it. When a key's data changes, every
// subscribed component re-renders; when its last subscriber unmounts, the entry is evicted
// after a short grace period. Invalidation (invalidate / invalidateMatching) is the single
// choke point that pushes fresh data — after a mutation, or later from an SSE event — so
// components never contain refetch/branching logic: they just declare the data they use.

import { useCallback, useSyncExternalStore } from 'react'

// Immutable view handed to components. `data` is the last successful value (kept during a
// refetch so the UI doesn't flash), `loading` is true while a fetch is in flight.
export interface ResourceState<T> {
  data: T | undefined
  error: Error | undefined
  loading: boolean
}

interface Entry<T> {
  key: string
  fetch: () => Promise<T>
  state: ResourceState<T> // stable snapshot; replaced (new object) only when it changes
  listeners: Set<() => void>
  refCount: number
  inFlight: Promise<void> | undefined
  evictTimer: ReturnType<typeof setTimeout> | undefined
  retryAttempt: number // consecutive failures, for backoff; reset to 0 on success
  retryTimer: ReturnType<typeof setTimeout> | undefined // pending auto-retry after a failure
}

// How long an entry lingers after its last subscriber leaves, so navigating away and back
// (or a StrictMode unmount/remount) reuses the cached data instead of refetching.
const EVICT_GRACE_MS = 30_000

// Auto-retry a failed fetch (with exponential backoff, capped) while it still has subscribers.
// During a backend deploy the server briefly 502s; rather than leaving a panel stuck on empty
// (first load) or stale (later) until the next mutation/SSE event, the store heals itself once
// the backend is back. Only resource fetchers run through here, and every resource fetcher is an
// idempotent GET — the mutation wrappers call api.* directly and are NEVER auto-retried, so a
// non-idempotent write can't be silently replayed (task 549).
const RETRY_BASE_MS = 1_000
const RETRY_MAX_MS = 30_000

const entries = new Map<string, Entry<unknown>>()

function ensureEntry<T>(key: string, fetch: () => Promise<T>): Entry<T> {
  let entry = entries.get(key) as Entry<T> | undefined
  if (!entry) {
    entry = {
      key,
      fetch,
      state: { data: undefined, error: undefined, loading: true },
      listeners: new Set(),
      refCount: 0,
      inFlight: undefined,
      evictTimer: undefined,
      retryAttempt: 0,
      retryTimer: undefined,
    }
    entries.set(key, entry as Entry<unknown>)
  } else {
    // Keep the latest fetcher (closures may capture fresh values across renders).
    entry.fetch = fetch
  }
  return entry
}

function setState<T>(entry: Entry<T>, patch: Partial<ResourceState<T>>) {
  entry.state = { ...entry.state, ...patch }
  for (const l of entry.listeners) l()
  updateHealth()
}

// Fetch (or refetch) an entry. Coalesces concurrent callers onto one request and keeps the
// previous data visible while the new one loads (stale-while-revalidate). On failure the last
// good data is kept (never cleared) and an auto-retry is scheduled with backoff, so a panel
// degrades to "stale + reconnecting" rather than blanking, and heals on its own (task 549).
function load<T>(entry: Entry<T>): Promise<void> {
  if (entry.inFlight) return entry.inFlight
  // A fresh load supersedes any pending retry (e.g. an invalidate raced the backoff timer).
  clearTimeout(entry.retryTimer)
  entry.retryTimer = undefined
  setState(entry, { loading: true, error: undefined })
  entry.inFlight = entry
    .fetch()
    .then((data) => {
      entry.retryAttempt = 0
      setState(entry, { data, loading: false, error: undefined })
    })
    .catch((err) => {
      setState(entry, { error: err as Error, loading: false }) // keep entry.state.data
      scheduleRetry(entry)
    })
    .finally(() => {
      entry.inFlight = undefined
    })
  return entry.inFlight
}

// After a failed fetch, retry with exponential backoff (jittered, capped) while the entry still
// has subscribers. An unwatched entry heals on its next mount instead (see subscribe), so we
// don't poll a resource nobody is looking at.
function scheduleRetry<T>(entry: Entry<T>) {
  clearTimeout(entry.retryTimer)
  entry.retryTimer = undefined
  if (entry.refCount <= 0) return
  const delay = Math.min(RETRY_MAX_MS, RETRY_BASE_MS * 2 ** entry.retryAttempt)
  entry.retryAttempt++
  // 50-100% jitter so many panels failing at once don't retry in lockstep (thundering herd).
  const jittered = delay * (0.5 + Math.random() * 0.5)
  entry.retryTimer = setTimeout(() => {
    entry.retryTimer = undefined
    if (entry.refCount > 0) void load(entry)
  }, jittered)
}

function scheduleEvict(entry: Entry<unknown>) {
  clearTimeout(entry.evictTimer)
  entry.evictTimer = setTimeout(() => {
    if (entry.refCount === 0) entries.delete(entry.key)
  }, EVICT_GRACE_MS)
}

// Add a subscriber to a key, creating and fetching the entry on first use. Returns an
// unsubscribe fn. Used internally by useResource via useSyncExternalStore.
function subscribe<T>(key: string, fetch: () => Promise<T>, listener: () => void): () => void {
  const entry = ensureEntry(key, fetch)
  entry.listeners.add(listener)
  entry.refCount++
  clearTimeout(entry.evictTimer)
  entry.evictTimer = undefined
  // Kick off a fetch on (re)subscribe when there's nothing fresh to show: no data yet, or the
  // last fetch errored (heal a stale-errored panel when the user navigates back to it). Skip if
  // a request or a backoff retry is already pending.
  if (!entry.inFlight && !entry.retryTimer && (entry.state.data === undefined || entry.state.error))
    void load(entry)
  return () => {
    entry.listeners.delete(listener)
    entry.refCount--
    if (entry.refCount <= 0) {
      // No one is watching: stop the backoff loop (it resumes on next mount) and start eviction.
      clearTimeout(entry.retryTimer)
      entry.retryTimer = undefined
      scheduleEvict(entry as Entry<unknown>)
      updateHealth()
    }
  }
}

/**
 * Invalidate one resource key. If it currently has subscribers, refetch it in place (they
 * re-render with fresh data); if not, drop its cached data so the next mount refetches.
 * This is the hook mutations and (later) SSE events call to push updates.
 */
export function invalidate(key: string) {
  const entry = entries.get(key)
  if (!entry) return
  if (entry.refCount > 0) void load(entry)
  else entries.delete(key)
}

/** Invalidate every key starting with `prefix` (e.g. "task:" or "tasks:"). */
export function invalidateMatching(prefix: string) {
  for (const key of [...entries.keys()]) {
    if (key.startsWith(prefix)) invalidate(key)
  }
}

/**
 * Subscribe a component to a resource. Pass a stable key and a fetcher; the component
 * re-renders whenever that resource's data changes, and shares one fetch/cache entry with
 * every other component using the same key.
 */
export function useResource<T>(key: string, fetch: () => Promise<T>): ResourceState<T> {
  const sub = useCallback(
    (listener: () => void) => subscribe(key, fetch, listener),
    // Re-subscribe only when the key changes; the fetcher is refreshed inside ensureEntry.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  )
  const getSnapshot = useCallback(() => ensureEntry(key, fetch).state, [key, fetch])
  return useSyncExternalStore(sub, getSnapshot) as ResourceState<T>
}

// ---------------------------------------------------------------------------
// Connection health
//
// So the UI can show a "reconnecting" hint (rather than a silently blank/stale screen) while the
// backend is unreachable — e.g. the 502 window during a backend deploy. `retrying` is true while
// any subscribed resource is in an errored/backoff state; `streamDown` reflects the live (SSE)
// connection, reported by the live-updates hook. (task 549)

export interface ConnectionHealth {
  retrying: boolean
  streamDown: boolean
}

let streamConnected = true
const healthListeners = new Set<() => void>()
let healthSnapshot: ConnectionHealth = { retrying: false, streamDown: false }

function computeHealth(): ConnectionHealth {
  let retrying = false
  for (const entry of entries.values()) {
    if (entry.refCount > 0 && entry.state.error !== undefined) {
      retrying = true
      break
    }
  }
  return { retrying, streamDown: !streamConnected }
}

// Recompute and, only if it changed, publish a NEW snapshot object (useSyncExternalStore requires
// a stable reference between renders, so we must not allocate when nothing changed).
function updateHealth() {
  const next = computeHealth()
  if (next.retrying !== healthSnapshot.retrying || next.streamDown !== healthSnapshot.streamDown) {
    healthSnapshot = next
    for (const l of healthListeners) l()
  }
}

// Reported by useLiveUpdates on the SSE connection opening / erroring.
export function setStreamConnected(connected: boolean) {
  if (streamConnected === connected) return
  streamConnected = connected
  updateHealth()
}

export function useConnectionHealth(): ConnectionHealth {
  const sub = useCallback((listener: () => void) => {
    healthListeners.add(listener)
    return () => healthListeners.delete(listener)
  }, [])
  const getSnapshot = useCallback(() => healthSnapshot, [])
  return useSyncExternalStore(sub, getSnapshot)
}
