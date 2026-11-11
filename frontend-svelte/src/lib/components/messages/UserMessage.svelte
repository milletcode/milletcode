<script lang="ts">
  import type { HistoryMessage } from '$lib/stores/session'
  import RenderOutput from '../RenderOutput.svelte'
  import { Clipboard, ClipboardCheck } from 'lucide-svelte'

  interface Props {
    message: HistoryMessage
  }

  let { message }: Props = $props()

  const TRUNCATE_THRESHOLD = 500

  let expanded = $state(false)
  let copied = $state(false)
  let copyTimer: ReturnType<typeof setTimeout> | null = null
  const shouldTruncate = $derived(message.content.length > TRUNCATE_THRESHOLD)
  const displayContent = $derived(
    shouldTruncate && !expanded
      ? message.content.slice(0, TRUNCATE_THRESHOLD) + '...'
      : message.content
  )

  function formatTimestamp(ts: number | undefined): string {
    if (!ts) return ''
    const date = new Date(ts)
    return date.toLocaleTimeString('zh-CN', {
      hour: '2-digit',
      minute: '2-digit',
      hour12: false
    })
  }

  async function copyMessage() {
    try {
      await navigator.clipboard.writeText(message.content)
      copied = true
      if (copyTimer) clearTimeout(copyTimer)
      copyTimer = setTimeout(() => { copied = false }, 2000)
    } catch {
      // fallback
      const ta = document.createElement('textarea')
      ta.value = message.content
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
</script>

<div class="flex gap-3 p-4 bg-gray-800/30 rounded-lg flex-row-reverse">
  <div class="w-8 h-8 rounded-full bg-blue-600 flex items-center justify-center flex-shrink-0">
    <span class="text-white text-sm font-bold">U</span>
  </div>
  <div class="flex-1 min-w-0">
    <div class="flex items-center justify-end gap-2 mb-1">
      <button
        class="text-gray-500 hover:text-gray-300 transition-colors p-1 rounded hover:bg-gray-700/50"
        onclick={copyMessage}
        title="复制消息"
      >
        {#if copied}
          <ClipboardCheck class="w-3.5 h-3.5 text-green-400" />
        {:else}
          <Clipboard class="w-3.5 h-3.5" />
        {/if}
      </button>
      <span class="text-gray-500 text-xs">{formatTimestamp(message.timestamp)}</span>
      <span class="text-xs text-gray-500">You</span>
    </div>
    <div
      class="text-gray-100 {shouldTruncate && !expanded ? 'cursor-pointer' : ''}"
      onclick={() => shouldTruncate && !expanded && (expanded = true)}
    >
      <RenderOutput content={displayContent} done={true} />
      {#if shouldTruncate && !expanded}
        <button class="mt-1 text-xs text-blue-400 hover:text-blue-300 transition-colors">
          展开全文 ({message.content.length} 字符)
        </button>
      {/if}
    </div>
  </div>
</div>
