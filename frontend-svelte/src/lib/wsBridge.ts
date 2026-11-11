import {
  setWSMessageHandler,
  subscribeSession,
  unsubscribeSession,
  initWebSocket,
  getWSStatus,
} from './wsPool'
import {
  appendSessionMessage,
  appendWSMessage,
  updateSessionStatus,
  setTurnComplete,
  type HistoryMessage,
} from './stores/session'
import {
  addApproval,
  removeApproval,
  addUserQuestion,
  removeUserQuestion,
} from './stores/approval'
import { addNotification } from './stores/notification'

/**
 * Initialize the WebSocket bridge.
 * This sets up the global message handler that routes WS events to Svelte stores.
 */
export function initBridge() {
  setWSMessageHandler(handleWSMessage)
}

function handleWSMessage(msg: any) {
  const sessionId = msg.session_id

  switch (msg.type) {
    case 'output': {
      const raw = msg.raw || {}

      // The CLI's end-of-turn marker (success or error_*). Never a chat message.
      if (raw.type === 'result') {
        setTurnComplete(sessionId, true)
        break
      }
      // A user-line echo means the CLI accepted input and a new turn is starting
      if (raw.type === 'user') {
        setTurnComplete(sessionId, false)
      }

      // Extract structured content blocks from raw message
      const contentBlocks = raw.message?.content || []
      const timestamp = msg.timestamp || Date.now()
      const uuid = raw.uuid
      const parentUuid = raw.parentUuid
      const userType = raw.userType
      // Why Claude stopped on this assistant line — absent while the turn is still
      // running, "tool_use" when a tool is about to be called, and a terminal reason
      // such as "end_turn" once the turn is over. Carried through so the thread can
      // tell a finished turn from an idle-looking one.
      const stopReason = raw.message?.stop_reason ?? null

      // If no structured blocks, fall back to text-only message
      if (contentBlocks.length === 0) {
        const historyMsg: HistoryMessage = {
          role: 'assistant',
          type: 'assistant',
          content: msg.text || '',
          timestamp,
          uuid,
          parentUuid,
          userType,
          id: msg.id || uuid,
          done: msg.done ?? false,
          stopReason,
          isError: msg.is_error ?? false,
        }
        appendSessionMessage(sessionId, historyMsg)
        appendWSMessage(sessionId, historyMsg)
        break
      }

      // Process each content block separately
      contentBlocks.forEach((block: any, idx: number) => {
        const blockId = `${uuid}-${idx}`

        if (block.type === 'text') {
          // Text block
          const historyMsg: HistoryMessage = {
            role: 'assistant',
            type: 'assistant',
            content: block.text || '',
            timestamp,
            uuid,
            parentUuid,
            userType,
            id: blockId,
            done: msg.done ?? false,
            stopReason,
          }
          appendSessionMessage(sessionId, historyMsg)
          appendWSMessage(sessionId, historyMsg)
        } else if (block.type === 'tool_use') {
          // Tool use block
          const historyMsg: HistoryMessage = {
            role: 'assistant',
            type: 'assistant',
            content: JSON.stringify(block.input || {}, null, 2),
            timestamp,
            uuid,
            parentUuid,
            userType,
            id: blockId,
            done: msg.done ?? false,
            stopReason,
            toolName: block.name,
            toolUseId: block.id,
          }
          appendSessionMessage(sessionId, historyMsg)
          appendWSMessage(sessionId, historyMsg)
        } else if (block.type === 'tool_result') {
          // Tool result block
          const historyMsg: HistoryMessage = {
            role: 'user',
            type: 'user',
            content: block.content || '',
            timestamp,
            uuid,
            parentUuid,
            userType,
            id: blockId,
            done: true,
            isError: block.is_error ?? false,
            toolUseId: block.tool_use_id,
          }
          appendSessionMessage(sessionId, historyMsg)
          appendWSMessage(sessionId, historyMsg)
        } else if (block.type === 'thinking') {
          // Thinking block - treat as text for now
          const historyMsg: HistoryMessage = {
            role: 'assistant',
            type: 'assistant',
            content: `[thinking]\n${block.thinking || ''}\n[/thinking]`,
            timestamp,
            uuid,
            parentUuid,
            userType,
            id: blockId,
            done: msg.done ?? false,
            stopReason,
          }
          appendSessionMessage(sessionId, historyMsg)
          appendWSMessage(sessionId, historyMsg)
        }
      })
      break
    }

    case 'status':
    case 'session_status': {
      // Session status change (backend sends "session_status" via snake_case serde)
      updateSessionStatus(sessionId, msg.status)
      break
    }

    case 'session_complete': {
      // Session process exited - verify with API before updating status
      // This prevents stale messages from incorrectly marking running sessions as stopped
      // Also clear any pending control state — a dead process cannot be waiting on us
      removeApproval(sessionId)
      removeUserQuestion(sessionId)
      ;(async () => {
        // A completion frame right after a fresh send is usually the OLD process
        // dying while the backend restarts to serve the new message — ending the
        // turn here would kill the busy indicator mid-send. Only end it when the
        // API confirms the session is really down; keep it in flight otherwise.
        let confirmedDown = true
        try {
          const { apiFetch } = await import('$lib/api')
          const res = await apiFetch(`/api/sessions/${sessionId}`)
          if (res.ok) {
            const data = await res.json()
            const alive =
              data.status === 'running' ||
              data.status === 'starting' ||
              data.status === 'restarting' ||
              data.status === 'waiting_approval' ||
              data.status === 'waitingapproval'
            if (alive) {
              confirmedDown = false
            } else {
              updateSessionStatus(sessionId, msg.exit_code === 0 ? 'stopped' : 'error')
            }
          }
        } catch {
          // If API call fails, fall back to updating status
          updateSessionStatus(sessionId, msg.exit_code === 0 ? 'stopped' : 'error')
        }
        if (confirmedDown) setTurnComplete(sessionId, true)
      })()
      break
    }

    case 'approval_request': {
      // Permission approval via in-band control protocol
      addApproval({
        session_id: sessionId,
        prompt: msg.prompt,
        options: msg.options || [],
        request_id: msg.request_id,
        tool_name: msg.tool_name,
        session_key: msg.session_key,
        project_name: msg.project_name,
        agent_name: msg.agent_name,
      })
      break
    }

    case 'approval_resolved': {
      // Backend emits one approval_resolved for both approval and question paths
      removeApproval(sessionId)
      removeUserQuestion(sessionId)
      break
    }

    case 'user_question': {
      // AskUserQuestion via in-band control protocol
      addUserQuestion({
        session_id: sessionId,
        tool_use_id: msg.tool_use_id,
        request_id: msg.request_id,
        questions: msg.questions || [],
        project_name: msg.project_name,
        agent_name: msg.agent_name,
      })
      break
    }

    case 'error': {
      // Error message. Backend also flips in-memory status to error on this
      // event (crash / provider error) — mirror that so the live badge matches
      // what a fresh page load would read from the record.
      updateSessionStatus(sessionId, 'error')
      addNotification({
        id: `error-${sessionId}-${Date.now()}`,
        level: 'error',
        message: msg.message || 'Unknown error',
        ts: Date.now(),
      })
      break
    }

    case 'session_restarted': {
      // Stale request_ids must not block a resumed session's fresh ones
      removeApproval(sessionId)
      removeUserQuestion(sessionId)
      setTurnComplete(sessionId, false)
      break
    }

    case 'config_changed': {
      // smartcode hot-switch receipt — authoritative confirmation that the
      // new config is live; payload reflects current values (api_key never
      // comes back). Non-smartcode sessions never receive this.
      addNotification({
        id: `config-${sessionId}-${Date.now()}`,
        level: 'info',
        message: `model → ${msg.model || '?'}`,
        ts: Date.now(),
      })
      break
    }

    default:
      console.log('[WS Bridge] Unhandled message type:', msg.type, msg)
  }
}

// Re-export wsPool functions for convenience
export {
  subscribeSession,
  unsubscribeSession,
  initWebSocket,
  getWSStatus,
}
