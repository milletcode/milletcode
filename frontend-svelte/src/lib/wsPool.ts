/**
 * WebSocket Connection Pool
 *
 * Manages per-project WebSocket connections.
 * Key format: "org/project/agent-type" (e.g. "smartcode/git-kanban/claude-code")
 *
 * Architecture: one WS connection per project+agent-type, with multiple
 * session-names as subscribers within that connection.
 *
 * The pool maintains a session→projectKey mapping so callers can send
 * messages by session_id without knowing the projectKey.
 */

import { apiFetch } from './api'

type WSStatus = 'connecting' | 'connected' | 'disconnected'

type StatusListener = (status: WSStatus) => void

const wsPool = new Map<string, WebSocket>()
const subscribedSets = new Map<string, Set<string>>()
const statusMap = new Map<string, WSStatus>()
const statusListeners = new Map<string, Set<StatusListener>>()
const expectingReplay = new Map<string, Set<string>>()

// Consecutive reconnect attempts per projectKey — drives the backoff below.
// Reset to zero whenever a connection opens successfully.
const reconnectAttempts = new Map<string, number>()

// Session → projectKey mapping for routing messages
const sessionToProject = new Map<string, string>()

// Global message handler (set by App.tsx)
let onMessage: ((msg: any) => void) | null = null

export function setWSMessageHandler(handler: (msg: any) => void) {
  onMessage = handler
}

function setStatus(projectKey: string, status: WSStatus) {
  statusMap.set(projectKey, status)
  const listeners = statusListeners.get(projectKey)
  if (listeners) {
    listeners.forEach((fn) => fn(status))
  }
}

export function getWSStatus(projectKey?: string): WSStatus {
  if (projectKey) {
    return statusMap.get(projectKey) || 'disconnected'
  }
  // Aggregate: connected if ANY connection is connected
  for (const status of statusMap.values()) {
    if (status === 'connected') return 'connected'
  }
  for (const status of statusMap.values()) {
    if (status === 'connecting') return 'connecting'
  }
  return 'disconnected'
}

export function subscribeWSStatus(projectKey: string | undefined, fn: StatusListener) {
  const key = projectKey || '__global__'
  if (!statusListeners.has(key)) {
    statusListeners.set(key, new Set())
  }
  statusListeners.get(key)!.add(fn)
  // Immediately fire with current status
  fn(getWSStatus(projectKey))
  return () => {
    statusListeners.get(key)?.delete(fn)
  }
}

/**
 * Initialize a WebSocket connection for a specific project+agent-type.
 * The session WS cannot use Basic auth (browser WS API cannot set headers),
 * so a one-time ticket is minted over an authenticated REST call first.
 */
export function initWebSocket(projectKey: string) {
  const existing = wsPool.get(projectKey)
  if (existing && (existing.readyState === WebSocket.CONNECTING || existing.readyState === WebSocket.OPEN)) {
    return
  }

  // Clean up old connection if any
  if (existing) {
    existing.onclose = null
    existing.onerror = null
    existing.onopen = null
    existing.onmessage = null
    existing.close()
  }

  setStatus(projectKey, 'connecting')

  apiFetch('/api/ws-ticket', { method: 'POST' })
    .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`ws-ticket ${res.status}`))))
    .then(({ ticket }) => {
      connectWithTicket(projectKey, ticket)
    })
    .catch(() => {
      setStatus(projectKey, 'disconnected')
    })
}

function connectWithTicket(projectKey: string, ticket: string) {
  // A newer connection attempt may already be in flight for this key.
  const current = wsPool.get(projectKey)
  if (current && (current.readyState === WebSocket.CONNECTING || current.readyState === WebSocket.OPEN)) {
    return
  }

  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const url = `${protocol}//${window.location.host}/ws/${projectKey}?ticket=${encodeURIComponent(ticket)}`

  const ws = new WebSocket(url)
  wsPool.set(projectKey, ws)
  if (!subscribedSets.has(projectKey)) {
    subscribedSets.set(projectKey, new Set())
  }
  if (!expectingReplay.has(projectKey)) {
    expectingReplay.set(projectKey, new Set())
  }

  ws.onopen = () => {
    reconnectAttempts.delete(projectKey)
    setStatus(projectKey, 'connected')
    // Re-subscribe sessions that were tracked before reconnect
    const subscribed = subscribedSets.get(projectKey) || new Set()
    const idsToResubscribe = [...subscribed]
    subscribed.clear()
    idsToResubscribe.forEach((sessionId) => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({
          type: 'subscribe_session',
          session_id: sessionId,
        }))
        subscribed.add(sessionId)
      }
    })
  }

  ws.onclose = () => {
    setStatus(projectKey, 'disconnected')
    wsPool.delete(projectKey)
    // Exponential backoff (3s → 6s → 12s → 15s cap): a flat 3s retry keeps
    // hammering /api/ws-ticket on bad mobile networks, which is exactly when
    // connections drop the most.
    const attempt = reconnectAttempts.get(projectKey) ?? 0
    const delay = Math.min(3000 * 2 ** attempt, 15000)
    reconnectAttempts.set(projectKey, attempt + 1)
    setTimeout(() => initWebSocket(projectKey), delay)
  }

  ws.onerror = () => {
    ws.close()
  }

  ws.onmessage = (event) => {
    try {
      const msg = JSON.parse(event.data)
      if (onMessage) onMessage(msg)
    } catch {
      // ignore parse errors
    }
  }
}

/**
 * Register a session→projectKey mapping.
 */
export function registerSessionProject(sessionId: string, projectKey: string) {
  sessionToProject.set(sessionId, projectKey)
}

/**
 * Unregister a session.
 */
export function unregisterSession(sessionId: string) {
  sessionToProject.delete(sessionId)
}

/**
 * Look up the projectKey for a session.
 */
export function getProjectForSession(sessionId: string): string | null {
  return sessionToProject.get(sessionId) || null
}

/**
 * Check if a session is currently subscribed (receiving WS messages).
 */
export function isSessionSubscribed(sessionId: string): boolean {
  const key = sessionToProject.get(sessionId)
  if (!key) return false
  return subscribedSets.get(key)?.has(sessionId) ?? false
}

/**
 * Subscribe to a session on its project connection.
 */
export function subscribeSession(sessionId: string, projectKey: string) {
  registerSessionProject(sessionId, projectKey)

  const subscribed = subscribedSets.get(projectKey) || new Set()
  subscribedSets.set(projectKey, subscribed)

  const replaySet = expectingReplay.get(projectKey) || new Set()
  expectingReplay.set(projectKey, replaySet)

  if (!subscribed.has(sessionId)) {
    replaySet.add(sessionId)
  }

  subscribed.add(sessionId)

  // If WS is already open, send subscribe immediately.
  // Otherwise, the session is registered and onopen will send it.
  const ws = wsPool.get(projectKey)
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({
      type: 'subscribe_session',
      session_id: sessionId,
    }))
  }
}

/**
 * Unsubscribe from a session.
 */
export function unsubscribeSession(sessionId: string, projectKey?: string) {
  const key = projectKey || sessionToProject.get(sessionId)
  if (!key) return

  const ws = wsPool.get(key)
  if (ws && ws.readyState === WebSocket.OPEN) {
    subscribedSets.get(key)?.delete(sessionId)
    expectingReplay.get(key)?.delete(sessionId)
    ws.send(JSON.stringify({
      type: 'unsubscribe_session',
      session_id: sessionId,
    }))
  }
  sessionToProject.delete(sessionId)
}

/**
 * Send a message for a specific session. Routes to the correct connection.
 */
export function sendWSMessage(msg: any, projectKey?: string) {
  // If projectKey is provided, use it directly
  if (projectKey) {
    const ws = wsPool.get(projectKey)
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg))
    }
    return
  }
  // Otherwise, try to route by session_id
  const sessionId = msg.session_id
  if (sessionId) {
    const key = sessionToProject.get(sessionId)
    if (key) {
      const ws = wsPool.get(key)
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify(msg))
      }
      return
    }
  }
  // Fallback: send on any connected WS
  for (const [, ws] of wsPool.entries()) {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg))
      return
    }
  }
}

/**
 * Close a project's WebSocket connection.
 */
export function closeWebSocket(projectKey: string) {
  const ws = wsPool.get(projectKey)
  if (ws) {
    ws.onclose = null // prevent auto-reconnect
    ws.close()
    wsPool.delete(projectKey)
    subscribedSets.delete(projectKey)
    expectingReplay.delete(projectKey)
    statusMap.delete(projectKey)
    setStatus(projectKey, 'disconnected')
  }
}

/**
 * Check if a session is expecting replay (first output after subscribe).
 * Returns true on first call (consuming the flag), false on subsequent calls.
 */
export function consumeReplay(sessionId: string, projectKey?: string): boolean {
  const key = projectKey || sessionToProject.get(sessionId)
  if (!key) return false
  const replaySet = expectingReplay.get(key)
  if (replaySet && replaySet.has(sessionId)) {
    replaySet.delete(sessionId)
    return true
  }
  return false
}

/**
 * Clear replay flag for a session (e.g. on restart).
 */
export function clearReplay(sessionId: string, projectKey?: string) {
  const key = projectKey || sessionToProject.get(sessionId)
  if (key) {
    expectingReplay.get(key)?.delete(sessionId)
  }
}

/**
 * Get all active project keys.
 */
export function getActiveProjectKeys(): string[] {
  return [...wsPool.keys()]
}

/**
 * Get the count of active connections.
 */
export function getConnectionCount(): number {
  return wsPool.size
}

/**
 * Get the count of subscribed sessions across all connections.
 */
export function getSubscribedSessionCount(): number {
  let count = 0
  for (const set of subscribedSets.values()) {
    count += set.size
  }
  return count
}

/**
 * Get connection status map.
 */
export function getWSStatusMap(): Map<string, WSStatus> {
  return new Map(statusMap)
}
