<script lang="ts">
  import type { HistoryMessage } from '$lib/stores/session'
  import type { ViewMode } from '$lib/pure/messageFilter'
  import RenderOutput from '../RenderOutput.svelte'
  import { Clipboard, ClipboardCheck } from 'lucide-svelte'

  interface Props {
    message: HistoryMessage
    viewMode?: ViewMode
  }

  let { message, viewMode = 'detailed' }: Props = $props()

  const TRUNCATE_THRESHOLD = 500

  let expanded = $state(false)
  let copied = $state(false)
  let copyTimer: ReturnType<typeof setTimeout> | null = null

  async function copyMessage() {
    try {
      await navigator.clipboard.writeText(plainText())
      copied = true
      if (copyTimer) clearTimeout(copyTimer)
      copyTimer = setTimeout(() => { copied = false }, 2000)
    } catch {
      // fallback
      const ta = document.createElement('textarea')
      ta.value = plainText()
      ta.style.position = 'fixed'
      ta.style.left = '-9999px'
      document.body.appendChild(ta)
      ta.select()
      document.execCommand('copy')
      document.body.removeChild(ta)
      copied = true
      if (copyTimer) clearTimeout(copyTimer)
      copyTimer = setTimeout(() => { copied = false }, 2000)
    }
  }

  // Get plain text content (strip all block tags)
  const plainText = $derived(() => {
    return message.content
      .replace(/\[thinking\][\s\S]*?\[\/thinking\]/g, '')
      .replace(/\[tool_use[^\]]*\][\s\S]*?\[\/tool_use\]/g, '')
      .replace(/\[tool_result[^\]]*\][\s\S]*?\[\/tool_result\]/g, '')
      .replace(/\[ask_user_question[^\]]*\][\s\S]*?\[\/ask_user_question\]/g, '')
      .trim()
  })

  const shouldTruncate = $derived(plainText().length > TRUNCATE_THRESHOLD)
  const displayContent = $derived(
    shouldTruncate && !expanded
      ? message.content.slice(0, TRUNCATE_THRESHOLD)
      : message.content
  )

  // Parse content into blocks: text, thinking, tool_use, tool_result, ask_user_question
  const contentBlocks = $derived(() => {
    const content = displayContent
    const blocks: Array<{ type: 'text' | 'thinking' | 'tool_use' | 'tool_result' | 'ask_user_question'; content: string }> = []

    // Pattern to match all block types; attrs (e.g. tool_use id / ask_user_question
    // id) are consumed outside the capture group so the \1 closing-tag backreference
    // stays the bare type name
    const blockPattern = /\[(thinking|tool_use|tool_result|ask_user_question)(?:[^\]]*)?\]([\s\S]*?)\[\/\1\]/g

    let lastIndex = 0
    let match

    while ((match = blockPattern.exec(content)) !== null) {
      // Add text before this block
      if (match.index > lastIndex) {
        const textBefore = content.slice(lastIndex, match.index).trim()
        if (textBefore) {
          blocks.push({ type: 'text', content: textBefore })
        }
      }

      // Normalize block type (tool_use:xxx -> tool_use)
      const rawType = match[1]
      const blockType = rawType.startsWith('tool_use') ? 'tool_use'
        : rawType.startsWith('tool_result') ? 'tool_result'
        : rawType as 'thinking' | 'tool_use' | 'tool_result' | 'ask_user_question'

      blocks.push({ type: blockType, content: match[2].trim() })
      lastIndex = match.index + match[0].length
    }

    // Add remaining text after last block
    if (lastIndex < content.length) {
      const textAfter = content.slice(lastIndex).trim()
      if (textAfter) {
        blocks.push({ type: 'text', content: textAfter })
      }
    }

    // If no blocks found, treat entire content as text
    if (blocks.length === 0 && content.trim()) {
      blocks.push({ type: 'text', content: content.trim() })
    }

    return blocks
  })

  function formatTimestamp(ts: number | undefined): string {
    if (!ts) return ''
    const date = new Date(ts)
    return date.toLocaleTimeString('zh-CN', {
      hour: '2-digit',
      minute: '2-digit',
      hour12: false
    })
  }

  // Outline/compact mode: check if this message contains only tool blocks (no thinking/text)
  const isFilteredInOutline = $derived(() => {
    if (viewMode !== 'outline' && viewMode !== 'compact') return false
    // Always show completed messages
    if (message.done === true) return false
    const content = message.content
    // Check if there's any thinking or text content
    const hasThinking = /\[thinking\]/.test(content)
    const stripped = content
      .replace(/\[thinking\][\s\S]*?\[\/thinking\]/g, '')
      .replace(/\[tool_use[^\]]*\][\s\S]*?\[\/tool_use\]/g, '')
      .replace(/\[tool_result[^\]]*\][\s\S]*?\[\/tool_result\]/g, '')
      .replace(/\[ask_user_question[^\]]*\][\s\S]*?\[\/ask_user_question\]/g, '')
      .trim()
    const hasText = stripped.length > 0
    return !hasThinking && !hasText
  })

  // Compact mode: hide pure thinking messages (no text content after removing thinking)
  const isHiddenInCompact = $derived(() => {
    if (viewMode !== 'compact') return false
    // Always show completed messages
    if (message.done === true) return false
    const content = message.content
    // Check if this is a pure thinking message
    const hasThinking = /\[thinking\]/.test(content)
    const textOnly = content
      .replace(/\[thinking\][\s\S]*?\[\/thinking\]/g, '')
      .trim()
    // If it has thinking but no text after removing thinking, hide it
    return hasThinking && textOnly.length === 0
  })
</script>

{#if isHiddenInCompact()}
  <!-- Hidden in compact mode: pure thinking message -->
{:else if isFilteredInOutline()}
  <div class="text-[10px] text-gray-600 font-mono py-1 px-2 border border-dashed border-gray-700/30 rounded">
    FILTERED: {message.content?.substring(0, 100)}...
  </div>
{:else}
  <div class="flex gap-3 p-4 bg-gray-800/50 rounded-lg">
    <div class="w-8 h-8 rounded-full bg-green-600 flex items-center justify-center flex-shrink-0">
      <span class="text-white text-sm font-bold">A</span>
    </div>
    <div class="flex-1 min-w-0">
      <div class="flex items-center gap-2 mb-1">
        <span class="text-xs text-gray-500">Assistant</span>
        <span class="text-gray-500 text-xs">{formatTimestamp(message.timestamp)}</span>
        {#if message.isError}
          <span class="text-xs px-1.5 py-0.5 bg-red-900/50 text-red-400 rounded">Error</span>
        {/if}
        {#if message.done === false}
          <span class="text-xs px-1.5 py-0.5 bg-blue-900/50 text-blue-400 rounded animate-pulse">Streaming</span>
        {/if}
        <button
          class="ml-auto text-gray-500 hover:text-gray-300 transition-colors p-1 rounded hover:bg-gray-700/50"
          onclick={copyMessage}
          title="复制消息"
        >
          {#if copied}
            <ClipboardCheck class="w-3.5 h-3.5 text-green-400" />
          {:else}
            <Clipboard class="w-3.5 h-3.5" />
          {/if}
        </button>
      </div>
      <div
        class="text-gray-100 {shouldTruncate && !expanded ? 'cursor-pointer' : ''}"
        onclick={() => shouldTruncate && !expanded && (expanded = true)}
      >
        {#each contentBlocks() as block}
          {#if block.type === 'thinking'}
            {#if viewMode !== 'compact'}
              <details class="mb-2 bg-gray-900/50 rounded border border-gray-700/50">
                <summary class="px-2 py-1 text-xs text-gray-400 cursor-pointer hover:text-gray-300 flex items-center gap-1.5">
                  <svg class="w-3 h-3" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                    <path d="M12 2a7 7 0 0 1 7 7c0 2.5-1.5 4.5-3 6l-1 3H9l-1-3c-1.5-1.5-3-3.5-3-6a7 7 0 0 1 7-7z"/>
                    <path d="M9 18h6"/>
                    <path d="M10 22h4"/>
                  </svg>
                  <span>Thinking</span>
                </summary>
                <div class="px-3 py-2 text-xs text-gray-500 whitespace-pre-wrap">
                  {block.content}
                </div>
              </details>
            {/if}
          {:else if block.type === 'tool_use' || block.type === 'tool_result' || block.type === 'ask_user_question'}
            {#if viewMode === 'detailed'}
              <RenderOutput content={`[${block.type}]${block.content}[/${block.type}]`} done={message.done ?? true} />
            {/if}
          {:else}
            <RenderOutput content={block.content} done={message.done ?? true} />
          {/if}
        {/each}
        {#if shouldTruncate && !expanded}
          <button class="mt-2 text-xs text-blue-400 hover:text-blue-300 transition-colors">
            展开全文 ({plainText().length} 字符)
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}
