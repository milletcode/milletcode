<script lang="ts">
  import type { DisplayMessage } from '$lib/pure/messageFilter'
  import type { ViewMode } from '$lib/pure/messageFilter'
  import UserMessage from './UserMessage.svelte'
  import AssistantMessage from './AssistantMessage.svelte'
  import ToolMessage from './ToolMessage.svelte'
  import AskUserQuestionCard from './AskUserQuestionCard.svelte'

  interface Props {
    displayMsg: DisplayMessage
    viewMode?: ViewMode
    sessionId?: string
  }

  let { displayMsg, viewMode = 'detailed', sessionId = '' }: Props = $props()

  // Tool messages are identified by toolName or toolUseId fields
  const isToolMessage = $derived(displayMsg.msg.toolName || displayMsg.msg.toolUseId)
  const isAskUserQuestion = $derived(displayMsg.msg.toolName === 'AskUserQuestion')
</script>

{#if isAskUserQuestion}
  <AskUserQuestionCard message={displayMsg.msg} {sessionId} {viewMode} />
{:else if isToolMessage}
  <ToolMessage message={displayMsg.msg} {viewMode} />
{:else if displayMsg.msg.role === 'user'}
  <UserMessage message={displayMsg.msg} />
{:else if displayMsg.msg.role === 'assistant'}
  <AssistantMessage message={displayMsg.msg} {viewMode} />
{/if}
