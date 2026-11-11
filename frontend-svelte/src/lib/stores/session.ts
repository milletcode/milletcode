import { writable, derived } from 'svelte/store'

export interface HistoryMessage {
  role: 'user' | 'assistant'
  type: 'user' | 'assistant'
  content: string
  timestamp?: number
  userType?: string
  uuid?: string
  parentUuid?: string
  // --- New fields for better message handling ---
  id?: string          // Dedup key / Svelte {#each} key
  done?: boolean       // Streaming complete
  stopReason?: string | null  // Claude's reason for stopping this assistant message
  isError?: boolean    // Error flag
  toolName?: string    // Tool name (for tool_use)
  toolUseId?: string   // Links tool_use ↔ tool_result
}

export interface Session {
  /** Composite key: "org/project/agent-name/session-name" (primary identifier) */
  id: string
  /** Claude CLI UUID (used internally for --session-id/--resume, disk paths) */
  claude_session_id?: string
  /** @deprecated Use `id` — was separate composite key, now same as `id` */
  session_key?: string
  project_id: string | null
  agent_id: string
  status: string
  created_at: string
  prompt: string
  output: string
  create_time: number
  update_time: number
  /** Last crash/error reason (stderr tail) — persisted on the record, cleared when the session runs again */
  last_error?: string | null
}

// Writable stores
export const sessions = writable<Session[]>([])
export const activeSession = writable<string | null>(null)
export const viewMode = writable<'list' | 'card'>('list')
export const sessionMessages = writable<Map<string, HistoryMessage[]>>(new Map())
/** WS-only messages (debug panel). Separate from sessionMessages which is pre-loaded from HTTP history. */
export const wsSessionMessages = writable<Map<string, HistoryMessage[]>>(new Map())

// Derived stores
export const sortedSessions = derived(sessions, ($sessions) =>
  [...$sessions].sort((a, b) => b.update_time - a.update_time)
)

// Actions
export function setSessions(s: Session[]) {
  sessions.set(s)
}

export function addSession(session: Session) {
  sessions.update((current) => [...current, session])
}

export function setActiveSession(id: string | null) {
  activeSession.set(id)
}

export function setViewMode(mode: 'list' | 'card') {
  viewMode.set(mode)
}

export function updateSessionStatus(id: string, status: string) {
  sessions.update((current) =>
    current.map((s) => (s.id === id ? { ...s, status } : s))
  )
}

export function replaceOutput(id: string, text: string) {
  sessions.update((current) =>
    current.map((s) => (s.id === id ? { ...s, output: text } : s))
  )
}

export function appendOutput(id: string, text: string) {
  sessions.update((current) =>
    current.map((s) => (s.id === id ? { ...s, output: s.output + text } : s))
  )
}

export function setSessionMessages(id: string, messages: HistoryMessage[]) {
  sessionMessages.update((map) => {
    const newMap = new Map(map)
    newMap.set(id, messages)
    return newMap
  })
}

export function prependSessionMessages(id: string, messages: HistoryMessage[]) {
  sessionMessages.update((map) => {
    const newMap = new Map(map)
    const existing = newMap.get(id) || []
    newMap.set(id, [...messages, ...existing])
    return newMap
  })
}

export function appendSessionMessage(id: string, message: HistoryMessage) {
  sessionMessages.update((map) => {
    const newMap = new Map(map)
    const existing = newMap.get(id) || []

    // Dedup by id: if message has id, check if it already exists
    if (message.id) {
      const idx = existing.findIndex((m) => m.id === message.id)
      if (idx >= 0) {
        // Update in place (streaming content accumulation)
        const updated = [...existing]
        updated[idx] = { ...updated[idx], ...message }
        newMap.set(id, updated)
        return newMap
      }
    }

    // No id or not found: append to end
    newMap.set(id, [...existing, message])
    return newMap
  })
}

export function appendWSMessage(id: string, message: HistoryMessage) {
  wsSessionMessages.update((map) => {
    const newMap = new Map(map)
    const existing = newMap.get(id) || []
    newMap.set(id, [...existing, message])
    return newMap
  })
}

/**
 * Authoritative turn-end flag per session, set by the CLI's `result` line.
 * Thread-based "last assistant stop_reason" detection misses cases like
 * interrupted turns that end on a user message.
 */
export const turnComplete = writable<Map<string, boolean>>(new Map())

export function setTurnComplete(id: string, complete: boolean) {
  turnComplete.update((map) => {
    const newMap = new Map(map)
    newMap.set(id, complete)
    return newMap
  })
}

export async function fetchSessions() {
  try {
    const { apiFetch } = await import('$lib/api')
    const res = await apiFetch('/api/sessions')
    const data = await res.json()
    sessions.set(data)
  } catch {
    // ignore
  }
}
