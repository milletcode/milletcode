<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { wsSessionMessages } from '$lib/stores/session'
  import debugIcon from '../../assets/debug-icon.svg'

  interface Props {
    sessionKey: string
    isOpen: boolean
  }

  let { sessionKey, isOpen }: Props = $props()

  let autoScroll = $state(true)
  let isMinimized = $state(true)

  // Window position and size
  let pos = $state({ x: 80, y: 60 })
  let size = $state({ w: 600, h: 500 })

  // Drag state
  let isDragging = false
  let dragOffset = { x: 0, y: 0 }

  // Resize state
  let isResizing = false
  let resizeStart = { x: 0, y: 0, w: 0, h: 0 }

  let scrollEl: HTMLDivElement

  // Read messages directly from the WS-only store — no HTTP history
  const messages = $derived(sessionKey ? $wsSessionMessages.get(sessionKey) || [] : [])

  // --- Drag handlers ---

  function onDocMouseMove(e: MouseEvent) {
    if (isDragging) {
      pos = {
        x: e.clientX - dragOffset.x,
        y: e.clientY - dragOffset.y
      }
    }
    if (isResizing) {
      const dx = e.clientX - resizeStart.x
      const dy = e.clientY - resizeStart.y
      size = {
        w: Math.max(400, resizeStart.w + dx),
        h: Math.max(300, resizeStart.h + dy)
      }
    }
  }

  function onDocMouseUp() {
    if (isDragging) {
      isDragging = false
      try {
        localStorage.setItem('debug-panel-pos', JSON.stringify(pos))
      } catch {}
    }
    if (isResizing) {
      isResizing = false
    }
  }

  function handleDragStart(e: MouseEvent) {
    if ((e.target as HTMLElement).closest('button, input, label, .no-drag')) return
    isDragging = true
    dragOffset = {
      x: e.clientX - pos.x,
      y: e.clientY - pos.y
    }
    e.preventDefault()
  }

  function handleResizeStart(e: MouseEvent) {
    isResizing = true
    resizeStart = {
      x: e.clientX,
      y: e.clientY,
      w: size.w,
      h: size.h
    }
    e.preventDefault()
    e.stopPropagation()
  }

  // --- Lifecycle ---

  // Load saved position on first mount
  try {
    const saved = localStorage.getItem('debug-panel-pos')
    if (saved) {
      const parsed = JSON.parse(saved)
      if (parsed.x !== undefined) pos.x = parsed.x
      if (parsed.y !== undefined) pos.y = parsed.y
    }
  } catch {}

  onMount(() => {
    document.addEventListener('mousemove', onDocMouseMove)
    document.addEventListener('mouseup', onDocMouseUp)
  })

  onDestroy(() => {
    document.removeEventListener('mousemove', onDocMouseMove)
    document.removeEventListener('mouseup', onDocMouseUp)
  })

  // Auto-scroll when new messages arrive
  $effect(() => {
    if (autoScroll && scrollEl && messages.length > 0) {
      scrollEl.scrollTop = scrollEl.scrollHeight
    }
  })

  function formatTimestamp(ts: number | undefined): string {
    if (!ts) return ''
    const date = new Date(ts)
    return date.toLocaleString('zh-CN', {
      year: 'numeric', month: '2-digit', day: '2-digit',
      hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false
    })
  }

  function copyToClipboard(text: string) {
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text)
    } else {
      const textArea = document.createElement('textarea')
      textArea.value = text
      textArea.style.position = 'fixed'
      textArea.style.left = '-999999px'
      document.body.appendChild(textArea)
      textArea.select()
      try { document.execCommand('copy') } catch {}
      document.body.removeChild(textArea)
    }
  }
</script>

{#if isOpen}
  {#if isMinimized}
    <!-- Minimized trapezoid on left side -->
    <button
      onclick={() => isMinimized = false}
      class="fixed left-0 top-1/2 -translate-y-1/2 w-5 h-12 bg-gray-800 border border-gray-700 hover:bg-gray-700 transition-colors shadow-lg z-50 flex items-center justify-center overflow-hidden"
      style="clip-path: polygon(0 0, 100% 20%, 100% 80%, 0 100%);"
      title="展开调试面板"
    >
      <img src={debugIcon} alt="Debug" class="w-3 h-3 object-contain opacity-80" />
    </button>
  {:else}
  <div
    class="fixed bg-gray-900 border border-gray-700 rounded-xl shadow-2xl flex flex-col"
    style="left: {pos.x}px; top: {pos.y}px; width: {size.w}px; height: {size.h}px; z-index: 100;"
  >
    <!-- Header (drag handle) -->
    <div
      class="flex items-center justify-between px-4 py-2.5 border-b border-gray-800 cursor-grab active:cursor-grabbing select-none bg-gray-900"
      onmousedown={handleDragStart}
    >
      <div class="flex-1 min-w-0">
        <h3 class="text-xs font-semibold text-gray-300 truncate">
          Session Log
        </h3>
        <p class="text-[10px] text-gray-500 truncate">{sessionKey} · {messages.length} msgs</p>
      </div>
      <div class="flex items-center gap-2">
        <button
          onclick={() => autoScroll = !autoScroll}
          class="text-xs px-2 py-0.5 rounded transition-colors {autoScroll ? 'bg-blue-600 text-white' : 'bg-gray-700 text-gray-400 hover:text-white'}"
        >
          {autoScroll ? 'Auto-scroll ON' : 'Auto-scroll OFF'}
        </button>
        <button
          onclick={() => isMinimized = true}
          class="text-gray-500 hover:text-white transition-colors"
          title="最小化"
        >
          <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" class="w-4 h-4">
            <path fill-rule="evenodd" d="M5.47 5.47a.75.75 0 011.06 0L12 10.94l5.47-5.47a.75.75 0 111.06 1.06L13.06 12l5.47 5.47a.75.75 0 11-1.06 1.06L12 13.06l-5.47 5.47a.75.75 0 01-1.06-1.06L10.94 12 5.47 6.53a.75.75 0 010-1.06z" clip-rule="evenodd" />
          </svg>
        </button>
      </div>
    </div>

    <!-- Content -->
    <div
      bind:this={scrollEl}
      class="flex-1 overflow-auto p-3 space-y-2 text-xs font-mono"
    >
      {#if messages.length === 0}
        <p class="text-gray-500">Waiting for WebSocket messages...</p>
      {:else}
        {#each messages as msg}
          <div class="rounded-lg p-3 bg-gray-800/50 border border-gray-700/50 hover:bg-gray-800/70 transition-colors">
            <div class="flex items-center justify-between mb-1">
              <div class="flex items-center gap-2">
                <span class="{msg.role === 'user' ? 'text-blue-400' : 'text-green-400'} text-[10px] uppercase font-semibold">{msg.role}</span>
                {#if msg.type}
                  <span class="text-gray-500 text-[10px]">{msg.type}</span>
                {/if}
                {#if msg.uuid}
                  <span class="text-gray-600 text-[10px] font-mono">{msg.uuid.slice(0, 8)}</span>
                {/if}
              </div>
              <div class="flex items-center gap-2">
                <span class="text-gray-500 text-[10px]">{formatTimestamp(msg.timestamp)}</span>
                <button
                  onclick={() => copyToClipboard(JSON.stringify(msg, null, 2))}
                  class="text-gray-500 hover:text-gray-300 transition-colors"
                  title="复制 JSON"
                >
                  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="currentColor" class="w-3 h-3">
                    <path d="M7 3.5A1.5 1.5 0 018.5 2h3.879a1.5 1.5 0 011.06.44l3.122 3.12A1.5 1.5 0 0117 6.622V12.5a1.5 1.5 0 01-1.5 1.5h-1v-3.379a3 3 0 00-.879-2.121L9.5 5.379A3 3 0 007.378 4.5H7v-1z" />
                    <path d="M4.5 6A1.5 1.5 0 003 7.5v9A1.5 1.5 0 004.5 18h7a1.5 1.5 0 001.5-1.5v-5.879a1.5 1.5 0 00-.44-1.06L9.44 6.439A1.5 1.5 0 008.378 6H4.5z" />
                  </svg>
                </button>
              </div>
            </div>
            <pre class="text-gray-300 whitespace-pre-wrap break-words">{JSON.stringify(msg, null, 2)}</pre>
          </div>
        {/each}
      {/if}
    </div>

    <!-- Resize handle -->
    <div
      class="absolute bottom-0 right-0 w-4 h-4 cursor-se-resize bg-gray-500/20 hover:bg-gray-500/40"
      onmousedown={handleResizeStart}
      style="clip-path: polygon(100% 0, 100% 100%, 0 100%);"
      role="button"
      aria-label="Resize"
    ></div>
  </div>
  {/if}
{/if}
