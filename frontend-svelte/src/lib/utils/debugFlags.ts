/**
 * Debug flag management — persisted to localStorage.
 */

export interface DebugFlags {
  showMessageStats: boolean
  showRawMessageDump: boolean
}

const STORAGE_KEY = 'milletcode-debug-flags'

const DEFAULT_FLAGS: DebugFlags = {
  showMessageStats: false,
  showRawMessageDump: false,
}

export function getDebugFlags(): DebugFlags {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (raw) {
      return { ...DEFAULT_FLAGS, ...JSON.parse(raw) }
    }
  } catch {}
  return { ...DEFAULT_FLAGS }
}

export function setDebugFlags(flags: DebugFlags) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(flags))
  } catch {}
}
