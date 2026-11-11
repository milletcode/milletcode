<script lang="ts">
  import type { HistoryMessage } from '$lib/stores/session'
  import type { ViewMode } from '$lib/pure/messageFilter'
  import RenderOutput from '../RenderOutput.svelte'

  interface Props {
    message: HistoryMessage
    viewMode?: ViewMode
  }

  let { message, viewMode = 'detailed' }: Props = $props()

  let collapsed = $state(true)

  function formatTimestamp(ts: number | undefined): string {
    if (!ts) return ''
    const date = new Date(ts)
    return date.toLocaleTimeString('zh-CN', {
      hour: '2-digit',
      minute: '2-digit',
      hour12: false
    })
  }
</script>

{#if viewMode === 'outline' || viewMode === 'compact'}
  <!-- Hidden in outline/compact mode -->
{:else}
  <div class="flex gap-3 p-3 bg-gray-900/50 rounded-lg border border-gray-700/50">
    <div class="w-8 h-8 rounded-full bg-purple-600/50 flex items-center justify-center flex-shrink-0">
      <svg class="w-4 h-4 text-purple-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
      </svg>
    </div>
    <div class="flex-1 min-w-0">
      <div class="flex items-center gap-2 cursor-pointer" onclick={() => collapsed = !collapsed}>
        <span class="text-xs font-semibold text-purple-400">{message.toolName || 'Tool'}</span>
        <span class="text-gray-500 text-xs">{formatTimestamp(message.timestamp)}</span>
        {#if message.isError}
          <span class="text-xs px-1.5 py-0.5 bg-red-900/50 text-red-400 rounded">Error</span>
        {/if}
        <span class="text-xs text-gray-500 ml-auto">{collapsed ? '▶' : '▼'}</span>
      </div>
      {#if !collapsed}
        <div class="mt-2 text-xs font-mono text-gray-400 space-y-2">
          {#if message.content}
            <div>
              <div class="text-gray-500 mb-1 text-xs">Content:</div>
              <div class="bg-gray-800 p-2 rounded overflow-x-auto max-h-60">
                <RenderOutput content={message.content} done={true} />
              </div>
            </div>
          {/if}
        </div>
      {/if}
    </div>
  </div>
{/if}
