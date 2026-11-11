<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { X, PanelRightClose, PanelRightOpen } from 'lucide-svelte'
  import { Terminal } from '@xterm/xterm'
  import { FitAddon } from '@xterm/addon-fit'
  import '@xterm/xterm/css/xterm.css'
  import { apiFetch } from '$lib/api'

  interface Props {
    project: { id: string; name: string }
    onclose: () => void
  }

  let { project, onclose }: Props = $props()

  let terminalContainer: HTMLDivElement
  let terminal: Terminal
  let fitAddon: FitAddon

  // Session credentials — plain values: they never drive re-render and must
  // not end up in a $state proxy or the URL. termReady is the reactive mirror
  // the template can key off (Svelte 5 won't re-evaluate expressions that only
  // read plain variables).
  let terminalId = ''
  let token = ''
  let termReady = $state(false)

  type Status = 'creating' | 'connecting' | 'connected' | 'reconnecting' | 'ended' | 'error'
  let status = $state<Status>('creating')
  let createError = $state<string | null>(null)
  let mode = $state<'local' | 'ssh'>('local')
  let hostname = $state<string | null>(null)

  interface ChatEntry {
    id: number
    request: string
    command: string
    editOrig: string
    editing: boolean
    sent: boolean
  }
  let chatEntries = $state<ChatEntry[]>([])
  let nextEntryId = 0
  let nlInput = $state('')
  let nlBusy = $state(false)
  let nlError = $state<string | null>(null)
  let quickCmds = $state<{ id: string; name: string; command: string }[]>([])

  let ws: WebSocket | null = null
  let reconnectAttempts = 0
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let isDestroyed = false
  let resizeObserver: ResizeObserver | null = null

  const CHAT_WIDTH_KEY = 'millet-terminal-chat-width'
  const CHAT_VISIBLE_KEY = 'millet-terminal-chat-visible'
  let chatWidth = $state(
    Math.min(560, Math.max(260, Number(localStorage.getItem(CHAT_WIDTH_KEY)) || 320)),
  )
  let chatVisible = $state(localStorage.getItem(CHAT_VISIBLE_KEY) !== '0')
  let dragging = $state(false)

  function toggleChat() {
    chatVisible = !chatVisible
    localStorage.setItem(CHAT_VISIBLE_KEY, chatVisible ? '1' : '0')
  }

  // Pointer events cover mouse + touch; touch-action:none on the divider stops
  // the browser from hijacking the gesture as a scroll.
  function startDrag(e: PointerEvent) {
    e.preventDefault()
    dragging = true
    const startX = e.clientX
    const startW = chatWidth
    const onMove = (ev: PointerEvent) => {
      chatWidth = Math.min(560, Math.max(260, startW - (ev.clientX - startX)))
    }
    const onUp = () => {
      dragging = false
      localStorage.setItem(CHAT_WIDTH_KEY, String(chatWidth))
      document.removeEventListener('pointermove', onMove)
      document.removeEventListener('pointerup', onUp)
      document.removeEventListener('pointercancel', onUp)
    }
    document.addEventListener('pointermove', onMove)
    document.addEventListener('pointerup', onUp)
    document.addEventListener('pointercancel', onUp)
  }

  // Phones have no Esc/Tab/arrow keys — a compact function-key bar sends the
  // control sequences straight to the PTY. Buttons cancel pointerdown so taps
  // never steal focus from xterm (which would hide the OS keyboard).
  const coarsePointer =
    typeof window !== 'undefined' &&
    (window.matchMedia('(pointer: coarse)').matches || 'ontouchstart' in window)

  interface FnKey {
    label: string
    seq: string
    accent?: boolean
  }
  const FN_KEYS: FnKey[] = [
    { label: 'Esc', seq: '\x1b' },
    { label: 'Tab', seq: '\t' },
    { label: 'Ctrl+C', seq: '\x03', accent: true },
    { label: 'Ctrl+D', seq: '\x04', accent: true },
    { label: 'Ctrl+Z', seq: '\x1a', accent: true },
    { label: 'Ctrl+L', seq: '\x0c' },
    { label: '↑', seq: '\x1b[A' },
    { label: '↓', seq: '\x1b[B' },
    { label: '←', seq: '\x1b[D' },
    { label: '→', seq: '\x1b[C' },
    { label: 'Home', seq: '\x1b[H' },
    { label: 'End', seq: '\x1b[F' },
    { label: 'PgUp', seq: '\x1b[5~' },
    { label: 'PgDn', seq: '\x1b[6~' },
  ]

  function sendKey(seq: string) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(seq)
    }
  }

  // Window close / refresh must free the backend PTY (keepalive lets the
  // DELETE survive page unload).
  function releaseOnUnload() {
    if (terminalId) {
      apiFetch(`/api/terminal/${terminalId}`, { method: 'DELETE', keepalive: true }).catch(() => {})
    }
  }

  $effect(() => {
    window.addEventListener('beforeunload', releaseOnUnload)
    mount()
    return () => {
      window.removeEventListener('beforeunload', releaseOnUnload)
      destroy()
    }
  })

  async function mount() {
    terminal = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: 'Menlo, Monaco, "Courier New", monospace',
      theme: {
        background: '#1a1a1a',
        foreground: '#e0e0e0',
        cursor: '#e0e0e0',
        selectionBackground: '#3a3a3a',
      },
    })
    fitAddon = new FitAddon()
    terminal.loadAddon(fitAddon)
    terminal.open(terminalContainer)
    terminal.onData((data) => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(data)
      }
    })

    resizeObserver = new ResizeObserver(() => {
      if (fitAddon) {
        fitAddon.fit()
        sendResize()
      }
    })
    resizeObserver.observe(terminalContainer)

    // Measure the real pane size before asking the backend for a PTY, so the
    // initial cols/rows match what the user sees.
    await new Promise((r) => requestAnimationFrame(r))
    fitAddon.fit()
    await new Promise((r) => setTimeout(r, 100))
    fitAddon.fit()

    await createSession()
    loadQuickCommands()
  }

  function destroy() {
    isDestroyed = true
    termReady = false
    if (reconnectTimer) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    if (ws) {
      ws.close()
      ws = null
    }
    if (terminalId) {
      // Free the PTY / SSH connection server-side.
      apiFetch(`/api/terminal/${terminalId}`, { method: 'DELETE' }).catch(() => {})
    }
    resizeObserver?.disconnect()
    terminal?.dispose()
  }

  async function createSession() {
    status = 'creating'
    try {
      const res = await apiFetch('/api/terminal/create', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          project_id: project.id,
          cols: terminal.cols,
          rows: terminal.rows,
        }),
      })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) {
        status = 'error'
        createError = data.error || `HTTP ${res.status}`
        return
      }
      terminalId = data.terminal_id
      token = data.token
      mode = data.mode || 'local'
      hostname = data.hostname || null
      termReady = true
      connectWs()
    } catch (err) {
      status = 'error'
      createError = String(err)
    }
  }

  async function loadQuickCommands() {
    const [keyword, ...rest] = project.id.split('/')
    const name = rest.join('/')
    if (!keyword || !name) return
    try {
      const res = await apiFetch(
        `/api/projects/${encodeURIComponent(keyword)}/${encodeURIComponent(name)}/terminal/commands`,
      )
      if (res.ok) {
        quickCmds = await res.json()
      }
    } catch {
      // Quick commands are optional decoration — ignore failures.
    }
  }

  function connectWs() {
    if (!terminalId || isDestroyed) return

    if (ws) {
      ws.close()
      ws = null
    }

    status = 'connecting'
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const url = `${proto}//${window.location.host}/api/terminal/${terminalId}/ws`
    // Token rides as a WebSocket subprotocol: it never appears in URLs or logs.
    ws = new WebSocket(url, [token])
    ws.binaryType = 'arraybuffer'

    ws.onopen = () => {
      status = 'connected'
      reconnectAttempts = 0
      // A fresh attach replays the full snapshot — start from a clean screen.
      terminal.reset()
      terminal.focus()
      sendResize()
    }

    ws.onmessage = (event) => {
      if (event.data instanceof ArrayBuffer) {
        terminal.write(new Uint8Array(event.data))
      } else {
        terminal.write(event.data)
      }
    }

    ws.onclose = (event) => {
      ws = null
      if (isDestroyed) return

      if (event.code === 1000 || event.code === 1001) {
        status = 'ended'
        terminal.write('\r\n\x1b[31m[' + $_('terminal.ended') + ']\x1b[0m\r\n')
        return
      }

      // The PTY lives on in the backend — retry forever with capped backoff.
      status = 'reconnecting'
      const delay = Math.min(500 * 2 ** reconnectAttempts, 8000)
      reconnectAttempts++
      reconnectTimer = setTimeout(() => {
        if (!isDestroyed) connectWs()
      }, delay)
    }
  }

  function sendResize() {
    if (!terminalId || !terminal) return
    apiFetch(`/api/terminal/${terminalId}/resize`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ cols: terminal.cols, rows: terminal.rows }),
    }).catch(() => {})
  }

  async function generateCommand() {
    if (!nlInput.trim() || !terminalId) return
    const request = nlInput.trim()
    nlBusy = true
    nlError = null
    try {
      const res = await apiFetch(`/api/terminal/${terminalId}/command`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token, text: request }),
      })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) {
        nlError = data.error || `HTTP ${res.status}`
      } else {
        chatEntries.push({
          id: ++nextEntryId,
          request,
          command: data.command,
          editOrig: data.command,
          editing: false,
          sent: false,
        })
        nlInput = ''
      }
    } catch (err) {
      nlError = String(err)
    } finally {
      nlBusy = false
    }
  }

  async function inject(entry: ChatEntry, enter: boolean) {
    if (!terminalId || !entry.command.trim()) return
    await apiFetch(`/api/terminal/${terminalId}/input`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token, text: entry.command, enter }),
    }).catch(() => {})
    entry.sent = true
    terminal.focus()
  }

  function startEdit(entry: ChatEntry) {
    entry.editOrig = entry.command
    entry.editing = true
  }

  function cancelEdit(entry: ChatEntry) {
    entry.command = entry.editOrig
    entry.editing = false
  }

  async function runQuickCommand(cmd: string) {
    if (!terminalId) return
    await apiFetch(`/api/terminal/${terminalId}/input`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token, text: cmd, enter: true }),
    }).catch(() => {})
    terminal.focus()
  }

  const statusColor: Record<Status, string> = {
    creating: 'bg-gray-500',
    connecting: 'bg-amber-500',
    connected: 'bg-green-500',
    reconnecting: 'bg-amber-500 animate-pulse',
    ended: 'bg-gray-500',
    error: 'bg-red-500',
  }
</script>

<div class="h-full flex flex-col bg-gray-950">
  <!-- Header -->
  <div class="flex items-center justify-between px-3 py-1.5 bg-[#161616] border-b border-[#262626] shrink-0">
    <div class="flex items-center gap-2 min-w-0">
      <span class="text-xs font-medium text-gray-300">{$_('terminal.title')}</span>
      <span
        class="text-[10px] px-1.5 py-0.5 rounded {mode === 'ssh'
          ? 'text-blue-300 bg-blue-900/40'
          : 'text-gray-400 bg-[#232323]'}"
      >
        {mode === 'ssh' ? hostname : $_('terminal.local')}
      </span>
    </div>
    <div class="flex items-center gap-2">
      <span class="flex items-center gap-1.5 text-[10px] {status === 'connected' ? 'text-green-400' : 'text-gray-500'}">
        <span class="w-1.5 h-1.5 rounded-full {statusColor[status]}"></span>
        {$_(`terminal.${status}`)}
      </span>
      <button
        onclick={toggleChat}
        class="text-gray-500 hover:text-gray-300 transition-colors"
        title={chatVisible ? $_('terminal.hideChat') : $_('terminal.showChat')}
      >
        {#if chatVisible}
          <PanelRightClose class="w-3.5 h-3.5" />
        {:else}
          <PanelRightOpen class="w-3.5 h-3.5" />
        {/if}
      </button>
      <button
        onclick={onclose}
        class="text-gray-500 hover:text-gray-300 transition-colors"
        title={$_('common.close')}
      >
        <X class="w-3.5 h-3.5" />
      </button>
    </div>
  </div>

  <!-- Body: terminal left, chat panel right -->
  <div class="flex-1 flex min-h-0 {dragging ? 'select-none' : ''}">
    <!-- Terminal column: xterm fills, function-key bar sits underneath on touch devices -->
    <div class="flex-1 min-w-0 flex flex-col">
      <div class="flex-1 min-h-0 relative">
        {#if status === 'error' && createError}
          <div class="h-full flex flex-col items-center justify-center gap-2 p-6 text-center">
            <div class="text-red-400 text-sm">{$_('terminal.createFailed')}</div>
            <div class="text-red-300/70 text-xs font-mono break-all">{createError}</div>
          </div>
        {:else}
          <div bind:this={terminalContainer} class="h-full"></div>
        {/if}
      </div>
      {#if coarsePointer && termReady}
        <div
          class="shrink-0 flex gap-1.5 overflow-x-auto border-t border-[#262626] bg-[#161616] px-2 py-1.5"
          style="touch-action: pan-x"
        >
          {#each FN_KEYS as k (k.label)}
            <button
              onpointerdown={(e) => e.preventDefault()}
              onclick={() => sendKey(k.seq)}
              class="h-10 shrink-0 min-w-11 px-2 rounded bg-[#232323] active:bg-blue-700 text-[11px] font-medium {k.accent
                ? 'text-red-300 active:text-white'
                : 'text-gray-300 active:text-white'} transition-colors"
              aria-label={k.label}
            >
              {k.label}
            </button>
          {/each}
        </div>
      {/if}
    </div>

    {#if chatVisible}
      <!-- Divider: wider hit area on touch-sized screens -->
      <div
        onpointerdown={startDrag}
        class="w-4 sm:w-1.5 shrink-0 cursor-col-resize relative group"
        style="touch-action: none"
      >
        <div
          class="absolute inset-y-0 left-1/2 -translate-x-1/2 w-px h-full bg-[#262626] group-hover:bg-blue-500/60 transition-colors"
        ></div>
      </div>

      <!-- Chat → command panel -->
      <aside class="shrink-0 bg-[#1a1a1a] flex flex-col" style="width:{chatWidth}px">
      {#if quickCmds.length > 0}
        <div class="px-3 pt-2 pb-2 border-b border-[#262626] flex flex-wrap gap-1.5">
          <span class="text-[10px] text-gray-500 w-full">{$_('terminal.quickCommands')}</span>
          {#each quickCmds as cmd (cmd.id)}
            <button
              onclick={() => runQuickCommand(cmd.command)}
              class="text-[10px] px-2 py-0.5 rounded bg-[#232323] hover:bg-[#2e2e2e] text-gray-400 transition-colors"
              title={cmd.command}
            >
              {cmd.name}
            </button>
          {/each}
        </div>
      {/if}

      <div class="flex-1 overflow-y-auto p-3 space-y-2 min-h-0">
        {#if chatEntries.length === 0}
          <div class="text-[11px] text-gray-600 leading-relaxed mt-6 px-1">
            {$_('terminal.chatHint')}
          </div>
        {/if}
        {#each chatEntries as entry (entry.id)}
          <div
            class="rounded border border-[#262626] bg-[#141414] overflow-hidden {entry.sent
              ? 'opacity-70'
              : ''}"
          >
            <div class="px-2 pt-1.5 text-[10px] text-gray-500 break-all">{entry.request}</div>
            {#if entry.editing}
              <textarea
                bind:value={entry.command}
                rows="3"
                class="mx-2 my-1 w-[calc(100%-1rem)] text-xs font-mono text-green-300 bg-[#1f1f1f] border border-[#3a3a3a] rounded px-1.5 py-1 focus:outline-none focus:border-[#4a4a4a] resize-none"
              ></textarea>
              <div class="flex justify-end gap-1.5 px-2 pb-1.5">
                <button
                  onclick={() => cancelEdit(entry)}
                  class="text-[10px] px-2 py-1 rounded bg-[#232323] hover:bg-[#2e2e2e] text-gray-300 transition-colors"
                >
                  {$_('common.cancel')}
                </button>
                <button
                  onclick={() => (entry.editing = false)}
                  class="text-[10px] px-2 py-1 rounded bg-blue-700 hover:bg-blue-600 text-white transition-colors"
                >
                  {$_('common.save')}
                </button>
              </div>
            {:else}
              <div
                onclick={() => startEdit(entry)}
                class="px-2 py-1.5 text-xs font-mono text-green-300 break-all cursor-pointer hover:bg-[#1f1f1f] transition-colors"
                title={$_('terminal.edit')}
              >
                {entry.command}
              </div>
              <div class="flex items-center justify-between px-2 py-1.5 border-t border-[#262626]">
                <span class="text-[10px] text-gray-600">{entry.sent ? $_('terminal.sent') : ''}</span>
                <div class="flex gap-1.5">
                  <button
                    onclick={() => inject(entry, false)}
                    class="text-[10px] px-2 py-1 rounded bg-[#232323] hover:bg-[#2e2e2e] text-gray-300 transition-colors"
                  >
                    {$_('terminal.inject')}
                  </button>
                  <button
                    onclick={() => inject(entry, true)}
                    class="text-[10px] px-2 py-1 rounded bg-green-700 hover:bg-green-600 text-white transition-colors"
                  >
                    {$_('terminal.injectRun')}
                  </button>
                </div>
              </div>
            {/if}
          </div>
        {/each}
      </div>

      {#if nlError}
        <div class="px-3 pb-1 text-[10px] text-red-400">{$_('terminal.nlError')}: {nlError}</div>
      {/if}

      <div class="flex items-center gap-1.5 p-2 border-t border-[#262626]">
        <input
          bind:value={nlInput}
          onkeydown={(e) => {
            if (e.key === 'Enter' && !nlBusy) generateCommand()
          }}
          placeholder={$_('terminal.nlPlaceholder')}
          disabled={!termReady || nlBusy}
          class="flex-1 min-w-0 text-xs bg-[#232323] border border-[#333333] rounded px-2 py-1.5 text-gray-200 placeholder-gray-500 focus:outline-none focus:border-[#4a4a4a] disabled:opacity-50"
        />
        <button
          onclick={generateCommand}
          disabled={!nlInput.trim() || nlBusy || !termReady}
          class="text-xs px-2.5 py-1.5 rounded bg-blue-600 hover:bg-blue-500 text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed shrink-0"
        >
          {nlBusy ? $_('terminal.generating') : $_('terminal.generate')}
        </button>
      </div>
      </aside>
    {/if}
  </div>
</div>

<style>
  :global(.xterm) {
    height: 100%;
    padding: 8px;
    box-sizing: border-box;
  }

  :global(.xterm-viewport) {
    overflow-y: auto !important;
  }
</style>
