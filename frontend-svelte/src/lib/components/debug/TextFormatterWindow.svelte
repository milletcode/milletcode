<script lang="ts">
  interface WindowData {
    content: string
    pos: { x: number; y: number }
    size: { w: number; h: number }
    zIndex: number
  }

  interface Props {
    win: WindowData
    onClose: () => void
    onMove: (x: number, y: number) => void
    onUpdateContent: (content: string) => void
    onFocus: () => void
  }

  let { win, onClose, onMove, onUpdateContent, onFocus }: Props = $props()

  let textareaRef: HTMLTextAreaElement
  let isDragging = false
  let dragStart = { x: 0, y: 0, origX: 0, origY: 0 }

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

  async function handlePaste() {
    let text = ''

    // Try Clipboard API first (requires secure context / HTTPS)
    if (navigator.clipboard && window.isSecureContext) {
      try {
        text = await navigator.clipboard.readText()
      } catch (err) {
        console.error('Clipboard API failed:', err)
      }
    }

    // If clipboard API not available (HTTP context), focus textarea and show hint
    if (!text) {
      if (textareaRef) {
        textareaRef.focus()
        const originalPlaceholder = textareaRef.placeholder
        textareaRef.placeholder = '请使用 Ctrl+V / Cmd+V 粘贴内容...'
        setTimeout(() => {
          if (textareaRef) {
            textareaRef.placeholder = originalPlaceholder
          }
        }, 3000)
      }
      return
    }

    // Try to detect and format JSON
    try {
      const parsed = JSON.parse(text)
      const formatted = JSON.stringify(parsed, null, 2)
      onUpdateContent(formatted)
      return
    } catch {
      // Not JSON, continue
    }

    // Try to detect YAML (simple heuristic)
    const yamlPattern = /^[\w-]+:\s+.+$/m
    if (yamlPattern.test(text) && !text.includes('{') && !text.includes('[')) {
      onUpdateContent(text)
      return
    }

    // Plain text: convert \n to actual newlines if needed
    const converted = text.replace(/\\n/g, '\n')
    onUpdateContent(converted)
  }

  function handleBeautify() {
    const content = win.content
    if (!content.trim()) return

    // Try JSON pretty-print
    try {
      const parsed = JSON.parse(content)
      const formatted = JSON.stringify(parsed, null, 2)
      onUpdateContent(formatted)
      return
    } catch {
      // Not JSON, continue
    }

    // YAML heuristic: normalize line endings
    const yamlPattern = /^[\w-]+:\s+.+$/m
    if (yamlPattern.test(content) && !content.includes('{') && !content.includes('[')) {
      const normalized = content.replace(/\r\n/g, '\n').replace(/\r/g, '\n')
      onUpdateContent(normalized)
      return
    }

    // Plain text: convert escaped \n and normalize line endings
    const converted = content
      .replace(/\\n/g, '\n')
      .replace(/\r\n/g, '\n')
      .replace(/\r/g, '\n')
    onUpdateContent(converted)
  }

  async function handleExport() {
    try {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(win.content)
      } else {
        const textArea = document.createElement('textarea')
        textArea.value = win.content
        textArea.style.position = 'fixed'
        textArea.style.left = '-999999px'
        document.body.appendChild(textArea)
        textArea.select()
        try {
          document.execCommand('copy')
        } catch (err) {
          console.error('Copy failed:', err)
        }
        document.body.removeChild(textArea)
      }
    } catch (err) {
      console.error('Failed to copy:', err)
    }
  }

  function handleClear() {
    onUpdateContent('')
  }

  function handleDeleteLine() {
    const textarea = textareaRef
    if (!textarea) return

    const cursorPos = textarea.selectionStart
    const text = win.content

    // Find the start and end of the current line
    let lineStart = cursorPos
    while (lineStart > 0 && text[lineStart - 1] !== '\n') {
      lineStart--
    }

    let lineEnd = cursorPos
    while (lineEnd < text.length && text[lineEnd] !== '\n') {
      lineEnd++
    }

    // Remove the line (including the newline character if it exists)
    const newText = text.slice(0, lineStart) + text.slice(lineEnd + (lineEnd < text.length ? 1 : 0))
    onUpdateContent(newText)

    // Restore cursor position
    setTimeout(() => {
      if (textareaRef) {
        textareaRef.focus()
        const newPos = Math.min(lineStart, newText.length)
        textareaRef.setSelectionRange(newPos, newPos)
      }
    }, 0)
  }

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
        Text Formatter
      </h3>
    </div>
    <div class="flex items-center gap-2">
      <button
        onclick={(e) => { e.stopPropagation(); handlePaste() }}
        class="text-xs px-2 py-1 rounded bg-blue-600 hover:bg-blue-700 text-white transition-colors"
        title="从剪贴板粘贴并自动格式化"
      >
        粘贴
      </button>
      <button
        onclick={(e) => { e.stopPropagation(); handleBeautify() }}
        class="text-xs px-2 py-1 rounded bg-purple-600 hover:bg-purple-700 text-white transition-colors"
        title="美化当前内容（JSON 格式化 / 换行转换）"
      >
        美化
      </button>
      <button
        onclick={(e) => { e.stopPropagation(); handleExport() }}
        class="text-xs px-2 py-1 rounded bg-green-600 hover:bg-green-700 text-white transition-colors"
        title="导出到剪贴板"
      >
        导出
      </button>
      <button
        onclick={(e) => { e.stopPropagation(); handleClear() }}
        class="text-xs px-2 py-1 rounded bg-gray-700 hover:bg-gray-600 text-white transition-colors"
        title="清空内容"
      >
        清空
      </button>
      <button
        onclick={(e) => { e.stopPropagation(); handleDeleteLine() }}
        class="text-xs px-2 py-1 rounded bg-red-600 hover:bg-red-700 text-white transition-colors"
        title="删除光标所在行"
      >
        删除本行
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
  <div class="flex-1 p-3">
    <textarea
      bind:this={textareaRef}
      value={win.content}
      oninput={(e) => onUpdateContent((e.target as HTMLTextAreaElement).value)}
      class="w-full h-full bg-gray-800 border border-gray-700 rounded-lg p-3 text-sm text-gray-200 font-mono resize-none focus:outline-none focus:border-blue-500"
      placeholder="点击「粘贴」按钮从剪贴板读取内容..."
    ></textarea>
  </div>
</div>
