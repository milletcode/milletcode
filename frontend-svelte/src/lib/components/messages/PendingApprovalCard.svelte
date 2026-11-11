<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { ShieldAlert } from 'lucide-svelte'
  import { approvals, removeApproval } from '$lib/stores/approval'
  import { sendWSMessage, getWSStatus } from '$lib/wsPool'
  import { apiFetch } from '$lib/api'
  import { addNotification } from '$lib/stores/notification'

  interface Props {
    sessionId: string
  }

  let { sessionId }: Props = $props()

  const pending = $derived($approvals.get(sessionId))

  let comment = $state('')
  let submitting = $state(false)

  // store 里重新出现 pending 条目（WS 断连时 fetchPending 会拉回未决的审批）
  // 说明上一次提交没送到后端 —— 必须复位 submitting，否则按钮永久禁用。
  $effect(() => {
    if (pending?.request_id) submitting = false
  })

  async function decide(decision: 'approve' | 'deny') {
    if (!pending?.request_id || submitting) return
    submitting = true
    // 与 Approvals.svelte 相同的策略：WS 连接时走 WS（断连时 sendWSMessage
    // 是静默 no-op，决定会凭空丢失），断连时回落 REST。
    if (getWSStatus() === 'disconnected') {
      try {
        const res = await apiFetch(`/api/sessions/${sessionId}/approve`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            request_id: pending.request_id,
            selected: decision,
            comment: comment || undefined,
          }),
        })
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
      } catch {
        submitting = false
        addNotification({
          id: `approval-${sessionId}-${Date.now()}`,
          level: 'error',
          message: $_('messages.submitFailed'),
          ts: Date.now(),
        })
        return
      }
      removeApproval(sessionId)
      return
    }
    sendWSMessage({
      type: 'approve',
      session_id: sessionId,
      request_id: pending.request_id,
      decision,
      comment: comment || undefined,
    })
    removeApproval(sessionId)
  }
</script>

{#if pending}
  <div class="mx-4 mb-2 flex gap-3 p-4 bg-blue-900/20 rounded-lg border border-blue-700/40">
    <div class="w-8 h-8 rounded-full bg-blue-600/50 flex items-center justify-center flex-shrink-0">
      <ShieldAlert class="w-4 h-4 text-blue-300" />
    </div>
    <div class="flex-1 min-w-0">
      <div class="flex items-center gap-2 mb-2">
        <span class="text-xs font-semibold text-blue-300">
          {pending.tool_name || $_('messages.tool')}
        </span>
        <span class="text-xs px-1.5 py-0.5 bg-blue-900/50 text-blue-300 rounded animate-pulse">
          {$_('messages.waitingApproval')}
        </span>
      </div>
      <pre class="text-xs text-gray-300 whitespace-pre-wrap break-all max-h-40 overflow-y-auto">{pending.prompt}</pre>

      {#if pending.request_id}
        <textarea
          bind:value={comment}
          placeholder={$_('messages.commentPlaceholder')}
          rows="2"
          class="w-full mt-3 px-3 py-2 rounded bg-gray-800/60 border border-gray-700/50 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-600 resize-none"
        ></textarea>
        <div class="flex gap-2 mt-2">
          <button
            class="px-4 py-1.5 rounded bg-green-700 hover:bg-green-600 text-white text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            onclick={() => decide('approve')}
            disabled={submitting}
          >
            {$_('messages.approve')}
          </button>
          <button
            class="px-4 py-1.5 rounded bg-red-700 hover:bg-red-600 text-white text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            onclick={() => decide('deny')}
            disabled={submitting}
          >
            {$_('messages.deny')}
          </button>
        </div>
      {:else}
        <div class="mt-2 text-xs text-yellow-400">
          {$_('messages.missingRequestId')}
        </div>
      {/if}
    </div>
  </div>
{/if}
