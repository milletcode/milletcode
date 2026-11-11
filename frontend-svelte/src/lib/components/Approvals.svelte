<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { onMount } from 'svelte'
  import { ShieldAlert, HelpCircle, Inbox, ExternalLink, Server } from 'lucide-svelte'
  import {
    approvals,
    userQuestions,
    removeApproval,
    removeUserQuestion,
    type ApprovalRequest,
    type UserQuestionRequest,
  } from '$lib/stores/approval'
  import { sendWSMessage, getWSStatus } from '$lib/wsPool'
  import { apiFetch } from '$lib/api'
  import { addNotification } from '$lib/stores/notification'

  interface Props {
    navigate: (path: string) => void
  }
  let { navigate }: Props = $props()

  type PendingItem =
    | { kind: 'approval'; ts: number; data: ApprovalRequest }
    | { kind: 'question'; ts: number; data: UserQuestionRequest }

  // GET /api/approvals 的 DecisionRecord
  interface HistoryRecord {
    request_id: string
    session_id: string
    project_name?: string
    kind: string // "approval" | "question"
    tool_name?: string
    summary: string // approval: 审批 prompt；question: 答案 JSON
    decision: string // "approved" | "denied" | "answered"
    comment?: string
    created_at: number
    decided_at: number
  }

  type AllItem = PendingItem | { kind: 'history'; ts: number; data: HistoryRecord }

  const items = $derived.by(() => {
    const list: PendingItem[] = []
    for (const a of $approvals.values()) list.push({ kind: 'approval', ts: a.receivedAt ?? 0, data: a })
    for (const q of $userQuestions.values()) list.push({ kind: 'question', ts: q.receivedAt ?? 0, data: q })
    list.sort((x, y) => y.ts - x.ts)
    return list
  })

  let activeTab = $state<'pending' | 'all'>('pending')
  let history = $state<HistoryRecord[]>([])
  let historyLoading = $state(false)
  let historyError = $state(false)

  async function fetchHistory() {
    historyLoading = true
    historyError = false
    try {
      const res = await apiFetch('/api/approvals?limit=100')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const d = await res.json()
      history = d.records ?? []
    } catch {
      historyError = true
    } finally {
      historyLoading = false
    }
  }

  onMount(fetchHistory)

  function switchTab(tab: 'pending' | 'all') {
    activeTab = tab
    if (tab === 'all') fetchHistory()
  }

  // “全部” = 待处理条目 + 已决策历史；同一 request_id 仍在待处理时去重
  const allItems = $derived.by(() => {
    const pendingIds = new Set<string>()
    for (const a of $approvals.values()) if (a.request_id) pendingIds.add(a.request_id)
    for (const q of $userQuestions.values()) if (q.request_id) pendingIds.add(q.request_id)
    const merged: AllItem[] = [...items]
    for (const r of history) {
      if (pendingIds.has(r.request_id)) continue
      merged.push({ kind: 'history', ts: r.decided_at || r.created_at, data: r })
    }
    merged.sort((x, y) => y.ts - x.ts)
    return merged
  })

  const listItems: AllItem[] = $derived(activeTab === 'pending' ? items : allItems)

  function keyOf(item: AllItem): string {
    // 历史表按 (decided_at, request_id) 存储，同一 request_id 可能出现多次
    return item.kind === 'history'
      ? `h:${item.data.decided_at}:${item.data.request_id}`
      : `p:${item.data.session_id}`
  }

  // Composite session_id = "org/project/agent-name/session-name"; the org
  // segment identifies the environment (local machine, remote host, …) and
  // the project segment may itself contain slashes.
  function sourceFromSession(sessionId: string, projectName?: string): { env: string; project: string; sessionName: string } {
    const segs = sessionId.split('/')
    const env = segs[0] || ''
    const sessionName = segs.length >= 2 ? segs[segs.length - 1] : sessionId
    const project =
      projectName ||
      (segs.length >= 4 ? segs.slice(1, segs.length - 2).join('/') : segs[1] || '')
    return { env, project, sessionName }
  }

  function sourceOf(data: ApprovalRequest | UserQuestionRequest) {
    return sourceFromSession(data.session_id, data.project_name ?? undefined)
  }

  function openSession(sessionId: string) {
    navigate(`/sessions/${sessionId}`)
  }

  // Composite session_id = "org/project/agent-name/session-name"; the project
  // segment may itself contain slashes, so split from the right.
  function sessionRestPath(sessionId: string): string | null {
    const parts = sessionId.split('/')
    if (parts.length < 4) return null
    const name = parts.pop() as string
    const agent = parts.pop() as string
    const org = parts.shift() as string
    const project = parts.join('/')
    return `${org}/${project}/${agent}/${name}`
  }

  // WS first (backend routes decisions globally by session_id); REST when no
  // connection exists — e.g. the user landed directly on this page.
  function submitDecision(wsPayload: Record<string, unknown>, restPath: string, restBody: Record<string, unknown>, onSent: () => void) {
    if (getWSStatus() === 'disconnected') {
      apiFetch(`/api/sessions/${restPath}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(restBody),
      })
        .then((res) => {
          if (!res.ok) throw new Error(`HTTP ${res.status}`)
          onSent()
        })
        .catch(() => {
          addNotification({
            id: `panel-${Date.now()}`,
            level: 'error',
            message: $_('messages.submitFailed'),
            ts: Date.now(),
          })
        })
    } else {
      sendWSMessage(wsPayload)
      onSent()
    }
  }

  let approvalComments = $state<Record<string, string>>({})

  function decide(data: ApprovalRequest, decision: 'approve' | 'deny') {
    if (!data.request_id) return
    const comment = approvalComments[data.session_id] || undefined
    submitDecision(
      { type: 'approve', session_id: data.session_id, request_id: data.request_id, decision, comment },
      `${sessionRestPath(data.session_id)}/approve`,
      { request_id: data.request_id, selected: decision, comment },
      () => {
        removeApproval(data.session_id)
        // WS 发送是 fire-and-forget：立刻拉历史可能赶在后端落库前，
        // 刚决策的条目会在两个标签里都消失，延迟半秒再刷新
        setTimeout(fetchHistory, 500)
      }
    )
  }

  let questionAnswers = $state<Record<string, Record<number, string>>>({})
  let correctionTexts = $state<Record<string, Record<number, string>>>({})

  // 与 AskUserQuestionCard 相同的否决哨兵：选项都不对时，答案是自由输入的修正文本
  const CORRECTION = '__correction__'

  function selectOption(sessionId: string, qIdx: number, label: string) {
    questionAnswers = {
      ...questionAnswers,
      [sessionId]: { ...(questionAnswers[sessionId] || {}), [qIdx]: label },
    }
  }

  function hasValidAnswer(sessionId: string, qIdx: number): boolean {
    const answer = questionAnswers[sessionId]?.[qIdx]
    if (!answer) return false
    if (answer === CORRECTION) return !!correctionTexts[sessionId]?.[qIdx]?.trim()
    return true
  }

  function answeredCount(data: UserQuestionRequest): number {
    return data.questions.filter((_, qIdx) => hasValidAnswer(data.session_id, qIdx)).length
  }

  function submitAnswers(data: UserQuestionRequest) {
    const selected = questionAnswers[data.session_id] || {}
    const answers: Record<string, string> = {}
    for (const [qIdxStr, label] of Object.entries(selected)) {
      const q = data.questions[parseInt(qIdxStr)]
      if (!q) continue
      answers[q.question] = label === CORRECTION
        ? (correctionTexts[data.session_id]?.[parseInt(qIdxStr)] || '').trim()
        : label
    }
    submitDecision(
      { type: 'answer_question', session_id: data.session_id, request_id: data.request_id, answers },
      `${sessionRestPath(data.session_id)}/answer`,
      { request_id: data.request_id, answers },
      () => {
        removeUserQuestion(data.session_id)
        setTimeout(fetchHistory, 500)
      }
    )
  }

  function isFresh(ts: number | undefined): boolean {
    return ts != null && Date.now() - ts < 3000
  }

  function formatTime(ts: number): string {
    if (!ts) return ''
    return new Date(ts).toLocaleString(undefined, {
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      hour12: false,
    })
  }

  // question 历史的 summary 是答案 JSON：{"问题": "选中项", ...}
  function parsedAnswers(r: HistoryRecord): Record<string, string> | null {
    if (r.kind !== 'question') return null
    try {
      const v = JSON.parse(r.summary)
      if (v && typeof v === 'object') return v as Record<string, string>
    } catch {
      // fall through
    }
    return null
  }
</script>

<div class="space-y-6">
  <h1 class="text-2xl font-bold">{$_('messages.title')}</h1>

  <!-- Tab Switcher -->
  <div class="inline-flex rounded-lg bg-gray-800 p-0.5">
    <button
      onclick={() => switchTab('pending')}
      class="px-4 py-1.5 rounded-md text-sm font-medium transition-colors {activeTab === 'pending'
        ? 'bg-blue-600 text-white'
        : 'text-gray-400 hover:text-white'}"
    >
      {$_('messages.tabPending')}
      <span class="ml-1.5 px-1.5 py-0.5 rounded-full text-xs {activeTab === 'pending' ? 'bg-blue-500/30 text-blue-100' : 'bg-gray-700 text-gray-300'}">{items.length}</span>
    </button>
    <button
      onclick={() => switchTab('all')}
      class="px-4 py-1.5 rounded-md text-sm font-medium transition-colors {activeTab === 'all'
        ? 'bg-blue-600 text-white'
        : 'text-gray-400 hover:text-white'}"
    >
      {$_('messages.tabAll')}
      <span class="ml-1.5 px-1.5 py-0.5 rounded-full text-xs {activeTab === 'all' ? 'bg-blue-500/30 text-blue-100' : 'bg-gray-700 text-gray-300'}">{allItems.length}</span>
    </button>
  </div>

  {#if activeTab === 'all' && historyLoading && listItems.length === 0}
    <div class="text-center text-gray-500 mt-20">{$_('common.loading')}</div>
  {:else if listItems.length === 0}
    <div class="text-center text-gray-500 mt-20">
      <Inbox class="w-12 h-12 mx-auto mb-4 text-gray-600" />
      <p>{activeTab === 'pending' ? $_('messages.noPending') : $_('messages.noHistory')}</p>
    </div>
  {:else}
    {#if activeTab === 'all' && historyError}
      <div class="text-xs text-yellow-400">{$_('messages.historyLoadFailed')}</div>
    {/if}
    <div class="space-y-4">
      {#each listItems as item (keyOf(item))}
        {#if item.kind === 'approval'}
          {@const data = item.data}
          {@const source = sourceOf(data)}
          <div class="bg-gray-900 rounded-lg border border-gray-800 p-5 {isFresh(data.receivedAt) ? 'msg-flash' : ''}">
            <div class="flex items-start justify-between gap-3 mb-3">
              <div class="min-w-0">
                <div class="flex items-center gap-2 flex-wrap">
                  <span class="w-7 h-7 rounded-full bg-blue-600/40 flex items-center justify-center flex-shrink-0">
                    <ShieldAlert class="w-4 h-4 text-blue-300" />
                  </span>
                  <span class="text-base font-semibold text-white truncate">{source.project}</span>
                  <span class="inline-flex items-center gap-1 px-2 py-0.5 rounded text-xs font-medium bg-indigo-500/15 text-indigo-300 border border-indigo-500/30">
                    <Server class="w-3 h-3" />
                    {source.env}
                  </span>
                  <span class="text-xs px-1.5 py-0.5 bg-blue-900/50 text-blue-300 rounded animate-pulse">
                    {$_('messages.waitingApproval')}
                  </span>
                </div>
                <div class="text-xs text-gray-500 mt-1.5 ml-9">
                  <span class="font-mono">{data.tool_name || $_('messages.tool')}</span>
                  <span class="mx-1.5 text-gray-600">·</span>
                  <span class="font-mono">{source.sessionName}</span>
                </div>
              </div>
              <button
                class="flex items-center gap-1 text-xs text-gray-400 hover:text-white transition-colors flex-shrink-0 mt-1"
                onclick={() => openSession(data.session_id)}
              >
                <ExternalLink class="w-3 h-3" />
                {$_('messages.viewSession')}
              </button>
            </div>
            <pre class="text-xs text-gray-300 whitespace-pre-wrap break-all max-h-48 overflow-y-auto bg-gray-950/60 rounded p-3 border border-gray-800/60">{data.prompt}</pre>

            {#if data.request_id}
              <textarea
                bind:value={approvalComments[data.session_id]}
                placeholder={$_('messages.commentPlaceholder')}
                rows="2"
                class="w-full mt-3 px-3 py-2 rounded bg-gray-800/60 border border-gray-700/50 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-blue-600 resize-none"
              ></textarea>
              <div class="flex gap-2 mt-2">
                <button
                  class="px-4 py-1.5 rounded bg-green-700 hover:bg-green-600 text-white text-sm font-medium transition-colors"
                  onclick={() => decide(data, 'approve')}
                >
                  {$_('messages.approve')}
                </button>
                <button
                  class="px-4 py-1.5 rounded bg-red-700 hover:bg-red-600 text-white text-sm font-medium transition-colors"
                  onclick={() => decide(data, 'deny')}
                >
                  {$_('messages.deny')}
                </button>
              </div>
            {:else}
              <div class="mt-2 text-xs text-yellow-400">{$_('messages.missingRequestId')}</div>
            {/if}
          </div>
        {:else if item.kind === 'question'}
          {@const data = item.data}
          {@const source = sourceOf(data)}
          <div class="bg-gray-900 rounded-lg border border-gray-800 p-5 {isFresh(data.receivedAt) ? 'msg-flash' : ''}">
            <div class="flex items-start justify-between gap-3 mb-3">
              <div class="min-w-0">
                <div class="flex items-center gap-2 flex-wrap">
                  <span class="w-7 h-7 rounded-full bg-amber-600/40 flex items-center justify-center flex-shrink-0">
                    <HelpCircle class="w-4 h-4 text-amber-300" />
                  </span>
                  <span class="text-base font-semibold text-white truncate">{source.project}</span>
                  <span class="inline-flex items-center gap-1 px-2 py-0.5 rounded text-xs font-medium bg-indigo-500/15 text-indigo-300 border border-indigo-500/30">
                    <Server class="w-3 h-3" />
                    {source.env}
                  </span>
                  <span class="text-xs px-1.5 py-0.5 bg-amber-900/50 text-amber-400 rounded animate-pulse">
                    {$_('messages.waitingQuestion')}
                  </span>
                </div>
                <div class="text-xs text-gray-500 mt-1.5 ml-9">
                  <span class="font-mono">AskUserQuestion</span>
                  <span class="mx-1.5 text-gray-600">·</span>
                  <span class="font-mono">{source.sessionName}</span>
                </div>
              </div>
              <button
                class="flex items-center gap-1 text-xs text-gray-400 hover:text-white transition-colors flex-shrink-0 mt-1"
                onclick={() => openSession(data.session_id)}
              >
                <ExternalLink class="w-3 h-3" />
                {$_('messages.viewSession')}
              </button>
            </div>

            <div class="space-y-4">
              {#each data.questions as q, qIdx}
                <div class="space-y-1.5">
                  <div class="text-sm text-gray-200 font-medium">{q.question}</div>
                  {#if q.header}
                    <div class="text-xs text-gray-400">{q.header}</div>
                  {/if}
                  <div class="space-y-1">
                    {#each q.options as option}
                      <button
                        class="w-full text-left px-3 py-2 rounded border transition-colors {
                          questionAnswers[data.session_id]?.[qIdx] === option.label
                            ? 'bg-amber-700/30 border-amber-600 text-amber-200'
                            : 'bg-gray-800/50 border-gray-700/50 text-gray-300 hover:bg-gray-800 hover:border-gray-600'
                        }"
                        onclick={() => selectOption(data.session_id, qIdx, option.label)}
                      >
                        <div class="text-sm font-medium">{option.label}</div>
                        {#if option.description}
                          <div class="text-xs text-gray-400 mt-0.5">{option.description}</div>
                        {/if}
                      </button>
                    {/each}

                    <!-- 否决选项：选项都不对时，自由输入修正（与会话内卡片一致） -->
                    <button
                      class="w-full text-left px-3 py-2 rounded border transition-colors {
                        questionAnswers[data.session_id]?.[qIdx] === CORRECTION
                          ? 'bg-red-800/40 border-red-600 text-red-200'
                          : 'bg-gray-800/50 border-gray-700/50 text-gray-400 hover:bg-gray-800 hover:border-red-700/60'
                      }"
                      onclick={() => selectOption(data.session_id, qIdx, CORRECTION)}
                    >
                      <div class="text-sm font-medium">{$_('messages.correctionOption')}</div>
                    </button>

                    {#if questionAnswers[data.session_id]?.[qIdx] === CORRECTION}
                      <textarea
                        class="w-full px-3 py-2 rounded bg-gray-900 border border-red-700/50 text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-red-500 resize-y"
                        rows="3"
                        placeholder={$_('messages.correctionPlaceholder')}
                        value={correctionTexts[data.session_id]?.[qIdx] ?? ''}
                        oninput={(e) => {
                          correctionTexts = {
                            ...correctionTexts,
                            [data.session_id]: {
                              ...(correctionTexts[data.session_id] || {}),
                              [qIdx]: e.currentTarget.value,
                            },
                          }
                        }}
                      ></textarea>
                    {/if}
                  </div>
                </div>
              {/each}

              <button
                class="w-full px-4 py-2 rounded bg-amber-600 hover:bg-amber-500 text-white text-sm font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                onclick={() => submitAnswers(data)}
                disabled={answeredCount(data) < data.questions.length}
              >
                {$_('messages.submitAnswers')} ({answeredCount(data)}/{data.questions.length})
              </button>
            </div>
          </div>
        {:else}
          {@const r = item.data}
          {@const source = sourceFromSession(r.session_id, r.project_name)}
          {@const answers = parsedAnswers(r)}
          <div class="bg-gray-900 rounded-lg border border-gray-800 p-5">
            <div class="flex items-start justify-between gap-3 mb-3">
              <div class="min-w-0">
                <div class="flex items-center gap-2 flex-wrap">
                  <span class="w-7 h-7 rounded-full {r.kind === 'approval' ? 'bg-blue-600/25' : 'bg-amber-600/25'} flex items-center justify-center flex-shrink-0">
                    {#if r.kind === 'approval'}
                      <ShieldAlert class="w-4 h-4 text-blue-400/80" />
                    {:else}
                      <HelpCircle class="w-4 h-4 text-amber-400/80" />
                    {/if}
                  </span>
                  <span class="text-base font-semibold text-gray-300 truncate">{source.project}</span>
                  <span class="inline-flex items-center gap-1 px-2 py-0.5 rounded text-xs font-medium bg-indigo-500/15 text-indigo-300 border border-indigo-500/30">
                    <Server class="w-3 h-3" />
                    {source.env}
                  </span>
                  {#if r.decision === 'approved'}
                    <span class="text-xs px-1.5 py-0.5 bg-green-900/50 text-green-300 rounded">{$_('messages.approved')}</span>
                  {:else if r.decision === 'denied'}
                    <span class="text-xs px-1.5 py-0.5 bg-red-900/50 text-red-300 rounded">{$_('messages.denied')}</span>
                  {:else}
                    <span class="text-xs px-1.5 py-0.5 bg-amber-900/50 text-amber-300 rounded">{$_('messages.answered')}</span>
                  {/if}
                </div>
                <div class="text-xs text-gray-500 mt-1.5 ml-9">
                  <span class="font-mono">{r.kind === 'approval' ? (r.tool_name || $_('messages.tool')) : 'AskUserQuestion'}</span>
                  <span class="mx-1.5 text-gray-600">·</span>
                  <span class="font-mono">{source.sessionName}</span>
                  {#if r.decided_at}
                    <span class="mx-1.5 text-gray-600">·</span>
                    <span>{formatTime(r.decided_at)}</span>
                  {/if}
                </div>
              </div>
              <button
                class="flex items-center gap-1 text-xs text-gray-400 hover:text-white transition-colors flex-shrink-0 mt-1"
                onclick={() => openSession(r.session_id)}
              >
                <ExternalLink class="w-3 h-3" />
                {$_('messages.viewSession')}
              </button>
            </div>

            {#if answers}
              <div class="space-y-2 bg-gray-950/60 rounded p-3 border border-gray-800/60">
                {#each Object.entries(answers) as [question, answer]}
                  <div>
                    <div class="text-sm text-gray-300">{question}</div>
                    <div class="text-xs text-amber-300 mt-0.5">✓ {answer}</div>
                  </div>
                {/each}
              </div>
            {:else}
              <pre class="text-xs text-gray-400 whitespace-pre-wrap break-all max-h-48 overflow-y-auto bg-gray-950/60 rounded p-3 border border-gray-800/60">{r.summary}</pre>
            {/if}

            {#if r.comment}
              <div class="mt-2 text-xs text-gray-400 italic bg-gray-950/60 rounded p-2 border border-gray-800/60">“{r.comment}”</div>
            {/if}
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
