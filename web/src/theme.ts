import { useEffect, useState } from 'react'

// Theme preference (task 520). 'system' follows the OS via prefers-color-scheme; 'light'/'dark'
// are explicit overrides. Persisted in localStorage; the resolved light/dark value is written to
// document.documentElement[data-theme], which index.css keys the --color-* palette off.
export type ThemePref = 'system' | 'light' | 'dark'

const KEY = 'tb-theme'

export function readThemePref(): ThemePref {
  const v = typeof localStorage !== 'undefined' ? localStorage.getItem(KEY) : null
  return v === 'light' || v === 'dark' || v === 'system' ? v : 'system'
}

export function resolveTheme(pref: ThemePref): 'light' | 'dark' {
  if (pref === 'light' || pref === 'dark') return pref
  return typeof window !== 'undefined' && window.matchMedia?.('(prefers-color-scheme: light)').matches
    ? 'light'
    : 'dark'
}

// Write the resolved theme to the document element so the CSS palette switches. Called pre-paint
// by the inline bootstrap in index.html and on every change by useTheme, so there's no flash.
export function applyTheme(pref: ThemePref) {
  if (typeof document !== 'undefined') document.documentElement.dataset.theme = resolveTheme(pref)
}

// The single theme source of truth: current preference + a setter that persists and re-applies.
// While on 'system' it follows OS changes live. Mounted once (in Layout) and shared via context so
// the settings control and the app agree; there's no second instance to desync.
export function useTheme(): { pref: ThemePref; setPref: (p: ThemePref) => void } {
  const [pref, setPrefState] = useState<ThemePref>(readThemePref)
  useEffect(() => {
    applyTheme(pref)
    if (pref !== 'system' || typeof window === 'undefined' || !window.matchMedia) return
    const mq = window.matchMedia('(prefers-color-scheme: light)')
    const onChange = () => applyTheme('system')
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [pref])
  const setPref = (p: ThemePref) => {
    localStorage.setItem(KEY, p)
    setPrefState(p)
  }
  return { pref, setPref }
}
