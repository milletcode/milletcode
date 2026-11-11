import type { HistoryMessage } from '../stores/session'

/**
 * Parse flattened history messages from HTTP API into structured HistoryMessage objects.
 *
 * Backend flattens structured content blocks into a single string with XML-like markers:
 * - text blocks → plain text
 * - thinking blocks → [thinking]...[/thinking]
 * - tool_use blocks → [tool_use id="..."]name {json}[/tool_use]
 * - tool_result blocks → [tool_result tool_use_id="..."]content[/tool_result]
 *
 * This parser reverses that process, creating separate HistoryMessage objects for each block.
 */
export function parseFlattenedHistory(messages: HistoryMessage[]): HistoryMessage[] {
  const result: HistoryMessage[] = []
  let fallbackCounter = 0

  for (const msg of messages) {
    const parsed = parseMessage(msg, () => `hist-${fallbackCounter++}`)
    result.push(...parsed)
  }

  return result
}

function parseMessage(msg: HistoryMessage, fallbackId: () => string): HistoryMessage[] {
  const content = msg.content
  const blocks: HistoryMessage[] = []
  let blockIndex = 0

  // Regex patterns for different block types
  const thinkingPattern = /\[thinking\]([\s\S]*?)\[\/thinking\]/g
  const toolUsePattern = /\[tool_use id="([^"]+)"\]([\s\S]*?)\[\/tool_use\]/g
  const toolResultPattern = /\[tool_result tool_use_id="([^"]+)"\]([\s\S]*?)\[\/tool_result\]/g
  const askUserQuestionPattern = /\[ask_user_question(?: id="([^"]+)")?\]([\s\S]*?)\[\/ask_user_question\]/g

  // Track positions of all matches
  const matches: Array<{
    type: 'thinking' | 'tool_use' | 'tool_result' | 'ask_user_question' | 'text'
    start: number
    end: number
    content: string
    id?: string
    name?: string
  }> = []

  // Find thinking blocks
  let match
  while ((match = thinkingPattern.exec(content)) !== null) {
    matches.push({
      type: 'thinking',
      start: match.index,
      end: match.index + match[0].length,
      content: match[1].trim(),
    })
  }

  // Find tool_use blocks
  while ((match = toolUsePattern.exec(content)) !== null) {
    const id = match[1]
    const innerContent = match[2].trim()

    // Parse tool name and input JSON
    // Format: "toolName {json}" or just "toolName"
    const nameMatch = innerContent.match(/^(\S+)\s*([\s\S]*)$/)
    const toolName = nameMatch ? nameMatch[1] : innerContent
    const toolInput = nameMatch ? nameMatch[2] : ''

    matches.push({
      type: 'tool_use',
      start: match.index,
      end: match.index + match[0].length,
      content: toolInput,
      id,
      name: toolName,
    })
  }

  // Find tool_result blocks
  while ((match = toolResultPattern.exec(content)) !== null) {
    matches.push({
      type: 'tool_result',
      start: match.index,
      end: match.index + match[0].length,
      content: match[2].trim(),
      id: match[1],
    })
  }

  // Find ask_user_question blocks (id attr pairs the card with userQuestions
  // store pending entries — without it the submit button disappears on reload)
  while ((match = askUserQuestionPattern.exec(content)) !== null) {
    matches.push({
      type: 'ask_user_question',
      start: match.index,
      end: match.index + match[0].length,
      content: match[2].trim(),
      id: match[1],
    })
  }

  // Sort by start position
  matches.sort((a, b) => a.start - b.start)

  // Extract text blocks between special blocks. Collect into a separate array —
  // pushing into `matches` while iterating it makes the new entries get visited
  // by the same loop, clobbering lastEnd and re-emitting trailing markers
  // (e.g. a duplicate raw [tool_use] block) as text.
  const textBlocks: Array<{
    type: 'thinking' | 'tool_use' | 'tool_result' | 'ask_user_question' | 'text'
    start: number
    end: number
    content: string
    id?: string
    name?: string
  }> = []
  let lastEnd = 0
  for (const m of matches) {
    if (m.start > lastEnd) {
      const textContent = content.slice(lastEnd, m.start).trim()
      if (textContent) {
        textBlocks.push({
          type: 'text',
          start: lastEnd,
          end: m.start,
          content: textContent,
        })
      }
    }
    lastEnd = m.end
  }

  // Add trailing text
  if (lastEnd < content.length) {
    const textContent = content.slice(lastEnd).trim()
    if (textContent) {
      textBlocks.push({
        type: 'text',
        start: lastEnd,
        end: content.length,
        content: textContent,
      })
    }
  }
  matches.push(...textBlocks)

  // Sort again after adding text blocks
  matches.sort((a, b) => a.start - b.start)

  // Convert matches to HistoryMessage objects
  for (const m of matches) {
    const blockId = msg.uuid ? `${msg.uuid}-${blockIndex}` : fallbackId()
    blockIndex++

    if (m.type === 'text') {
      blocks.push({
        role: msg.role,
        type: msg.type,
        content: m.content,
        timestamp: msg.timestamp,
        uuid: msg.uuid,
        parentUuid: msg.parentUuid,
        userType: msg.userType,
        id: blockId,
        done: true,
      })
    } else if (m.type === 'thinking') {
      blocks.push({
        role: msg.role,
        type: msg.type,
        content: `[thinking]\n${m.content}\n[/thinking]`,
        timestamp: msg.timestamp,
        uuid: msg.uuid,
        parentUuid: msg.parentUuid,
        userType: msg.userType,
        id: blockId,
        done: true,
      })
    } else if (m.type === 'tool_use') {
      blocks.push({
        role: 'assistant',
        type: 'assistant',
        content: m.content,
        timestamp: msg.timestamp,
        uuid: msg.uuid,
        parentUuid: msg.parentUuid,
        userType: msg.userType,
        id: blockId,
        done: true,
        toolName: m.name,
        toolUseId: m.id,
      })
    } else if (m.type === 'tool_result') {
      blocks.push({
        role: 'user',
        type: 'user',
        content: m.content,
        timestamp: msg.timestamp,
        uuid: msg.uuid,
        parentUuid: msg.parentUuid,
        userType: msg.userType,
        id: blockId,
        done: true,
        toolUseId: m.id,
      })
    } else if (m.type === 'ask_user_question') {
      // Treat as tool_use with special name
      blocks.push({
        role: 'assistant',
        type: 'assistant',
        content: m.content,
        timestamp: msg.timestamp,
        uuid: msg.uuid,
        parentUuid: msg.parentUuid,
        userType: msg.userType,
        id: blockId,
        done: true,
        toolName: 'AskUserQuestion',
        toolUseId: m.id,
      })
    }
  }

  // If no blocks were parsed (no markers found), return original message
  if (blocks.length === 0) {
    return [
      {
        ...msg,
        id: msg.uuid ? `${msg.uuid}-0` : fallbackId(),
        done: true,
      },
    ]
  }

  return blocks
}
