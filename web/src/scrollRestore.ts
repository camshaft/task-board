// Scroll-position restoration for back/forward navigation (task 696). The app scrolls inside each
// page's own container (not the window), so the browser's native history scroll restoration does
// not apply. This hook saves a container's scroll position per history entry and restores it when
// you navigate back or forward, so Back returns you to where you were instead of the top.

import { useLayoutEffect, useRef } from 'react'
import { useLocation, useNavigationType } from 'react-router-dom'

// Per history-entry scroll offsets, keyed by location.key. In-memory for the SPA session: a
// back/forward (POP) returns to the saved offset; a fresh navigation (PUSH/REPLACE) starts at top.
const positions = new Map<string, number>()

// Attach the returned ref to a page's scroll container (the overflow-y-auto element). On POP the
// saved offset is restored (retried across a few frames, since list data from the resource store
// may hydrate just after mount and only then make the offset reachable); on a new navigation the
// container resets to the top.
export function useScrollRestoration<T extends HTMLElement = HTMLDivElement>() {
  const ref = useRef<T>(null)
  const { key } = useLocation()
  const navType = useNavigationType()

  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return

    // Continuously record this entry's offset while the page is mounted.
    const onScroll = () => positions.set(key, el.scrollTop)
    el.addEventListener('scroll', onScroll, { passive: true })

    let raf = 0
    if (navType === 'POP' && positions.has(key)) {
      const target = positions.get(key) ?? 0
      let tries = 0
      const settle = () => {
        const node = ref.current
        if (!node) return
        node.scrollTop = target
        // Stop once the offset sticks (content is tall enough) or after ~30 frames (~0.5s), so a
        // still-loading list that never reaches `target` doesn't spin forever.
        if (Math.abs(node.scrollTop - target) > 1 && tries++ < 30) {
          raf = requestAnimationFrame(settle)
        }
      }
      raf = requestAnimationFrame(settle)
    } else {
      el.scrollTop = 0
    }

    return () => {
      // Capture a final offset only while still attached (a detached element reports scrollTop 0);
      // during a route change the scroll listener has already recorded the live offset for `key`.
      if (el.isConnected) positions.set(key, el.scrollTop)
      el.removeEventListener('scroll', onScroll)
      if (raf) cancelAnimationFrame(raf)
    }
  }, [key, navType])

  return ref
}
