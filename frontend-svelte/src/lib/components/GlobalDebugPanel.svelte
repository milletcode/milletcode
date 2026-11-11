<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import SystemLogWindow from './debug/SystemLogWindow.svelte'
  import TextFormatterWindow from './debug/TextFormatterWindow.svelte'
  import DebugToggle from './debug/DebugToggle.svelte'
  import debugIcon from '../../assets/debug-icon.svg'
  import { apiFetch } from '$lib/api'

  interface ClaudeSession {
    pid: string
    session_uuid: string
    host: string
    org: string
    project: string
    agent: string
    session: string
    command: string
  }

  interface PsData {
    entries: ClaudeSession[]
    raw: string
  }

  interface SystemLogWindowData {
    logPath: string
    lines: string[]
    loading: boolean
    pos: { x: number; y: number }
    size: { w: number; h: number }
    autoScroll: boolean
    zIndex: number
  }

  interface TextFormatterWindowData {
    content: string
    pos: { x: number; y: number }
    size: { w: number; h: number }
    zIndex: number
  }

  const STORAGE_KEY = 'debug-panel-pos'
  const BUTTON_STORAGE_KEY = 'debug-button-pos'

  let env = $state<string | null>(null)
  let open = $state(false)
  // 自定义图标（/api/config/debug-icon）加载失败时落回内置开源 SVG
  let iconFallback = $state(false)
  let psData = $state<PsData | null>(null)
  let systemLogWindow = $state<SystemLogWindowData | null>(null)
  let textFormatterWindow = $state<TextFormatterWindowData | null>(null)
  let topZIndex = $state(75)

  // Button position
  let buttonPos = $state<{ x: number; y: number }>({ x: 12, y: -1 })
  let buttonRef: HTMLButtonElement
  let buttonDragging = false
  let buttonDragStart = { startX: 0, startY: 0, origX: 0, origY: 0 }

  // Panel position
  let panelPos = $state<{ x: number; y: number }>({ x: 16, y: -1 })
  let panelRef: HTMLDivElement
  let panelDragging = false
  let panelDragStart = { startX: 0, startY: 0, origX: 0, origY: 0 }

  let psInterval: ReturnType<typeof setInterval>
  let logInterval: ReturnType<typeof setInterval>

  function loadPosition(): { x: number; y: number } | null {
    try {
      const raw = localStorage.getItem(STORAGE_KEY)
      if (raw) return JSON.parse(raw)
    } catch {}
    return null
  }

  function savePosition(x: number, y: number) {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify({ x, y }))
    } catch {}
  }

  function loadButtonPosition(): { x: number; y: number } | null {
    try {
      const raw = localStorage.getItem(BUTTON_STORAGE_KEY)
      if (raw) return JSON.parse(raw)
    } catch {}
    return null
  }

  function saveButtonPosition(x: number, y: number) {
    try {
      localStorage.setItem(BUTTON_STORAGE_KEY, JSON.stringify({ x, y }))
    } catch {}
  }

  async function fetchPs() {
    try {
      const res = await apiFetch('/api/debug/ps')
      const data = await res.json()
      psData = data
    } catch {}
  }

  function bringToFront() {
    topZIndex += 1
  }

  async function openSystemLog() {
    if (systemLogWindow) {
      topZIndex += 1
      systemLogWindow = { ...systemLogWindow, zIndex: topZIndex }
      return
    }

    const newZIndex = topZIndex + 1
    topZIndex = newZIndex
    systemLogWindow = {
      logPath: '',
      lines: [],
      loading: true,
      pos: { x: 100, y: 80 },
      size: { w: 700, h: 500 },
      autoScroll: true,
      zIndex: newZIndex,
    }

    try {
      const res = await apiFetch('/api/debug/log/tail?lines=200')
      const data = await res.json()
      systemLogWindow = {
        ...systemLogWindow,
        logPath: data.path || '',
        lines: data.lines || [],
        loading: false,
      }
    } catch {
      systemLogWindow = { ...systemLogWindow, loading: false }
    }
  }

  function closeSystemLog() {
    systemLogWindow = null
    if (logInterval) {
      clearInterval(logInterval)
      logInterval = undefined as any
    }
  }

  function updateSystemLogPos(x: number, y: number) {
    if (systemLogWindow) {
      systemLogWindow = { ...systemLogWindow, pos: { x, y } }
    }
  }

  function toggleSystemLogAutoScroll() {
    if (systemLogWindow) {
      systemLogWindow = { ...systemLogWindow, autoScroll: !systemLogWindow.autoScroll }
    }
  }

  function openTextFormatter() {
    if (textFormatterWindow) {
      topZIndex += 1
      textFormatterWindow = { ...textFormatterWindow, zIndex: topZIndex }
      return
    }

    const newZIndex = topZIndex + 1
    topZIndex = newZIndex
    textFormatterWindow = {
      content: '',
      pos: { x: 120, y: 100 },
      size: { w: 700, h: 500 },
      zIndex: newZIndex,
    }
  }

  function closeTextFormatter() {
    textFormatterWindow = null
  }

  function updateTextFormatterPos(x: number, y: number) {
    if (textFormatterWindow) {
      textFormatterWindow = { ...textFormatterWindow, pos: { x, y } }
    }
  }

  function updateTextFormatterContent(content: string) {
    if (textFormatterWindow) {
      textFormatterWindow = { ...textFormatterWindow, content }
    }
  }

  // Button drag handlers
  function onButtonDragStart(e: MouseEvent | TouchEvent) {
    e.preventDefault()
    const rect = buttonRef?.getBoundingClientRect()
    if (!rect) return
    const clientX = 'touches' in e ? e.touches[0].clientX : e.clientX
    const clientY = 'touches' in e ? e.touches[0].clientY : e.clientY
    buttonDragStart = {
      startX: clientX,
      startY: clientY,
      origX: rect.left,
      origY: rect.top,
    }
    buttonDragging = true

    const onMove = (ev: MouseEvent | TouchEvent) => {
      if (!buttonDragging) return
      const cx = 'touches' in ev ? ev.touches[0].clientX : ev.clientX
      const cy = 'touches' in ev ? ev.touches[0].clientY : ev.clientY
      const dx = cx - buttonDragStart.startX
      const dy = cy - buttonDragStart.startY
      const newX = Math.max(0, buttonDragStart.origX + dx)
      const newY = Math.max(0, buttonDragStart.origY + dy)
      buttonPos = { x: newX, y: newY }
    }

    const onUp = () => {
      if (buttonDragging) {
        const rect = buttonRef?.getBoundingClientRect()
        if (rect) saveButtonPosition(rect.left, rect.top)
      }
      buttonDragging = false
      window.removeEventListener('mousemove', onMove)
      window.removeEventListener('mouseup', onUp)
      window.removeEventListener('touchmove', onMove)
      window.removeEventListener('touchend', onUp)
    }

    window.addEventListener('mousemove', onMove)
    window.addEventListener('mouseup', onUp)
    window.addEventListener('touchmove', onMove, { passive: false })
    window.addEventListener('touchend', onUp)
  }

  // Panel drag handlers
  function onPanelDragStart(e: MouseEvent | TouchEvent) {
    e.preventDefault()
    const rect = panelRef?.getBoundingClientRect()
    if (!rect) return
    const clientX = 'touches' in e ? e.touches[0].clientX : e.clientX
    const clientY = 'touches' in e ? e.touches[0].clientY : e.clientY
    panelDragStart = {
      startX: clientX,
      startY: clientY,
      origX: rect.left,
      origY: rect.top,
    }
    panelDragging = true

    const onMove = (ev: MouseEvent | TouchEvent) => {
      if (!panelDragging) return
      const cx = 'touches' in ev ? ev.touches[0].clientX : ev.clientX
      const cy = 'touches' in ev ? ev.touches[0].clientY : ev.clientY
      const dx = cx - panelDragStart.startX
      const dy = cy - panelDragStart.startY
      const newX = Math.max(0, panelDragStart.origX + dx)
      const newY = Math.max(0, panelDragStart.origY + dy)
      panelPos = { x: newX, y: newY }
    }

    const onUp = () => {
      if (panelDragging) {
        const rect = panelRef?.getBoundingClientRect()
        if (rect) savePosition(rect.left, rect.top)
      }
      panelDragging = false
      window.removeEventListener('mousemove', onMove)
      window.removeEventListener('mouseup', onUp)
      window.removeEventListener('touchmove', onMove)
      window.removeEventListener('touchend', onUp)
    }

    window.addEventListener('mousemove', onMove)
    window.addEventListener('mouseup', onUp)
    window.addEventListener('touchmove', onMove, { passive: false })
    window.addEventListener('touchend', onUp)
  }

  onMount(async () => {
    // Load saved positions
    const savedPos = loadPosition()
    if (savedPos) panelPos = savedPos
    const savedBtnPos = loadButtonPosition()
    if (savedBtnPos) buttonPos = savedBtnPos

    // Check debug mode
    try {
      const res = await apiFetch('/api/settings')
      const data = await res.json()
      env = data.env || 'production'
    } catch {
      env = 'production'
    }
  })

  // Auto-refresh PS when panel is open
  $effect(() => {
    if (open) {
      fetchPs()
      psInterval = setInterval(fetchPs, 5000)
    } else {
      if (psInterval) clearInterval(psInterval)
    }
    return () => {
      if (psInterval) clearInterval(psInterval)
    }
  })

  // Auto-refresh system log when window is open
  $effect(() => {
    if (systemLogWindow && !systemLogWindow.loading) {
      logInterval = setInterval(async () => {
        try {
          const res = await fetch('/api/debug/log/tail?lines=200')
          const data = await res.json()
          if (systemLogWindow) {
            systemLogWindow = {
              ...systemLogWindow,
              logPath: data.path || '',
              lines: data.lines || [],
            }
          }
        } catch {}
      }, 2000)
    } else {
      if (logInterval) {
        clearInterval(logInterval)
        logInterval = undefined as any
      }
    }
    return () => {
      if (logInterval) clearInterval(logInterval)
    }
  })

  onDestroy(() => {
    if (psInterval) clearInterval(psInterval)
    if (logInterval) clearInterval(logInterval)
  })

  // Computed styles
  const buttonStyle = $derived(
    buttonPos.y === -1
      ? 'position: fixed; left: 12px; bottom: 80px; z-index: 70;'
      : `position: fixed; left: ${buttonPos.x}px; top: ${buttonPos.y}px; z-index: 70;`
  )

  const panelStyle = $derived(
    panelPos.y === -1
      ? 'position: fixed; left: 16px; bottom: 80px; z-index: 65;'
      : `position: fixed; left: ${panelPos.x}px; top: ${panelPos.y}px; z-index: 65;`
  )
</script>

{#if env === 'debug'}
  <!-- Floating trigger button -->
  <button
    bind:this={buttonRef}
    onmousedown={onButtonDragStart}
    ontouchstart={onButtonDragStart}
    onclick={() => { if (!buttonDragging) open = !open }}
    style={buttonStyle}
    class="w-12 h-12 bg-gray-800/90 border-[5px] border-gray-600 rounded-full flex items-center justify-center hover:bg-gray-700/90 hover:border-gray-500 transition-colors shadow-lg cursor-grab active:cursor-grabbing p-0 touch-none"
    title="Debug Panel"
  >
    <img
      src={iconFallback ? debugIcon : '/api/config/debug-icon'}
      onerror={() => (iconFallback = true)}
      alt="Debug"
      class="w-12 h-12 rounded-full object-cover pointer-events-none"
    />
  </button>

  <!-- Backdrop -->
  {#if open}
    <div class="fixed inset-0 z-[60]" onclick={() => open = false}>
      <div class="absolute inset-0 bg-black/40"></div>
    </div>
  {/if}

  <!-- Draggable Panel -->
  {#if open}
    <div
      bind:this={panelRef}
      style={panelStyle}
      class="w-[520px] max-w-[90vw] h-[600px] max-h-[80vh] bg-gray-900 border border-gray-800 rounded-xl shadow-2xl flex flex-col overflow-hidden touch-none"
    >
      <!-- Drag handle / Header -->
      <div
        onmousedown={onPanelDragStart}
        ontouchstart={onPanelDragStart}
        class="flex items-center justify-between px-5 py-3 border-b border-gray-800 cursor-grab active:cursor-grabbing select-none bg-gray-900 touch-none"
      >
        <h2 class="text-sm font-semibold text-gray-300 uppercase tracking-wider">Debug</h2>
        <div class="flex items-center gap-2">
          <button
            onclick={(e) => { e.stopPropagation(); fetchPs() }}
            class="text-xs text-blue-400 hover:text-blue-300 transition-colors px-2 py-0.5 rounded hover:bg-gray-800"
          >
            Refresh
          </button>
          <button
            onclick={(e) => { e.stopPropagation(); open = false }}
            class="text-gray-500 hover:text-white transition-colors"
          >
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" class="w-4 h-4">
              <path fill-rule="evenodd" d="M5.47 5.47a.75.75 0 011.06 0L12 10.94l5.47-5.47a.75.75 0 111.06 1.06L13.06 12l5.47 5.47a.75.75 0 11-1.06 1.06L12 13.06l-5.47 5.47a.75.75 0 01-1.06-1.06L10.94 12 5.47 6.53a.75.75 0 010-1.06z" clip-rule="evenodd" />
            </svg>
          </button>
        </div>
      </div>

      <!-- Content -->
      <div class="flex-1 overflow-y-auto p-4 space-y-4">
        <!-- PS Section -->
        <section>
          <h3 class="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
            Claude Processes
          </h3>

          {#if !psData}
            <p class="text-sm text-gray-500">Loading...</p>
          {:else if psData.entries.length === 0}
            <div class="bg-gray-800/50 border border-gray-700 rounded-lg px-4 py-3">
              <p class="text-sm text-gray-400">No claude processes found</p>
            </div>
          {:else}
            <div class="overflow-x-auto rounded-lg border border-gray-700">
              <table class="w-full text-xs">
                <thead>
                  <tr class="bg-gray-800 text-gray-400">
                    <th class="px-3 py-2 text-left font-medium">Host</th>
                    <th class="px-3 py-2 text-left font-medium">Project</th>
                    <th class="px-3 py-2 text-left font-medium">Agent</th>
                    <th class="px-3 py-2 text-left font-medium">Session</th>
                    <th class="px-3 py-2 text-left font-medium">PID</th>
                    <th class="px-3 py-2 text-left font-medium">UUID</th>
                  </tr>
                </thead>
                <tbody class="divide-y divide-gray-800">
                  {#each psData.entries as entry}
                    <tr class="text-gray-300 hover:bg-gray-800/50">
                      <td class="px-3 py-1.5 font-mono whitespace-nowrap">
                        {#if entry.host !== 'local'}
                          <span class="text-blue-400 bg-blue-900/30 text-[9px] px-1 py-0.5 rounded mr-1 align-middle">ssh</span>
                        {/if}
                        <span class={entry.host === 'local' ? 'text-gray-500' : 'text-gray-300'}>{entry.host}</span>
                      </td>
                      <td class="px-3 py-1.5 truncate max-w-[160px]" title="{entry.org}/{entry.project}">
                        <span class={entry.org === '?' ? 'text-gray-600' : 'text-gray-300'}>{entry.org === '?' ? '?' : `${entry.org}/${entry.project}`}</span>
                      </td>
                      <td class="px-3 py-1.5 text-gray-400 truncate max-w-[80px]">{entry.agent}</td>
                      <td class="px-3 py-1.5 truncate max-w-[80px]">{entry.session}</td>
                      <td class="px-3 py-1.5 font-mono">{entry.pid}</td>
                      <td class="px-3 py-1.5 font-mono text-gray-500">{entry.session_uuid.slice(0, 8)}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </section>

        <!-- Raw output -->
        {#if psData && psData.raw}
          <details class="group">
            <summary class="text-xs text-gray-500 cursor-pointer hover:text-gray-300 transition-colors">
              Raw output
            </summary>
            <pre class="mt-2 text-xs text-gray-500 bg-gray-800/30 rounded-lg p-3 overflow-x-auto whitespace-pre-wrap">{psData.raw}</pre>
          </details>
        {/if}

        <!-- System Log Section -->
        <section>
          <h3 class="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
            System
          </h3>
          <button
            onclick={openSystemLog}
            class="w-full bg-gray-800/50 border border-gray-700 rounded-lg px-4 py-3 hover:bg-gray-800 transition-colors flex items-center justify-between"
          >
            <span class="text-sm text-gray-300">System Log</span>
            <span class="text-xs text-gray-500">Real-time →</span>
          </button>
        </section>

        <!-- Debug Overlays Section -->
        <section>
          <h3 class="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
            Debug Overlays
          </h3>
          <div class="space-y-2">
            <DebugToggle
              label="Message Stats"
              desc="Show thread/message stats in SessionView header"
              flagKey="showMessageStats"
            />
            <DebugToggle
              label="Raw Message Dump"
              desc="Dump all session messages as raw text list"
              flagKey="showRawMessageDump"
            />
          </div>
        </section>

        <!-- Toolbox Section -->
        <section>
          <h3 class="text-xs font-semibold text-gray-500 uppercase tracking-wider mb-2">
            Toolbox
          </h3>
          <div class="grid grid-cols-3 gap-2">
            <button
              onclick={openTextFormatter}
              class="bg-gray-800/50 border border-gray-700 rounded-lg px-3 py-4 hover:bg-gray-800 hover:border-gray-600 transition-colors flex flex-col items-center gap-2"
            >
              <svg class="w-5 h-5 text-gray-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width={2} d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
              </svg>
              <span class="text-xs text-gray-300">Text Formatter</span>
            </button>
            <button
              class="bg-gray-800/30 border border-gray-700/50 rounded-lg px-3 py-4 flex flex-col items-center gap-2 opacity-50 cursor-not-allowed"
              disabled
            >
              <svg class="w-5 h-5 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width={2} d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
              </svg>
              <span class="text-xs text-gray-500">Coming Soon</span>
            </button>
            <button
              class="bg-gray-800/30 border border-gray-700/50 rounded-lg px-3 py-4 flex flex-col items-center gap-2 opacity-50 cursor-not-allowed"
              disabled
            >
              <svg class="w-5 h-5 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width={2} d="M10 20l4-16m4 4l4 4-4 4M6 16l-4-4 4-4" />
              </svg>
              <span class="text-xs text-gray-500">Coming Soon</span>
            </button>
          </div>
        </section>
      </div>
    </div>
  {/if}

  <!-- System Log Floating Window -->
  {#if systemLogWindow}
    <SystemLogWindow
      win={systemLogWindow}
      onClose={closeSystemLog}
      onMove={updateSystemLogPos}
      onToggleAutoScroll={toggleSystemLogAutoScroll}
      onFocus={bringToFront}
    />
  {/if}

  <!-- Text Formatter Floating Window -->
  {#if textFormatterWindow}
    <TextFormatterWindow
      win={textFormatterWindow}
      onClose={closeTextFormatter}
      onMove={updateTextFormatterPos}
      onUpdateContent={updateTextFormatterContent}
      onFocus={bringToFront}
    />
  {/if}
{/if}
