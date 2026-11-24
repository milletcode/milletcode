import { writable, derived } from 'svelte/store'
import { apiFetch } from '../api'

export interface ApprovalRequest {
  session_id: string
  prompt: string
  options: string[]
  request_id?: string
  tool_name?: string
  session_key?: string | null
  project_name?: string | null
  agent_name?: string | null
  receivedAt?: number
}

export interface UserQuestionItem {
  question: string
  header: string
  options: Array<{ label: string; description: string }>
}

export interface UserQuestionRequest {
  session_id: string
  tool_use_id: string
  request_id: string
  questions: UserQuestionItem[]
  project_name?: string | null
  agent_name?: string | null
  receivedAt?: number
}

// Writable stores
export const approvals = writable<Map<string, ApprovalRequest>>(new Map())
export const userQuestions = writable<Map<string, UserQuestionRequest>>(new Map())

// Derived stores
export const totalPendingCount = derived(
  [approvals, userQuestions],
  ([$approvals, $questions]) => $approvals.size + $questions.size
)

export const hasPendingControl = derived(
  approvals,
  ($approvals) => (sessionId: string) => $approvals.has(sessionId)
)

export const hasPendingQuestion = derived(
  userQuestions,
  ($questions) => (sessionId: string) => $questions.has(sessionId)
)

// Actions
export function addApproval(req: ApprovalRequest) {
  approvals.update((map) => {
    const newMap = new Map(map)
    newMap.set(req.session_id, { ...req, receivedAt: req.receivedAt ?? Date.now() })
    return newMap
  })
}

export function removeApproval(sessionId: string) {
  approvals.update((map) => {
    const newMap = new Map(map)
    newMap.delete(sessionId)
    return newMap
  })
}

export function addUserQuestion(req: UserQuestionRequest) {
  userQuestions.update((map) => {
    const newMap = new Map(map)
    newMap.set(req.session_id, { ...req, receivedAt: req.receivedAt ?? Date.now() })
    return newMap
  })
}

export function removeUserQuestion(sessionId: string) {
  userQuestions.update((map) => {
    const newMap = new Map(map)
    newMap.delete(sessionId)
    return newMap
  })
}

/** Bootstrap pending decisions from REST (page load / WS reconnect / tab focus).
 *  Reconciles by replacing with the server snapshot, but keeps entries that
 *  arrived via WS after the fetch started — a stale snapshot must not wipe a
 *  newer event. */
export async function fetchPending() {
  const startedAt = Date.now()
  try {
    const res = await apiFetch('/api/pending')
    if (!res.ok) return
    const data = await res.json()
    if (Array.isArray(data.approvals)) {
      approvals.update((map) => {
        const kept = [...map.values()].filter((a) => (a.receivedAt ?? 0) > startedAt)
        const newMap = new Map<string, ApprovalRequest>()
        for (const a of data.approvals) {
          if (a?.session_id) newMap.set(a.session_id, { ...a, receivedAt: a.receivedAt ?? Date.now() })
        }
        for (const a of kept) newMap.set(a.session_id, a)
        return newMap
      })
    }
    if (Array.isArray(data.questions)) {
      userQuestions.update((map) => {
        const kept = [...map.values()].filter((q) => (q.receivedAt ?? 0) > startedAt)
        const newMap = new Map<string, UserQuestionRequest>()
        for (const q of data.questions) {
          if (q?.session_id) newMap.set(q.session_id, { ...q, receivedAt: q.receivedAt ?? Date.now() })
        }
        for (const q of kept) newMap.set(q.session_id, q)
        return newMap
      })
    }
  } catch {
    // network hiccup — WS reconnect will retry
  }
}

/** Pending input for MessageInput. Keyed by sessionId. */
export const pendingInput = writable<Map<string, string>>(new Map())

/** Clear pending input for a session */
export function clearPendingInput(sessionId: string) {
  pendingInput.update((map) => {
    const newMap = new Map(map)
    newMap.delete(sessionId)
    return newMap
  })
}
