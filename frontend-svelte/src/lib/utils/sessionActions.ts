import { addNotification } from '../stores/notification'
import { subscribeSession, initWebSocket, getWSStatus } from '../wsPool'

/**
 * 统一的 session 恢复/启动流程
 * 1. POST /api/sessions/{id}/restore (等待返回)
 * 2. 初始化 WS + 订阅
 * 3. 导航到详情页
 */
export async function resumeSession(params: {
  sessionId: string
  sessionKey: string | null
  navigate: (path: string) => void
  setActiveSession: (id: string) => void
}): Promise<void> {
  const { sessionId, sessionKey, navigate, setActiveSession } = params

  try {
    // Step 1: 调用 REST API 启动恢复
    const res = await fetch(`/api/sessions/${sessionId}/restore`, {
      method: 'POST',
    })
    if (!res.ok) {
      const err = await res.json().catch(() => ({ error: 'Unknown error' }))
      throw new Error(err.error || `Restore failed (${res.status})`)
    }

    // Step 2: 初始化 WS + 订阅
    if (sessionKey) {
      const parts = sessionKey.split('/')
      if (parts.length >= 3) {
        const projectKey = parts.slice(0, 3).join('/')
        initWebSocket(projectKey)
        await waitForWSConnection(projectKey)
        subscribeSession(sessionId, projectKey)
      }
    }

    // Step 3: 导航
    setActiveSession(sessionId)
    if (sessionKey) {
      const parts = sessionKey.split('/')
      if (parts.length === 4) {
        window.location.href = `/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}`
        return
      }
    }
    navigate(`/sessions/${sessionId}`)
  } catch (e) {
    addNotification({
      id: `resume-${sessionId}-${Date.now()}`,
      level: 'error',
      message: e instanceof Error ? e.message : 'Failed to resume session',
      ts: Date.now(),
    })
    throw e
  }
}

async function waitForWSConnection(projectKey: string, timeout = 5000): Promise<void> {
  const start = Date.now()
  while (getWSStatus(projectKey) !== 'connected') {
    if (Date.now() - start > timeout) {
      throw new Error('WebSocket connection timeout')
    }
    await new Promise(r => setTimeout(r, 100))
  }
}
