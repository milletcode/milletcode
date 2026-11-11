import type { HistoryMessage } from '../stores/session'

export type ViewMode = 'compact' | 'outline' | 'detailed'

export interface DisplayMessage {
  msg: HistoryMessage
  isRoot: boolean
  depth: number
  hasChildren: boolean
}

/**
 * Filter and process messages for display.
 * - compact: outline mode + hide thinking blocks
 * - outline: hide external assistant messages, collapse tool-only messages
 * - detailed: show everything
 */
export function filterAndBuildMessages(raw: HistoryMessage[], viewMode: ViewMode = 'detailed'): DisplayMessage[] {
  // Filter out invisible messages (api_helper, etc.)
  let visible = raw.filter((msg) => {
    if (msg.userType === 'api_helper') {
      return false
    }
    // Hidden control message: the idle-notify trigger is delivered to the
    // agent (so the skill can act) but never shown in the transcript.
    if (msg.role === 'user' && typeof msg.content === 'string' && msg.content.trimStart().startsWith('/millet-msg')) {
      return false
    }
    return true
  })

  // Outline and compact modes: filter out external assistant messages
  if (viewMode === 'outline' || viewMode === 'compact') {
    visible = visible.filter((msg) => {
      // Drop external assistant messages
      if (msg.role === 'assistant' && msg.userType === 'external') {
        return false
      }
      // Drop tool messages — they render nothing in these modes
      // Keep AskUserQuestion since it has its own compact rendering
      if ((msg.toolName || msg.toolUseId) && msg.toolName !== 'AskUserQuestion') {
        return false
      }
      return true
    })
  }

  // Compact mode: additionally filter out pure-thinking assistant messages
  if (viewMode === 'compact') {
    visible = visible.filter((msg) => {
      if (msg.role !== 'assistant') return true
      // Keep AskUserQuestion messages - they have their own compact rendering
      if (msg.toolName === 'AskUserQuestion') return true
      const content = msg.content || ''
      const hasThinking = /\[thinking\]/.test(content)
      const textOnly = content
        .replace(/\[thinking\][\s\S]*?\[\/thinking\]/g, '')
        .replace(/\[tool_use[^\]]*\][\s\S]*?\[\/tool_use\]/g, '')
        .replace(/\[tool_result[^\]]*\][\s\S]*?\[\/tool_result\]/g, '')
        .replace(/\[ask_user_question[^\]]*\][\s\S]*?\[\/ask_user_question\]/g, '')
        .trim()
      // Keep if there's real text content (not just thinking/tools)
      return !hasThinking || textOnly.length > 0
    })
  }

  // Outline mode: filter out assistant messages that only contain tool blocks (no real text and no thinking)
  if (viewMode === 'outline') {
    visible = visible.filter((msg) => {
      if (msg.role !== 'assistant') return true
      const content = msg.content || ''
      // Keep messages that contain ask_user_question blocks
      if (/\[ask_user_question[^\]]*\]/.test(content)) return true
      // Keep AskUserQuestion tool messages
      if (msg.toolName === 'AskUserQuestion') return true
      // Keep messages that contain thinking blocks (show thinking in outline mode)
      if (/\[thinking\]/.test(content)) return true
      const stripped = content
        .replace(/\[thinking\][\s\S]*?\[\/thinking\]/g, '')
        .replace(/\[tool_use[^\]]*\][\s\S]*?\[\/tool_use\]/g, '')
        .replace(/\[tool_result[^\]]*\][\s\S]*?\[\/tool_result\]/g, '')
        .replace(/\[ask_user_question[^\]]*\][\s\S]*?\[\/ask_user_question\]/g, '')
        .trim()
      return stripped.length > 0
    })
  }

  // Direct mapping to DisplayMessage (no thread tree, no tool merging)
  return visible.map((msg) => ({
    msg,
    isRoot: true,
    depth: 0,
    hasChildren: false,
  }))
}
