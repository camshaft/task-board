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
}

// How long an entry lingers after its last subscriber leaves, so navigating away and back
// (or a StrictMode unmount/remount) reuses the cached data instead of refetching.
const EVICT_GRACE_MS = 30_000

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
}

// Fetch (or refetch) an entry. Coalesces concurrent callers onto one request and keeps the
// previous data visible while the new one loads (stale-while-revalidate).
function load<T>(entry: Entry<T>): Promise<void> {
  if (entry.inFlight) return entry.inFlight
  setState(entry, { loading: true, error: undefined })
  entry.inFlight = entry
    .fetch()
    .then((data) => setState(entry, { data, loading: false, error: undefined }))
    .catch((err) => setState(entry, { error: err as Error, loading: false }))
    .finally(() => {
      entry.inFlight = undefined
    })
  return entry.inFlight
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
  // First subscriber (or a re-subscribe after data was evicted): kick off a fetch.
  if (entry.state.data === undefined && !entry.inFlight) void load(entry)
  return () => {
    entry.listeners.delete(listener)
    entry.refCount--
    if (entry.refCount <= 0) scheduleEvict(entry as Entry<unknown>)
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
