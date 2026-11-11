<script lang="ts">
  import type { DisplayMessage, ViewMode } from '$lib/pure/messageFilter'
  import Message from './messages/Message.svelte'

  interface Props {
    messages: DisplayMessage[]
    viewMode?: ViewMode
    sessionId?: string
    isStreaming?: boolean
  }

  let { messages, viewMode = 'detailed', sessionId = '', isStreaming = false }: Props = $props()
</script>

<div class="space-y-3">
  {#each messages as displayMsg, idx (displayMsg.msg.id || displayMsg.msg.uuid || `idx-${idx}`)}
    <div class="message-item">
      <Message {displayMsg} {viewMode} {sessionId} />
    </div>
  {/each}

  <!-- Thinking indicator for compact/outline modes when streaming -->
  {#if isStreaming && (viewMode === 'compact' || viewMode === 'outline')}
    <div class="flex items-center gap-1 px-4 py-2 text-gray-400">
      <span class="animate-bounce" style="animation-delay: 0ms">.</span>
      <span class="animate-bounce" style="animation-delay: 150ms">.</span>
      <span class="animate-bounce" style="animation-delay: 300ms">.</span>
    </div>
  {/if}

  {#if messages.length === 0 && !isStreaming}
    <div class="text-center text-gray-500 py-12">
      <p>No messages yet</p>
    </div>
  {/if}
</div>

<style>
  .message-item {
    content-visibility: auto;
    contain-intrinsic-size: auto 200px;
  }
</style>
