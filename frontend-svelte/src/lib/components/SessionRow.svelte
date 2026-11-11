<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { Play, RefreshCw, StopCircle, Trash2, Unlink } from 'lucide-svelte'
  import type { Session } from '$lib/stores/session'
  import { unsubscribeSession, isSessionSubscribed } from '$lib/wsPool'

  interface Props {
    session: Session
    navigate: (path: string) => void
    resumingId?: string | null
    onResume?: (session: Session) => void
    onStop?: (session: Session) => void
    onDelete?: (session: Session) => void
  }

  let { session, navigate, resumingId = null, onResume, onStop, onDelete }: Props = $props()

  const isResuming = $derived(resumingId === session.id)
  const isRunning = $derived(session.status === 'running' || session.status === 'waitingapproval' || session.status === 'starting')

  // Parse session.id (format: org/project/agent-type/session-name)
  const parts = $derived(session.id.split('/'))
  const org = $derived(parts[0] || '')
  const projectName = $derived(parts[1] || '')
  const agentType = $derived(parts[2] || '')
  const sessionName = $derived(parts[3] || '')

  // Check if this session is subscribed (for showing Unwatch button)
  const isSubscribedSession = $derived.by(() => {
    return isSessionSubscribed(session.id)
  })

  // One colour per letter, no sharing. Searched over the sRGB gamut and verified
  // with CIEDE2000: every pair of these 26 sits at least ΔE00 9.8 apart as
  // rendered on the app's dark background, with ≥3.4:1 contrast for the label.
  // 'd' is pinned to indigo so the ubiquitous "default" session keeps its hue.
  // Re-run the search rather than hand-editing if these ever need to change.
  const LETTER_COLORS: Record<string, [number, number, number]> = {
    a: [84, 95, 30],
    b: [16, 95, 50],
    c: [192, 65, 42],
    d: [240, 70, 34],
    e: [316, 95, 50],
    f: [352, 55, 30],
    g: [40, 65, 30],
    h: [164, 55, 30],
    i: [212, 95, 42],
    j: [308, 95, 30],
    k: [340, 95, 46],
    l: [20, 85, 34],
    m: [272, 95, 50],
    n: [192, 55, 30],
    o: [52, 75, 34],
    p: [32, 85, 42],
    q: [204, 95, 46],
    r: [120, 55, 30],
    s: [152, 75, 34],
    t: [336, 55, 42],
    u: [352, 85, 38],
    v: [68, 55, 30],
    w: [236, 55, 50],
    x: [296, 75, 46],
    y: [280, 55, 30],
    z: [272, 85, 38],
  }

  function getSessionNameStyle(name: string): string {
    const color = name ? LETTER_COLORS[name[0].toLowerCase()] : undefined
    if (!color) {
      return 'background-color: hsl(220 8% 34%); color: hsl(220 10% 93%); border-color: hsl(220 8% 52%)'
    }
    const [h, s, l] = color
    return `background-color: hsl(${h} ${s}% ${l}%); color: hsl(${h} 30% 99%); border-color: hsl(${h} 65% 66%)`
  }

  function getStatusColor(status: string): string {
    switch (status.toLowerCase()) {
      case 'running':
      case 'starting':
      case 'waitingapproval':
        return 'bg-green-500'
      case 'idle':
        return 'bg-blue-500'
      case 'stopped':
        return 'bg-gray-500'
      case 'error':
        return 'bg-red-500'
      case 'complete':
        return 'bg-gray-500'
      default:
        return 'bg-gray-600'
    }
  }

  function getStatusText(status: string): string {
    switch (status.toLowerCase()) {
      case 'running':
        return $_('status.running')
      case 'starting':
        return $_('status.starting')
      case 'waitingapproval':
        return $_('status.waitingapproval')
      case 'idle':
        return $_('status.idle')
      case 'stopped':
        return $_('status.stopped')
      case 'error':
        return $_('status.error')
      case 'complete':
        return $_('status.complete')
      default:
        return status
    }
  }

  function handleClick(e: MouseEvent | KeyboardEvent) {
    if (fromInnerControl(e)) return
    // alt / ctrl / cmd + click mirrors the browser convention for <a target="_blank">
    if (e.altKey || e.ctrlKey || e.metaKey) {
      openInNewTab()
      return
    }
    // Navigate to session detail
    const parts = session.id.split('/')
    if (parts.length === 4) {
      navigate(`/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}`)
    }
  }

  // Row-internal action buttons (stop/resume/delete/unwatch) must not trigger
  // row navigation — their click handlers already stop propagation, this
  // covers pointer/keyboard events that bubble before the click.
  function fromInnerControl(e: Event): boolean {
    return e.target instanceof HTMLElement && e.target.closest('button') !== null
  }

  function sessionUrl(): string | null {
    const parts = session.id.split('/')
    return parts.length === 4 ? `/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}` : null
  }

  function openInNewTab() {
    const url = sessionUrl()
    // 同源页面无 tabnabbing 风险；noopener/置空 opener 会让 Chromium 把
    // 新标签放到后台，这里显式 focus 保证前台打开
    if (url) window.open(url, '_blank')?.focus()
  }

  function handleResume(e: Event) {
    e.stopPropagation()
    if (onResume) {
      onResume(session)
    }
  }

  function handleStop(e: Event) {
    e.stopPropagation()
    if (onStop) {
      onStop(session)
    }
  }

  function handleDelete(e: Event) {
    e.stopPropagation()
    if (onDelete) {
      onDelete(session)
    }
  }

  function handleUnwatch(e: Event) {
    e.stopPropagation()
    if (session.session_key) {
      const parts = session.session_key.split('/')
      if (parts.length === 4) {
        unsubscribeSession(session.id, parts.slice(0, 2).join('/'))
      }
    }
  }
</script>

<div
  class="px-6 py-4 hover:bg-gray-800/50 transition-colors cursor-pointer select-none"
  onclick={handleClick}
  onkeydown={(e) => {
    if (fromInnerControl(e)) return
    if (e.key === 'Enter' && (e.altKey || e.ctrlKey || e.metaKey)) {
      openInNewTab()
    } else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      handleClick(e)
    }
  }}
  role="button"
  tabindex="0"
>
  <div class="flex items-center justify-between">
    <div class="flex-1 min-w-0">
      <!-- 第一行：项目名称（突出）+ 标签 -->
      <div class="flex items-center gap-2 mb-1.5">
        <span class="text-base font-semibold text-white truncate">
          {projectName}
        </span>
        <span class="text-xs text-gray-400 bg-gray-800 px-2 py-0.5 rounded">
          {org}
        </span>
        <span class="text-xs text-blue-400 bg-blue-900/20 px-2 py-0.5 rounded">
          {agentType}
        </span>
        <span class="flex items-center gap-1.5 text-xs text-gray-400 bg-gray-800 px-2 py-0.5 rounded">
          <span class="w-1.5 h-1.5 rounded-full {getStatusColor(session.status)}"></span>
          {getStatusText(session.status)}
        </span>
      </div>
      <!-- 第二行：会话名称标签 + prompt -->
      <div class="flex items-center gap-2">
        <span
          class="text-xs font-semibold px-2 py-0.5 rounded font-mono border"
          style={getSessionNameStyle(sessionName)}
        >
          {sessionName}
        </span>
        {#if session.prompt}
          <span class="text-sm text-gray-400 truncate">{session.prompt}</span>
        {/if}
      </div>
    </div>
    <div class="flex items-center gap-2 ml-4">
      {#if isSubscribedSession}
        <button
          onclick={handleUnwatch}
          class="p-2 rounded hover:bg-gray-700 transition-colors text-blue-500"
          title={$_('sessions.unwatch')}
        >
          <Unlink class="w-4 h-4" />
        </button>
      {/if}

      {#if isRunning}
        <button
          onclick={handleStop}
          class="p-2 rounded hover:bg-gray-700 transition-colors text-amber-500"
          title={$_('sessions.stop')}
        >
          <StopCircle class="w-4 h-4" />
        </button>
      {:else}
        <button
          onclick={handleResume}
          disabled={isResuming}
          class="p-2 rounded hover:bg-gray-700 transition-colors text-green-500 disabled:opacity-50"
          title={$_('sessions.start')}
        >
          {#if isResuming}
            <RefreshCw class="w-4 h-4 animate-spin" />
          {:else}
            <Play class="w-4 h-4" />
          {/if}
        </button>

        <button
          onclick={handleDelete}
          class="p-2 rounded hover:bg-gray-700 transition-colors text-red-500"
          title={$_('sessions.delete')}
        >
          <Trash2 class="w-4 h-4" />
        </button>
      {/if}
    </div>
  </div>
</div>
