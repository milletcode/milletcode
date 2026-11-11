<script lang="ts">
  import { _ } from 'svelte-i18n'
  import type { HistoryMessage } from '$lib/stores/session'
  import type { ViewMode } from '$lib/pure/messageFilter'
  import { userQuestions, removeUserQuestion } from '$lib/stores/approval'
  import { sendWSMessage, getWSStatus } from '$lib/wsPool'
  import { apiFetch } from '$lib/api'
  import { addNotification } from '$lib/stores/notification'

  interface Props {
    message: HistoryMessage
    sessionId: string
    viewMode?: ViewMode
  }

  let { message, sessionId, viewMode = 'detailed' }: Props = $props()

  // Parse the question data from message content
  const questionData = $derived.by(() => {
    try {
      const parsed = JSON.parse(message.content)
      return {
        questions: parsed.questions || [],
        tool_use_id: message.toolUseId
      }
    } catch {
      return { questions: [], tool_use_id: message.toolUseId }
    }
  })

  // Check if this question is still pending: it's in the userQuestions store
  // with a matching tool_use_id (live in-band control request)
  const pendingQuestion = $derived.by(() => {
    const questions = $userQuestions
    return questions.get(sessionId)
  })

  const isPending = $derived(pendingQuestion?.tool_use_id === message.toolUseId)

  let selectedAnswers = $state<Record<number, string>>({})
  let correctionText = $state<Record<number, string>>({})

  // Sentinel marking "none of the options is right" — the real answer is the
  // free-text correction, forwarded to claude as that question's answer value.
  const CORRECTION = '__correction__'

  function handleSelectOption(questionIdx: number, optionLabel: string) {
    selectedAnswers = { ...selectedAnswers, [questionIdx]: optionLabel }
  }

  function handleSelectCorrection(questionIdx: number) {
    selectedAnswers = { ...selectedAnswers, [questionIdx]: CORRECTION }
  }

  function isCorrectionMode(questionIdx: number): boolean {
    return selectedAnswers[questionIdx] === CORRECTION
  }

  function hasValidAnswer(questionIdx: number): boolean {
    const answer = selectedAnswers[questionIdx]
    if (!answer) return false
    if (answer === CORRECTION) return !!correctionText[questionIdx]?.trim()
    return true
  }

  const allAnswered = $derived(questionData.questions.every((_: unknown, idx: number) => hasValidAnswer(idx)))
  const answeredCount = $derived(questionData.questions.filter((_: unknown, idx: number) => hasValidAnswer(idx)).length)

  async function handleSubmit() {
    if (!isPending || !pendingQuestion?.request_id) return

    // Full mapping: question text -> chosen label, or the typed correction
    // text when "有错误,我来修正" was selected (backend forwards as `answers`)
    const questions = questionData.questions
    const answers: Record<string, string> = {}
    for (const [qIdxStr, answer] of Object.entries(selectedAnswers)) {
      const q = questions[parseInt(qIdxStr)]
      if (!q) continue
      answers[q.question] = answer === CORRECTION
        ? (correctionText[parseInt(qIdxStr)] || '').trim()
        : answer
    }

    // WS 优先（断连时 sendWSMessage 是静默 no-op，答案会凭空丢失），断连时回落 REST
    if (getWSStatus() === 'disconnected') {
      try {
        const res = await apiFetch(`/api/sessions/${sessionId}/answer`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            request_id: pendingQuestion.request_id,
            answers,
          }),
        })
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
      } catch {
        addNotification({
          id: `answer-${sessionId}-${Date.now()}`,
          level: 'error',
          message: $_('messages.submitFailed'),
          ts: Date.now(),
        })
        return
      }
      removeUserQuestion(sessionId)
      return
    }
    sendWSMessage({
      type: 'answer_question',
      session_id: sessionId,
      request_id: pendingQuestion.request_id,
      answers,
    })
    removeUserQuestion(sessionId)
  }

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
  <!-- 缩略模式：展开显示所有问题和选项 -->
  <div class="flex gap-3 p-4 bg-amber-900/20 rounded-lg border border-amber-700/30">
    <div class="w-6 h-6 rounded-full bg-amber-600/50 flex items-center justify-center flex-shrink-0">
      <svg class="w-3 h-3 text-amber-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8.228 9c.549-1.165 2.03-2 3.772-2 2.21 0 4 1.343 4 3 0 1.4-1.278 2.575-3.006 2.907-.542.104-.994.54-.994 1.093m0 3h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
      </svg>
    </div>
    <div class="flex-1 min-w-0">
      <div class="flex items-center gap-2 mb-3">
        <span class="text-xs font-semibold text-amber-400">AskUserQuestion</span>
        <span class="text-gray-500 text-xs">{formatTimestamp(message.timestamp)}</span>
        {#if isPending}
          <span class="text-xs px-1.5 py-0.5 bg-amber-900/50 text-amber-400 rounded animate-pulse">{$_('messages.waitingQuestion')}</span>
        {:else}
          <span class="text-xs px-1.5 py-0.5 bg-gray-700/50 text-gray-400 rounded">{$_('messages.answered')}</span>
        {/if}
      </div>

      <div class="space-y-3">
        {#each questionData.questions as q, qIdx}
          <div class="space-y-1.5">
            <!-- 问题内容 -->
            <div class="text-sm text-gray-200 font-medium">
              {q.question}
            </div>
            {#if q.header}
              <div class="text-xs text-gray-400">
                {q.header}
              </div>
            {/if}
            <!-- 选项列表 -->
            <div class="space-y-1 ml-2">
              {#each q.options as option}
                <div class="text-xs flex items-start gap-2">
                  <span class="text-amber-500 flex-shrink-0">•</span>
                  <div class="flex-1">
                    <span class="text-gray-300 font-medium">{option.label}</span>
                    {#if option.description}
                      <span class="text-gray-500 ml-1">— {option.description}</span>
                    {/if}
                  </div>
                </div>
              {/each}
            </div>
          </div>
          {#if qIdx < questionData.questions.length - 1}
            <div class="border-t border-amber-700/20"></div>
          {/if}
        {/each}
      </div>
    </div>
  </div>
{:else}
  <!-- 详细模式：完整显示问题和选项 -->
  <div class="flex gap-3 p-4 bg-amber-900/20 rounded-lg border border-amber-700/30">
    <div class="w-8 h-8 rounded-full bg-amber-600/50 flex items-center justify-center flex-shrink-0">
      <svg class="w-4 h-4 text-amber-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8.228 9c.549-1.165 2.03-2 3.772-2 2.21 0 4 1.343 4 3 0 1.4-1.278 2.575-3.006 2.907-.542.104-.994.54-.994 1.093m0 3h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
      </svg>
    </div>
    <div class="flex-1 min-w-0">
      <div class="flex items-center gap-2 mb-2">
        <span class="text-xs font-semibold text-amber-400">AskUserQuestion</span>
        <span class="text-gray-500 text-xs">{formatTimestamp(message.timestamp)}</span>
        {#if isPending}
          <span class="text-xs px-1.5 py-0.5 bg-amber-900/50 text-amber-400 rounded animate-pulse">{$_('messages.waitingQuestion')}</span>
        {:else}
          <span class="text-xs px-1.5 py-0.5 bg-gray-700/50 text-gray-400 rounded">{$_('messages.answered')}</span>
        {/if}
      </div>

      <div class="space-y-4">
        {#each questionData.questions as q, qIdx}
          <div class="space-y-2">
            <div class="text-sm text-gray-200 font-medium">
              {q.question}
            </div>
            <div class="text-xs text-gray-400">
              {q.header}
            </div>
            <div class="space-y-1">
              {#each q.options as option}
                <button
                  class="w-full text-left px-3 py-2 rounded border transition-colors {
                    selectedAnswers[qIdx] === option.label
                      ? 'bg-amber-700/30 border-amber-600 text-amber-200'
                      : 'bg-gray-800/50 border-gray-700/50 text-gray-300 hover:bg-gray-800 hover:border-gray-600'
                  }"
                  onclick={() => handleSelectOption(qIdx, option.label)}
                  disabled={!isPending}
                >
                  <div class="text-sm font-medium">{option.label}</div>
                  {#if option.description}
                    <div class="text-xs text-gray-400 mt-0.5">{option.description}</div>
                  {/if}
                </button>
              {/each}

              <!-- 否决选项：选项都不对时，自由输入修正 -->
              <button
                class="w-full text-left px-3 py-2 rounded border transition-colors {
                  isCorrectionMode(qIdx)
                    ? 'bg-red-800/40 border-red-600 text-red-200'
                    : 'bg-gray-800/50 border-gray-700/50 text-gray-400 hover:bg-gray-800 hover:border-red-700/60'
                }"
                onclick={() => handleSelectCorrection(qIdx)}
                disabled={!isPending}
              >
                <div class="text-sm font-medium">{$_('messages.correctionOption')}</div>
              </button>

              {#if isCorrectionMode(qIdx)}
                <textarea
                  class="w-full px-3 py-2 rounded bg-gray-900 border border-red-700/50 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-red-500 resize-y"
                  rows="3"
                  placeholder={$_('messages.correctionPlaceholder')}
                  bind:value={correctionText[qIdx]}
                  disabled={!isPending}
                ></textarea>
              {/if}
            </div>
          </div>
        {/each}

        {#if isPending}
          <button
            class="w-full px-4 py-2 rounded bg-amber-600 hover:bg-amber-500 text-white text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            onclick={handleSubmit}
            disabled={!allAnswered}
          >
            {$_('messages.submitAnswers')} ({answeredCount}/{questionData.questions.length})
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}
