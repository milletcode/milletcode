<script lang="ts">
  import { onMount } from 'svelte'
  import { apiFetch } from '$lib/api'
  import hljs from 'highlight.js'
  import { X, ChevronLeft, FileCode, Folder, RefreshCw, Loader2, FileWarning, Eye, EyeOff, Download } from 'lucide-svelte'

  interface DirEntry {
    name: string
    is_dir: boolean
    size: number
  }
  interface FileContent {
    path: string
    is_binary: boolean
    truncated: boolean
    size: number
    content: string | null
  }

  interface Props {
    sessionKey: string
    onclose?: () => void
  }

  let { sessionKey, onclose }: Props = $props()

  let dirPath = $state('')
  let resolvedDir = $state('')
  let entries = $state<DirEntry[]>([])
  let file = $state<FileContent | null>(null)
  let loading = $state(false)
  let error = $state('')
  let showHidden = $state(true)

  const visibleEntries = $derived(
    showHidden ? entries : entries.filter((e) => !e.name.startsWith('.'))
  )

  // Browsing uses paths relative to the project root; the server resolves
  // them (and also accepts absolute / ~/... paths) against the session's
  // workspace, local or SSH/SFTP.
  async function loadDir(path: string = '') {
    loading = true
    error = ''
    file = null
    try {
      const res = await apiFetch(
        `/api/sessions/${sessionKey}/files?path=${encodeURIComponent(path)}`
      )
      const data = await res.json()
      if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`)
      dirPath = path
      resolvedDir = data.path
      entries = data.entries
    } catch (e: any) {
      error = e.message || String(e)
    } finally {
      loading = false
    }
  }

  async function loadFile(path: string) {
    loading = true
    error = ''
    try {
      const res = await apiFetch(
        `/api/sessions/${sessionKey}/file?path=${encodeURIComponent(path)}`
      )
      const data = await res.json()
      if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`)
      file = data
    } catch (e: any) {
      error = e.message || String(e)
    } finally {
      loading = false
    }
  }

  function open(entry: DirEntry) {
    const next = dirPath ? `${dirPath.replace(/\/+$/, '')}/${entry.name}` : entry.name
    if (entry.is_dir) {
      loadDir(next)
    } else {
      loadFile(next)
    }
  }

  function relPath(entry: DirEntry): string {
    return dirPath ? `${dirPath.replace(/\/+$/, '')}/${entry.name}` : entry.name
  }

  // 下载单个文件：后端返回原始字节，前端用 blob + a.download 触发保存。
  async function downloadFile(entry: DirEntry) {
    try {
      const res = await apiFetch(
        `/api/sessions/${sessionKey}/file/download?path=${encodeURIComponent(relPath(entry))}`
      )
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        error = data.error || `HTTP ${res.status}`
        return
      }
      const blob = await res.blob()
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = entry.name
      document.body.appendChild(a)
      a.click()
      a.remove()
      URL.revokeObjectURL(url)
    } catch (e: any) {
      error = e?.message || String(e)
    }
  }

  function goUp() {
    const trimmed = dirPath.replace(/\/+$/, '')
    const idx = trimmed.lastIndexOf('/')
    loadDir(idx <= 0 ? '' : trimmed.slice(0, idx))
  }

  interface Crumb {
    label: string
    // null = current location (not clickable)
    target: string | null
  }

  function crumbRoot(p: string): string {
    return p.startsWith('~/') ? '~' : '/'
  }

  // Breadcrumb over a resolved absolute (or ~/) path; every segment but the
  // last navigates. Works for both the directory view and the file view.
  function crumbsFor(p: string): Crumb[] {
    const home = p.startsWith('~/')
    const segs = p.replace(/^~\/?/, '').split('/').filter(Boolean)
    const base = home ? '~/' : '/'
    return segs.map((label, i) => ({
      label,
      target: i === segs.length - 1 ? null : base + segs.slice(0, i + 1).join('/'),
    }))
  }

  function backToList() {
    file = null
    error = ''
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (file) backToList()
      else onclose?.()
    }
  }

  function formatSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  }

  const EXT_LANG: Record<string, string> = {
    ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript',
    mjs: 'javascript', cjs: 'javascript', py: 'python', rs: 'rust', go: 'go',
    java: 'java', c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', hpp: 'cpp',
    cs: 'csharp', rb: 'ruby', php: 'php', swift: 'swift', kt: 'kotlin',
    sh: 'bash', bash: 'bash', zsh: 'bash', json: 'json', yaml: 'yaml',
    yml: 'yaml', toml: 'toml', ini: 'ini', md: 'markdown', html: 'xml',
    xml: 'xml', css: 'css', scss: 'scss', sql: 'sql', vue: 'xml',
    svelte: 'xml', makefile: 'makefile', dockerfile: 'dockerfile',
  }

  function langFor(path: string): string {
    const name = path.split('/').pop()?.toLowerCase() || ''
    if (EXT_LANG[name]) return EXT_LANG[name]
    const ext = name.includes('.') ? name.split('.').pop()! : ''
    return EXT_LANG[ext] || ''
  }

  const MAX_HIGHLIGHT_BYTES = 200 * 1024

  // null = render as plain text (binary/empty/too large/no highlight available)
  const codeHtml = $derived.by(() => {
    if (!file?.content) return null
    const lang = langFor(file.path)
    try {
      if (lang && hljs.getLanguage(lang)) {
        return hljs.highlight(file.content, { language: lang, ignoreIllegals: true }).value
      }
      if (file.content.length < MAX_HIGHLIGHT_BYTES) {
        return hljs.highlightAuto(file.content).value
      }
    } catch {
      // fall through to plain text
    }
    return null
  })

  onMount(() => {
    loadDir('')
  })
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="fixed inset-0 z-50 flex justify-end">
  <!-- Backdrop -->
  <div
    class="absolute inset-0 bg-black/60"
    onclick={() => (file ? backToList() : onclose?.())}
    aria-hidden="true"
  ></div>

  <!-- Panel -->
  <div class="relative w-full max-w-3xl h-full bg-gray-900 border-l border-gray-700 flex flex-col shadow-2xl">
    <!-- Header -->
    <div class="flex items-center gap-2 px-4 py-3 border-b border-gray-700/50 shrink-0">
      {#if file || resolvedDir}
        {@const crumbPathValue = file ? file.path : resolvedDir}
        <button
          onclick={file ? backToList : goUp}
          disabled={!file && (!dirPath || dirPath === '/')}
          class="text-gray-400 hover:text-white disabled:text-gray-600 disabled:cursor-not-allowed transition-colors shrink-0"
          title={file ? '返回目录' : '上级目录'}
        >
          <ChevronLeft class="w-4 h-4" />
        </button>
        <nav class="flex items-center gap-0.5 flex-1 min-w-0 overflow-x-auto font-mono text-xs">
          <button
            onclick={() => loadDir(crumbRoot(crumbPathValue))}
            class="text-gray-500 hover:text-white shrink-0"
            title={crumbRoot(crumbPathValue)}
          >
            {crumbRoot(crumbPathValue)}
          </button>
          {#each crumbsFor(crumbPathValue) as crumb, i}
            {#if i > 0}
              <span class="text-gray-600 shrink-0">/</span>
            {/if}
            {#if crumb.target}
              <button
                onclick={() => loadDir(crumb.target!)}
                class="truncate max-w-[10rem] shrink-0 text-gray-400 hover:text-white transition-colors"
                title={crumb.target}
              >
                {crumb.label}
              </button>
            {:else}
              <span
                class="truncate max-w-[10rem] shrink-0 text-gray-200"
                title={crumbPathValue}
              >
                {crumb.label}
              </span>
            {/if}
          {/each}
        </nav>
      {:else}
        <span class="text-xs text-gray-500 font-mono truncate flex-1">...</span>
      {/if}
      {#if file}
        <span class="text-xs text-gray-500 shrink-0">{formatSize(file.size)}</span>
      {:else}
        <button
          onclick={() => (showHidden = !showHidden)}
          class="text-gray-400 hover:text-white transition-colors shrink-0"
          title={showHidden ? '隐藏隐藏目录' : '显示隐藏目录'}
        >
          {#if showHidden}
            <Eye class="w-4 h-4" />
          {:else}
            <EyeOff class="w-4 h-4" />
          {/if}
        </button>
        <button
          onclick={() => loadDir(dirPath)}
          class="text-gray-400 hover:text-white transition-colors shrink-0"
          title="刷新"
        >
          <RefreshCw class="w-4 h-4" />
        </button>
      {/if}
      <button
        onclick={() => onclose?.()}
        class="text-gray-400 hover:text-white transition-colors shrink-0"
        title="关闭"
      >
        <X class="w-4 h-4" />
      </button>
    </div>

    <!-- Body -->
    <div class="flex-1 overflow-auto min-h-0">
      {#if loading}
        <div class="flex items-center justify-center h-32 text-gray-500">
          <Loader2 class="w-5 h-5 animate-spin" />
        </div>
      {:else if error}
        <div class="flex flex-col items-center justify-center h-32 gap-2 text-gray-500 text-sm">
          <FileWarning class="w-6 h-6" />
          <span>{error}</span>
        </div>
      {:else if file}
        {#if file.is_binary}
          <div class="flex flex-col items-center justify-center h-32 gap-2 text-gray-500 text-sm">
            <FileWarning class="w-6 h-6" />
            <span>二进制文件，无法预览（{formatSize(file.size)}）</span>
          </div>
        {:else if file.content !== null}
          {#if file.truncated}
            <div class="px-4 py-2 text-xs text-amber-400 bg-amber-900/20 border-b border-amber-900/30">
              文件超过 1 MB，仅显示前 1 MB 内容
            </div>
          {/if}
          {#if codeHtml !== null}
            <pre class="hljs text-xs leading-relaxed p-4 m-0 overflow-x-auto"><code>{@html codeHtml}</code></pre>
          {:else}
            <!-- highlight unavailable: plain escaped text -->
            <pre class="text-xs leading-relaxed p-4 m-0 overflow-x-auto text-gray-300 whitespace-pre">{file.content}</pre>
          {/if}
        {/if}
      {:else if visibleEntries.length === 0}
        <div class="flex items-center justify-center h-32 text-gray-500 text-sm">空目录</div>
      {:else}
        <ul class="divide-y divide-gray-800/60">
          {#each visibleEntries as entry (entry.name)}
            <li class="flex items-center hover:bg-gray-800/60 transition-colors">
              <button
                onclick={() => open(entry)}
                class="flex items-center gap-2 flex-1 min-w-0 px-4 py-1.5 text-sm text-left"
              >
                {#if entry.is_dir}
                  <Folder class="w-4 h-4 text-blue-400 shrink-0" />
                  <span class="text-gray-200 truncate">{entry.name}</span>
                {:else}
                  <FileCode class="w-4 h-4 text-gray-500 shrink-0" />
                  <span class="text-gray-400 truncate">{entry.name}</span>
                  <span class="ml-auto text-xs text-gray-600 shrink-0">{formatSize(entry.size)}</span>
                {/if}
              </button>
              {#if !entry.is_dir}
                <button
                  onclick={() => downloadFile(entry)}
                  class="p-1.5 mr-3 text-gray-500 hover:text-blue-400 hover:bg-gray-800 rounded transition-colors shrink-0"
                  title="下载"
                  aria-label="下载 {entry.name}"
                >
                  <Download class="w-4 h-4" />
                </button>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  </div>
</div>
