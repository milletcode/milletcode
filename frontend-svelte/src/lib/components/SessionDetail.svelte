<script lang="ts">
  import { onMount, onDestroy, tick } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { Terminal, Trash2 } from 'lucide-svelte'
  import { sessionMessages, sessions, setSessionMessages, fetchSessions, turnComplete } from '$lib/stores/session'
  import { projects, fetchProjects } from '$lib/stores/project'
  import {
    subscribeSession,
    unsubscribeSession,
    initWebSocket,
    getWSStatus,
  } from '$lib/wsBridge'
  import { sendWSMessage, subscribeWSStatus } from '$lib/wsPool'
  import ThreadView from './ThreadView.svelte'
  import DebugPanel from './DebugPanel.svelte'
  import MessageInput from './MessageInput.svelte'
  import StartAgentDialog from './StartAgentDialog.svelte'
  import FileBrowser from './FileBrowser.svelte'
  import PendingApprovalCard from './messages/PendingApprovalCard.svelte'
  import { hasPendingControl, hasPendingQuestion } from '$lib/stores/approval'
  import type { DisplayMessage } from '$lib/pure/messageFilter'
  import { filterAndBuildMessages, type ViewMode } from '$lib/pure/messageFilter'
  import { parseFlattenedHistory } from '$lib/pure/historyParser'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate }: Props = $props()

  let isLoading = $state(true)
  let error = $state<string | null>(null)
  let isDebugMode = $state(false)
  let showDebugPanel = $state(false)
  let showFileBrowser = $state(false)
  let autoScroll = $state(true)
  let gitStatus = $state<{ dirty: boolean; branch: string } | null>(null)
  let copied = $state<'web_id' | 'claude_id' | null>(null)
  let viewMode = $state<ViewMode>('detailed')

  /** Stop reasons that mean Claude finished its turn rather than pausing for a tool. */
  const TERMINAL_STOP_REASONS = new Set(['end_turn', 'stop_sequence', 'max_tokens'])

  // History pagination state
  let historyOffset = $state(0)
  let historyHasMore = $state(false)
  let isLoadingMore = $state(false)
  // 首页历史渲染后需要"落定式"滚到底（见 scrollToBottomSettle）
  let pendingSettleScroll = $state(false)
  const HISTORY_PAGE_SIZE = 50

  // The CLI ends an interrupted turn with a swallowed `result` line and an
  // "[Request interrupted by user]" user message — thread-based detection never
  // sees a terminal stop_reason. The user asked for the interrupt, so trust it:
  // lock streaming off until the next send starts a fresh turn.
  let interrupted = $state(false)

  // Build composite session key from params
  const sessionKey = $derived(
    params.org && params.project && params.agent && params.name
      ? `${params.org}/${params.project}/${params.agent}/${params.name}`
      : null
  )

  // Build project key for WebSocket
  const projectKey = $derived(
    params.org && params.project && params.agent
      ? `${params.org}/${params.project}/${params.agent}`
      : null
  )

  // Get current session
  const currentSession = $derived(
    sessionKey ? $sessions.find((s) => s.id === sessionKey) : undefined
  )

  // Get project for this session
  const project = $derived(
    currentSession?.project_id ? $projects.find((p) => p.id === currentSession.project_id) : undefined
  )

  // Terminal target: store lookup, falling back to the route params
  // (keyword/name) so the pane works before/even without a store hit.
  const terminalProjectId = $derived(
    project?.id ?? (params.org && params.project ? `${params.org}/${params.project}` : null)
  )

  // Get messages for this session
  const rawMessages = $derived(sessionKey ? $sessionMessages.get(sessionKey) || [] : [])

  // Get session status
  const sessionStatus = $derived(currentSession?.status)
  const st = $derived((sessionStatus || '').toLowerCase())
  const isActive = $derived(st === 'running' || st === 'waitingapproval' || st === 'starting')
  // Only disable input when explicitly stopped or error
  const isStopped = $derived(st === 'stopped' || st === 'error')

  // Determine if agent is actively streaming (not just running, but actively outputting)
  // This is used to show/hide the thinking indicator in compact/outline modes
  // Authoritative turn-end marker from the CLI's `result` line
  const turnEnded = $derived(sessionKey ? $turnComplete.get(sessionKey) === true : false)

  // A turn is in flight from the moment the user sends a regular message until
  // the CLI reports its result (or the process exits). Covers the startup
  // dead-window where the session status is still idle/starting and no content
  // has streamed yet — isAgentStreaming alone misses that gap.
  const turnInFlight = $derived(sessionKey ? $turnComplete.get(sessionKey) === false : false)

  const isAgentStreaming = $derived.by(() => {
    if (interrupted || turnEnded) return false

    // If session is not active (stopped/error), don't show streaming indicator
    if (!isActive) return false

    // The turn is over exactly when the thread ends on an assistant message that
    // finished. Anything else means work is in flight: a user message the agent has
    // not answered yet, or a tool result it is still acting on (tool results are
    // "user" messages too).
    const messages = rawMessages || []
    const last = messages[messages.length - 1]
    if (!last) return false
    if (last.role !== 'assistant') {
      // The CLI ends an interrupted turn with a "[Request interrupted by user]"
      // user line — a history reload lands on it and must not read as busy.
      if (typeof last.content === 'string' && last.content.includes('[Request interrupted by user]')) {
        return false
      }
      return true
    }

    // Messages replayed from history are marked done; live output reports why Claude
    // stopped, which is terminal only for end_turn/stop_sequence/max_tokens — a
    // missing reason or "tool_use" means the turn is still going.
    if (last.done === true) return false
    return !TERMINAL_STOP_REASONS.has(last.stopReason ?? '')
  })

  const agentBusy = $derived(turnInFlight || isAgentStreaming)

  // Filter and build display messages with requestAnimationFrame throttling
  let displayMessages = $state<DisplayMessage[]>([])
  let pendingRebuild: number | null = null

  function rebuildMessages() {
    displayMessages = filterAndBuildMessages(rawMessages, viewMode)
  }

  // Throttle rebuilds with requestAnimationFrame
  $effect(() => {
    void rawMessages // track changes
    void viewMode // track viewMode changes
    if (pendingRebuild === null) {
      pendingRebuild = requestAnimationFrame(() => {
        pendingRebuild = null
        rebuildMessages()
      })
    }
  })

  // Auto-scroll to bottom when new messages arrive
  $effect(() => {
    if (autoScroll && displayMessages.length > 0) {
      tick().then(() => {
        if (pendingSettleScroll) {
          pendingSettleScroll = false
          scrollToBottomSettle()
        } else {
          scrollToBottom()
        }
      })
    }
  })

  // Flag to distinguish programmatic scroll from user scroll
  let isProgrammaticScroll = false

  // Detect when user is manually scrolling to pause auto-scroll
  function handleScroll() {
    // Ignore scroll events triggered by our own scrollToBottom
    if (isProgrammaticScroll) return

    const container = document.getElementById('messages-container')
    if (!container) return

    // Check if user is near the bottom (within 100px threshold)
    const isNearBottom = container.scrollHeight - container.scrollTop - container.clientHeight < 100

    // Disable auto-scroll if user scrolled up to read history
    if (!isNearBottom) {
      autoScroll = false
    } else {
      // Re-enable auto-scroll when user returns to bottom
      autoScroll = true
    }
  }

  function scrollToBottom() {
    const container = document.getElementById('messages-container')
    if (container) {
      isProgrammaticScroll = true
      container.scrollTop = container.scrollHeight
      // Follow-up scroll to account for content-visibility re-layouts
      requestAnimationFrame(() => {
        if (container) {
          container.scrollTop = container.scrollHeight
        }
        // Reset flag after scroll events have fired
        requestAnimationFrame(() => {
          isProgrammaticScroll = false
        })
      })
    }
  }

  // 进入会话时的定位：ThreadView 用 content-visibility 懒渲染，消息真实高度
  // 分帧落定，一次性 scrollToBottom 会被后续布局顶离底部 —— 持续跟随滚动条
  // 直到高度稳定（或 1.5s 兜底超时）。
  function scrollToBottomSettle() {
    const container = document.getElementById('messages-container')
    if (!container) return
    isProgrammaticScroll = true
    let lastHeight = -1
    const startedAt = performance.now()
    const step = () => {
      container.scrollTop = container.scrollHeight
      const height = container.scrollHeight
      if (height !== lastHeight && performance.now() - startedAt < 1500) {
        lastHeight = height
        requestAnimationFrame(step)
      } else {
        isProgrammaticScroll = false
      }
    }
    requestAnimationFrame(step)
  }

  // 顶部哨兵进入视口即加载上一页历史（替代手动「加载更多」按钮）
  function historySentinelAction(node: HTMLElement) {
    const io = new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) loadMoreHistory()
    })
    io.observe(node)
    return {
      destroy() {
        io.disconnect()
      },
    }
  }

  onMount(async () => {
    if (!sessionKey || !projectKey) {
      error = 'Invalid session parameters'
      isLoading = false
      return
    }

    // Sessions reached via direct URL need the projects store populated
    // (project lookup, git status, terminal all depend on it).
    if ($projects.length === 0) {
      fetchProjects().catch(() => {})
    }

    // Check if debug mode
    try {
      const res = await fetch('/api/settings')
      const settings = await res.json()
      isDebugMode = settings.env === 'debug'
      if (isDebugMode) {
        showDebugPanel = true
      }
    } catch {}

    // Fetch git status
    if (project?.id) {
      try {
        const res = await fetch(`/api/projects/${project.id}/git-status`)
        if (res.ok) {
          const data = await res.json()
          if (data && !data.error) gitStatus = data
        }
      } catch {}
    }

    // Subscribe to WS status changes (for subscription tracking)
    subscribeWSStatus(projectKey, () => {})

    try {
      await loadSession()
    } catch {}
  })

  // 会话信息 + 历史 + WS 连接。失败只渲染错误与重试按钮（WS 超时、后端
  // 不可达等），由 loadSession 重新走完整加载。
  async function loadSession() {
    if (!sessionKey || !projectKey) return
    error = null
    isLoading = true
    try {
      // Fetch session info to get status
      const { apiFetch } = await import('$lib/api')
      const sessionRes = await apiFetch(`/api/sessions/${sessionKey}`)
      if (sessionRes.ok) {
        const sessionData = await sessionRes.json()
        if (sessionData) {
          // Update session in store
          sessions.update((current) => {
            const existing = current.find((s) => s.id === sessionKey)
            if (existing) {
              return current.map((s) => (s.id === sessionKey ? { ...s, ...sessionData } : s))
            }
            return [...current, sessionData]
          })
        }
      }

      // Fetch session history (first page)
      const res = await apiFetch(`/api/sessions/${sessionKey}/history?offset=0&limit=${HISTORY_PAGE_SIZE}`)
      if (!res.ok) {
        throw new Error(`Failed to fetch history: ${res.status}`)
      }
      const history = await res.json()
      const parsedMessages = parseFlattenedHistory(history.messages || [])
      setSessionMessages(sessionKey, parsedMessages)
      historyOffset = parsedMessages.length
      historyHasMore = history.has_more || false
      pendingSettleScroll = true

      // Initialize WebSocket connection
      initWebSocket(projectKey)

      // Wait for connection
      let attempts = 0
      while (getWSStatus(projectKey) !== 'connected' && attempts < 50) {
        await new Promise((r) => setTimeout(r, 100))
        attempts++
      }

      if (getWSStatus(projectKey) !== 'connected') {
        throw new Error('WebSocket connection timeout')
      }

      // Subscribe to session
      subscribeSession(sessionKey, projectKey)

      isLoading = false
    } catch (e) {
      error = e instanceof Error ? e.message : 'Unknown error'
      isLoading = false
    }
  }

  onDestroy(() => {
    if (pendingRebuild !== null) {
      cancelAnimationFrame(pendingRebuild)
    }
    if (sessionKey && projectKey) {
      unsubscribeSession(sessionKey, projectKey)
    }
  })

  async function handleCloseSession() {    if (!sessionKey) return
    sendWSMessage({ type: 'stop_session', session_id: sessionKey }, projectKey || undefined)
  }

  async function handleRefreshSession() {
    if (!sessionKey) return
    try {
      const res = await fetch(`/api/sessions/${sessionKey}`)
      if (res.ok) {
        const data = await res.json()
        if (data?.status) {
          // Update session status in store
          sessions.update((current) =>
            current.map((s) => (s.id === sessionKey ? { ...s, status: data.status, last_error: data.last_error ?? null } : s))
          )
        }
      }
    } catch {}
  }

  async function loadMoreHistory() {
    if (!sessionKey || isLoadingMore || !historyHasMore) return

    const container = document.getElementById('messages-container')
    if (!container) return

    // Record scroll position before loading
    const scrollHeightBefore = container.scrollHeight
    const scrollTopBefore = container.scrollTop

    isLoadingMore = true
    try {
      const res = await fetch(`/api/sessions/${sessionKey}/history?offset=${historyOffset}&limit=${HISTORY_PAGE_SIZE}`)
      if (res.ok) {
        const history = await res.json()
        const parsedMessages = parseFlattenedHistory(history.messages || [])

        if (parsedMessages.length > 0) {
          // Prepend older messages to existing ones
          const currentMessages = $sessionMessages.get(sessionKey) || []
          setSessionMessages(sessionKey, [...parsedMessages, ...currentMessages])
          historyOffset += parsedMessages.length
          historyHasMore = history.has_more || false

          // Wait for DOM to update, then restore scroll position
          await tick()
          requestAnimationFrame(() => {
            const scrollHeightAfter = container.scrollHeight
            const heightDiff = scrollHeightAfter - scrollHeightBefore
            // Adjust scrollTop to keep the same content visible
            container.scrollTop = scrollTopBefore + heightDiff
            // 视口还没被历史填满时继续接力加载：IntersectionObserver 对
            // "哨兵持续可见"不会重新触发，只能手动续。
            if (historyHasMore && container.scrollHeight <= container.clientHeight + 50) {
              loadMoreHistory()
            }
          })
        }
      }
    } catch (err) {
      console.error('Failed to load more history:', err)
    } finally {
      isLoadingMore = false
    }
  }

  async function copyToClipboard(text: string, type: 'web_id' | 'claude_id') {
    try {
      await navigator.clipboard.writeText(text)
      copied = type
      setTimeout(() => {
        if (copied === type) copied = null
      }, 2000)
    } catch {}
  }

  function statusDotColor(status: string | undefined): string {
    switch ((status || '').toLowerCase()) {
      case 'running':
      case 'starting':
        return 'bg-green-500'
      case 'waitingapproval':
        return 'bg-amber-500'
      case 'stopped':
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-gray-500'
    }
  }

  function handleOpenTerminal() {
    if (!terminalProjectId) return
    const [org, ...rest] = terminalProjectId.split('/')
    const name = rest.join('/')
    if (!org || !name) return
    window.open(
      `/terminal/${encodeURIComponent(org)}/${encodeURIComponent(name)}`,
      '_blank',
      'width=1100,height=680',
    )
  }

  async function handleClearHistory() {
    if (!sessionKey) return
    if (!confirm('确定要清空该会话的历史记录吗？此操作不可恢复。')) return

    try {
      const { apiFetch } = await import('$lib/api')
      const res = await apiFetch(`/api/sessions/${sessionKey}/full`, {
        method: 'DELETE',
      })
      if (res.ok) {
        // Refresh sessions list from server, then navigate back
        await fetchSessions()
        navigate('/sessions')
      } else {
        error = 'Failed to clear history'
      }
    } catch (err) {
      console.error('Failed to clear history:', err)
      error = 'Failed to clear history'
    }
  }

  // Session env quick-switcher: {agent}/{env} sessions of this project
  interface MenuSession {
    id: string // keyword/project/agent/env
    agent: string
    env: string
    updateTime: number
  }
  let showSessionMenu = $state(false)
  let sessionMenuLoading = $state(false)
  let menuSessions = $state<MenuSession[]>([])
  let sessionMenuWrap = $state<HTMLDivElement>()

  function toggleSessionMenu() {
    showSessionMenu = !showSessionMenu
    if (showSessionMenu) loadMenuSessions()
  }

  function handleWindowClick(e: MouseEvent) {
    if (!showSessionMenu) return
    const t = e.target as Node | null
    if (sessionMenuWrap && t && !sessionMenuWrap.contains(t)) showSessionMenu = false
  }

  async function loadMenuSessions() {
    const pid = project?.id
    if (!pid) return
    sessionMenuLoading = true
    try {
      const { apiFetch } = await import('$lib/api')
      const res = await apiFetch(`/api/projects/${pid}/sessions`)
      if (res.ok) {
        const data = await res.json()
        menuSessions = ((Array.isArray(data) ? data : []) as { id?: string; update_time?: number }[])
          .map((s) => {
            const parts = (s.id || '').split('/')
            if (parts.length < 4) return null
            return {
              id: s.id as string,
              agent: parts[2],
              env: parts.slice(3).join('/'),
              updateTime: s.update_time || 0,
            }
          })
          .filter((s): s is MenuSession => s !== null)
          .sort((a, b) => {
            // current session first, then most recently updated
            const ca = a.id === sessionKey ? 0 : 1
            const cb = b.id === sessionKey ? 0 : 1
            if (ca !== cb) return ca - cb
            return b.updateTime - a.updateTime
          })
      }
    } catch (err) {
      console.error('Failed to load project sessions:', err)
    } finally {
      sessionMenuLoading = false
    }
  }

  function switchSessionEnv(e: MouseEvent, id: string) {
    showSessionMenu = false
    // alt on Windows/Linux; cmd/ctrl is the habitual new-tab modifier on macOS
    if (e.altKey || e.metaKey || e.ctrlKey) {
      window.open(`/sessions/${id}`, '_blank')
    } else if (id !== sessionKey) {
      navigate(`/sessions/${id}`)
    }
  }
</script>

<svelte:window onclick={handleWindowClick} />

<div class="h-full flex flex-col">
  <!-- Header status bar -->
  <div class="flex items-center gap-3 mb-4 flex-wrap">
    <!-- Back button -->
    <button
      onclick={() => navigate('/sessions')}
      class="text-sm text-gray-400 hover:text-white transition-colors"
    >
      &larr; {$_('sessions.title')}
    </button>

    <!-- Project name badge: links to the project page -->
    {#if project}
      <button
        onclick={() => navigate(`/projects/${project.id}`)}
        class="text-xs text-gray-400 bg-gray-800 px-2 py-0.5 rounded hover:text-white hover:bg-gray-700 transition-colors"
        title={project.id}
      >
        {project.name}
      </button>
    {/if}

    <!-- Agent id -->
    {#if currentSession?.agent_id}
      <span class="text-xs text-gray-500">{currentSession.agent_id}</span>
      <span class="text-xs text-gray-500">·</span>
    {/if}

    <!-- Status indicator -->
    <span class="flex items-center gap-1.5 text-xs text-gray-500">
      <span class="w-2 h-2 rounded-full {statusDotColor(sessionStatus)}"></span>
      {sessionStatus || 'unknown'}
    </span>

    <!-- View mode toggle (compact/outline/detailed) -->
    <button
      onclick={() => {
        viewMode = viewMode === 'compact' ? 'outline' : viewMode === 'outline' ? 'detailed' : 'compact'
      }}
      class="text-xs px-2 py-0.5 rounded border transition-colors {
        viewMode === 'compact'
          ? 'text-purple-400 border-purple-700/50 bg-purple-900/20 hover:bg-purple-900/40'
          : viewMode === 'outline'
          ? 'text-blue-400 border-blue-700/50 bg-blue-900/20 hover:bg-blue-900/40'
          : 'text-gray-400 border-gray-700/50 bg-gray-900/20 hover:bg-gray-900/40'
      }"
      title={viewMode === 'compact' ? '缩略模式（隐藏 thinking）' : viewMode === 'outline' ? '大纲模式' : '详细模式'}
    >
      {viewMode === 'compact' ? '缩略' : viewMode === 'outline' ? '大纲' : '详细'}
    </button>

    <!-- Git status indicator -->
    {#if gitStatus}
      <span
        class="w-2.5 h-2.5 rounded-full shrink-0 {gitStatus.dirty ? 'bg-amber-500' : 'bg-green-500'}"
        title="{gitStatus.dirty ? $_('sessions.gitDirty') : $_('sessions.gitClean')} ({gitStatus.branch})"
      ></span>
    {/if}

    <!-- File viewer icon -->
    <button
      onclick={() => (showFileBrowser = true)}
      class="text-gray-500 hover:text-gray-300 transition-colors"
      title={$_('sessions.viewFiles')}
    >
      <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
      </svg>
    </button>

    <!-- Terminal icon -->
    <button
      onclick={handleOpenTerminal}
      class="text-gray-500 hover:text-gray-300 transition-colors"
      title="Terminal"
    >
      <Terminal class="w-4 h-4" />
    </button>

    <!-- Right side: session info + actions -->
    <div class="ml-auto flex items-center gap-2">
      <!-- Session key: click for {agent}/{env} quick switcher -->
      {#if sessionKey}
        <div class="relative" bind:this={sessionMenuWrap}>
          <button
            onclick={toggleSessionMenu}
            class="text-xs text-gray-500 bg-gray-800 px-2 py-0.5 rounded font-mono hover:text-white hover:bg-gray-700 transition-colors"
            title={$_('envPanel.openNewTabHint')}
          >
            {sessionKey}
          </button>
          {#if showSessionMenu}
            <div class="absolute right-0 top-full mt-1 w-56 bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50">
              <div class="max-h-60 overflow-y-auto">
                {#if sessionMenuLoading}
                  <div class="text-xs text-gray-500 text-center py-3">...</div>
                {:else if menuSessions.length === 0}
                  <div class="text-xs text-gray-600 text-center py-3">{$_('common.noData')}</div>
                {:else}
                  {#each menuSessions as s (s.id)}
                    <button
                      onclick={(e) => switchSessionEnv(e, s.id)}
                      class="w-full px-3 py-1.5 text-left text-xs font-mono truncate transition-colors hover:bg-gray-700 {s.id === sessionKey ? 'text-blue-400 bg-blue-900/20' : 'text-gray-300'}"
                    >
                      {s.agent}/{s.env}
                    </button>
                  {/each}
                {/if}
              </div>
            </div>
          {/if}
        </div>
      {/if}

      <!-- Copy session info button -->
      <button
        onclick={() => copyToClipboard(currentSession?.claude_session_id || project?.path || sessionKey || '', 'web_id')}
        class="text-xs text-gray-500 hover:text-white transition-colors"
        title={currentSession?.claude_session_id ? "Copy Claude Session ID" : "Copy project path"}
      >
        {#if copied === 'web_id'}
          <svg class="w-4 h-4 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
          </svg>
        {:else}
          <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
          </svg>
        {/if}
      </button>

      <!-- Stop/Clear button -->
      {#if isActive}
        <!-- Stop button (when active) -->
        <button
          onclick={handleCloseSession}
          class="ml-1 p-1.5 text-amber-400 hover:text-amber-300 hover:bg-amber-900/30 rounded transition-colors"
          title={$_('sessions.stop')}
        >
          <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <rect x="6" y="6" width="12" height="12" stroke-width="2" />
          </svg>
        </button>
      {:else if isStopped}
        <!-- Clear history button (when stopped) -->
        <button
          onclick={handleClearHistory}
          class="ml-1 p-1.5 text-red-400 hover:text-red-300 hover:bg-red-900/30 rounded transition-colors"
          title="清空会话记录"
        >
          <Trash2 class="w-4 h-4" />
        </button>
      {/if}

      <!-- Refresh button (when stopped) -->
      {#if isStopped}
        <button
          onclick={handleRefreshSession}
          class="ml-1 p-1.5 text-gray-400 hover:text-gray-300 hover:bg-gray-800/50 rounded transition-colors"
          title={$_('sessions.refresh')}
        >
          <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
          </svg>
        </button>
      {/if}
    </div>
  </div>

  {#if isLoading}
    <div class="flex-1 flex items-center justify-center">
      <div class="text-gray-500">{$_('common.loading')}</div>
    </div>
  {:else if error}
    <div class="flex-1 flex items-center justify-center">
      <div class="text-center">
        <div class="text-red-500 mb-3">{error}</div>
        <button
          onclick={() => loadSession()}
          class="px-4 py-1.5 bg-gray-800 hover:bg-gray-700 border border-gray-700 rounded-lg text-sm text-gray-300 transition-colors"
        >
          {$_('connection.retry')}
        </button>
      </div>
    </div>
  {:else}
    <!-- 崩溃原因横幅：record.last_error 落库后刷新也可见；会话恢复运行即消失 -->
    {#if st === 'error' && currentSession?.last_error}
      <div class="mx-4 mt-2 px-3 py-2 rounded-lg border border-red-800/60 bg-red-950/40">
        <div class="flex items-center gap-2 text-xs font-medium text-red-400 mb-1">
          <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
          </svg>
          {$_('sessions.lastError')}
        </div>
        <pre class="text-xs text-red-300/90 whitespace-pre-wrap break-all max-h-40 overflow-auto font-mono">{currentSession.last_error}</pre>
      </div>
    {/if}
    <div class="flex-1 overflow-auto pb-4" id="messages-container" onscroll={handleScroll}>
      <!-- 顶部哨兵：滚到顶自动加载更早的历史 -->
      {#if historyHasMore}
        <div use:historySentinelAction class="flex justify-center py-3 text-xs text-gray-500">
          {#if isLoadingMore}{$_('common.loading')}{/if}
        </div>
      {/if}
      <ThreadView messages={displayMessages} {viewMode} sessionId={sessionKey || ''} isStreaming={isAgentStreaming} />
    </div>

    <!-- Chat input or Start Agent dialog -->
    <div class="border-t border-gray-800 pt-4 relative">
      {#if isStopped}
        <StartAgentDialog
          sessionId={sessionKey || ''}
          onStart={handleRefreshSession}
        />
      {:else}
        {#if sessionKey}
          <PendingApprovalCard sessionId={sessionKey} />
        {/if}
        <MessageInput
          sessionId={sessionKey || ''}
          sessionKey={sessionKey || ''}
          projectKey={projectKey || ''}
          projectId={project?.id || ''}
          disabled={sessionKey ? $hasPendingControl(sessionKey) || $hasPendingQuestion(sessionKey) : false}
          busy={agentBusy}
          oninterrupt={() => (interrupted = true)}
          onsent={() => (interrupted = false)}
        />
        <!-- Status line: absolutely positioned into <main>'s existing p-6
             bottom padding — zero flow height, so the input area keeps its
             original size in both busy and idle states -->
        <div class="absolute left-1 top-full h-6 flex items-center text-xs text-gray-400 pointer-events-none">
          {#if agentBusy}
            <div class="flex items-center gap-2">
              <span class="w-1.5 h-1.5 rounded-full bg-blue-500 animate-pulse shrink-0"></span>
              <span>{$_('sessions.agentWorking')}<span class="busy-dots"><span>.</span><span>.</span><span>.</span></span></span>
            </div>
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  {#if isDebugMode}
    <DebugPanel
      sessionKey={sessionKey || ''}
      isOpen={showDebugPanel}
    />
  {/if}

  {#if showFileBrowser && sessionKey}
    <FileBrowser sessionKey={sessionKey} onclose={() => (showFileBrowser = false)} />
  {/if}
</div>

<style>
  /* Stepped "1 → 2 → 3 dots" loop: second 1 shows one dot, second 2 two,
     second 3 three, then the cycle restarts from one */
  .busy-dots span {
    opacity: 0;
  }
  .busy-dots span:nth-child(1) {
    opacity: 1;
  }
  .busy-dots span:nth-child(2) {
    animation: busy-dot-2 3s infinite;
  }
  .busy-dots span:nth-child(3) {
    animation: busy-dot-3 3s infinite;
  }
  @keyframes busy-dot-2 {
    0%, 33.3% { opacity: 0; }
    33.4%, 100% { opacity: 1; }
  }
  @keyframes busy-dot-3 {
    0%, 66.6% { opacity: 0; }
    66.7%, 100% { opacity: 1; }
  }
</style>
