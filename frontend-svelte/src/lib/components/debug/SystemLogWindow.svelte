<script lang="ts">
  interface WindowData {
    logPath: string
    lines: string[]
    loading: boolean
    pos: { x: number; y: number }
    size: { w: number; h: number }
    autoScroll: boolean
    zIndex: number
  }

  interface Props {
    win: WindowData
    onClose: () => void
    onMove: (x: number, y: number) => void
    onToggleAutoScroll: () => void
    onFocus: () => void
  }

  let { win, onClose, onMove, onToggleAutoScroll, onFocus }: Props = $props()

  let contentRef: HTMLDivElement
  let isDragging = false
  let dragStart = { x: 0, y: 0, origX: 0, origY: 0 }

  function formatTimestamp(timestamp: string | number): string {
    const date = new Date(timestamp)
    return date.toLocaleString('zh-CN', {
      year: 'numeric', month: '2-digit', day: '2-digit',
      hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false
    })
  }

  function parseLogTimestamp(line: string): string | null {
    const match = line.match(/^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?)/)
    if (match) {
      return formatTimestamp(match[1])
    }
    return null
  }

  function onDragStart(e: MouseEvent) {
    e.preventDefault()
    e.stopPropagation()
    onFocus()
    isDragging = true
    dragStart = {
      x: e.clientX,
      y: e.clientY,
      origX: win.pos.x,
      origY: win.pos.y,
    }

    const onMouseMove = (ev: MouseEvent) => {
      if (!isDragging) return
      const dx = ev.clientX - dragStart.x
      const dy = ev.clientY - dragStart.y
      const newX = Math.max(0, dragStart.origX + dx)
      const newY = Math.max(0, dragStart.origY + dy)
      onMove(newX, newY)
    }

    const onMouseUp = () => {
      isDragging = false
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
    }

    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)
  }

  // Auto-scroll to bottom when new content arrives
  $effect(() => {
    if (win.autoScroll && contentRef) {
      contentRef.scrollTop = contentRef.scrollHeight
    }
  })

  const style = $derived(
    `position: fixed; left: ${win.pos.x}px; top: ${win.pos.y}px; width: ${win.size.w}px; height: ${win.size.h}px; z-index: ${win.zIndex};`
  )
</script>

<div
  {style}
  onclick={onFocus}
  class="max-w-[90vw] max-h-[80vh] bg-gray-900 border border-gray-700 rounded-xl shadow-2xl flex flex-col overflow-hidden"
>
  <!-- Header / drag handle -->
  <div
    onmousedown={onDragStart}
    class="flex items-center justify-between px-4 py-2.5 border-b border-gray-800 cursor-grab active:cursor-grabbing select-none bg-gray-900"
  >
    <div class="flex-1 min-w-0">
      <h3 class="text-xs font-semibold text-gray-300 truncate">
        System Log
      </h3>
      {#if win.logPath}
        <p class="text-[10px] text-gray-500 truncate">{win.logPath}</p>
      {/if}
    </div>
    <div class="flex items-center gap-2">
      <button
        onclick={(e) => { e.stopPropagation(); onToggleAutoScroll() }}
        class="text-xs px-2 py-0.5 rounded transition-colors {win.autoScroll ? 'bg-blue-600 text-white' : 'bg-gray-700 text-gray-400 hover:text-white'}"
      >
        {win.autoScroll ? 'Auto-scroll ON' : 'Auto-scroll OFF'}
      </button>
      <button
        onclick={(e) => { e.stopPropagation(); onClose() }}
        class="text-gray-500 hover:text-white transition-colors"
      >
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" class="w-4 h-4">
          <path fill-rule="evenodd" d="M5.47 5.47a.75.75 0 011.06 0L12 10.94l5.47-5.47a.75.75 0 111.06 1.06L13.06 12l5.47 5.47a.75.75 0 11-1.06 1.06L12 13.06l-5.47 5.47a.75.75 0 01-1.06-1.06L10.94 12 5.47 6.53a.75.75 0 010-1.06z" clip-rule="evenodd" />
        </svg>
      </button>
    </div>
  </div>

  <!-- Content -->
  <div bind:this={contentRef} class="flex-1 overflow-y-auto p-3 font-mono text-xs">
    {#if win.loading}
      <p class="text-gray-500">Loading log...</p>
    {:else if win.lines.length === 0}
      <p class="text-gray-500">No log entries</p>
    {:else}
      {#each win.lines as line}
        {@const localTimestamp = parseLogTimestamp(line)}
        {@const restOfLine = localTimestamp ? line.replace(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?\s*/, '') : line}
        <div class="text-gray-300 whitespace-pre-wrap break-all hover:bg-gray-800/50">
          {#if localTimestamp}
            <span class="text-blue-400">{localTimestamp}</span>
            <span class="text-gray-300"> {restOfLine}</span>
          {:else}
            <span class="text-gray-300">{line}</span>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>
