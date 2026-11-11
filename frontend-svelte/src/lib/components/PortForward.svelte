<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { apiFetch } from '$lib/api'
  import { X, RefreshCw, ExternalLink, PlugZap, Unplug, Waypoints, Copy, Check, Trash2 } from 'lucide-svelte'

  interface ListeningPort {
    port: number
    address: string
    process: string | null
  }
  interface ForwardEntry {
    id: string
    session_key: string
    mode: string
    hostname: string
    remote_port: number
    local_port: number
    bind_lan: boolean
    created_at: number
  }
  interface TunnelEntry {
    id: string
    session_key: string
    remote_port: number
    connected: boolean
    created_at: number
  }
  // 每行独立的转发配置 + 测试/连接状态（key = 远端端口）
  interface RowState {
    localPort: string
    bindLan: boolean
    testing: boolean
    testResult: { ok: boolean; reason?: string; detail?: string } | null
    creating: boolean
    createError: string
    createErrorCode: string
  }
  interface Row {
    port: number
    address: string
    process: string | null
    forward: ForwardEntry | undefined
  }

  interface Props {
    sessionKey: string
    onclose?: () => void
  }

  let { sessionKey, onclose }: Props = $props()

  // 扫描状态
  let ports = $state<ListeningPort[]>([])
  let scanning = $state(false)
  let scanError = $state('')
  let scanMode = $state<'local' | 'ssh' | ''>('')
  let scanHostname = $state('')

  // 活跃转发
  let forwards = $state<ForwardEntry[]>([])

  // 隧道（用户本机 listener，millet-tunnel CLI 经 WS 连入）
  let tunnels = $state<TunnelEntry[]>([])
  // port → token（创建/幂等创建时返回；列表接口永不返回 token）
  let tunnelTokens = $state<Record<number, string>>({})
  // 展开了隧道子行的端口
  let openTunnelRows = $state<Record<number, boolean>>({})
  let copiedPort = $state<number | null>(null)

  // 端口打开方式：proxy = 服务端反向代理（默认，用户侧零安装）；
  // tunnel = 客户端隧道（127.0.0.1，需本机跑 millet-tunnel）
  let openMode = $state<'proxy' | 'tunnel'>('proxy')
  let openingProxyPort = $state<number | null>(null)

  // 行状态表：key = 远端端口
  let rowStates = $state<Record<number, RowState>>({})

  // 表格行 = 扫描端口 ∪ 已建转发 ∪ 已建隧道（远端端口已不在监听列表的也保留为行）
  const rows = $derived.by(() => {
    const list: Row[] = ports.map((p) => ({
      port: p.port,
      address: p.address,
      process: p.process,
      forward: forwards.find((f) => f.remote_port === p.port),
    }))
    const orphans = forwards
      .filter((f) => !ports.some((p) => p.port === f.remote_port))
      .map((f) => ({ port: f.remote_port, address: '', process: null, forward: f }))
    const covered = new Set([...list, ...orphans].map((r) => r.port))
    const tunnelOrphans = tunnels
      .filter((t) => !covered.has(t.remote_port))
      .map((t) => ({ port: t.remote_port, address: '', process: null, forward: undefined }))
    return [...list, ...orphans, ...tunnelOrphans]
  })

  // 行出现时补默认状态（保留刷新前已编辑的值）
  $effect(() => {
    for (const r of rows) {
      if (!rowStates[r.port]) {
        rowStates[r.port] = {
          localPort: String(r.port),
          bindLan: false,
          testing: false,
          testResult: null,
          creating: false,
          createError: '',
          createErrorCode: '',
        }
      }
    }
  })

  onMount(() => {
    scan()
    loadForwards()
    loadTunnels()
    loadOpenMode()
  })

  // 有子行展开时轮询隧道连接状态（CLI 是否在线）
  $effect(() => {
    if (!Object.values(openTunnelRows).some(Boolean)) return
    const timer = setInterval(loadTunnels, 3000)
    return () => clearInterval(timer)
  })

  async function scan() {
    scanning = true
    scanError = ''
    try {
      const res = await apiFetch('/api/forwards/scan', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_key: sessionKey }),
      })
      const data = await res.json()
      if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`)
      ports = data.ports
      scanMode = data.mode
      scanHostname = data.hostname
    } catch (e: any) {
      scanError = e.message || String(e)
      ports = []
    } finally {
      scanning = false
    }
  }

  async function loadForwards() {
    try {
      const res = await apiFetch(`/api/forwards?session_key=${encodeURIComponent(sessionKey)}`)
      const data = await res.json()
      if (res.ok) forwards = data.forwards
    } catch {
      // 列表加载失败不打断面板，删除/创建时还会重试
    }
  }

  async function loadTunnels() {
    try {
      const res = await apiFetch(`/api/tunnels?session_key=${encodeURIComponent(sessionKey)}`)
      const data = await res.json()
      if (res.ok) tunnels = data.tunnels
    } catch {
      // 同上，轮询失败静默跳过
    }
  }

  // 打开方式设置（未设置时 404，保持默认 proxy）
  async function loadOpenMode() {
    try {
      const res = await apiFetch('/api/config/settings.forwards.open_mode')
      if (!res.ok) return
      const data = await res.json()
      if (data === 'tunnel') openMode = 'tunnel'
    } catch {
      // 读取失败保持默认
    }
  }

  function tunnelOf(port: number): TunnelEntry | undefined {
    return tunnels.find((t) => t.remote_port === port)
  }

  async function toggleTunnel(port: number) {
    if (openTunnelRows[port]) {
      openTunnelRows[port] = false
      return
    }
    openTunnelRows[port] = true
    // 幂等创建：已存在则返回原 token，子行命令始终完整
    try {
      const res = await apiFetch('/api/tunnels', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_key: sessionKey, remote_port: port }),
      })
      const data = await res.json()
      if (res.ok) tunnelTokens[port] = data.token
    } catch {
      // 创建失败不打断面板，命令里的 token 留空提示重试
    }
    await loadTunnels()
  }

  function tunnelCommand(port: number): string {
    return `millet-tunnel --server ${location.origin} --listen 127.0.0.1:${port} --token ${tunnelTokens[port] ?? ''}`
  }

  async function copyTunnelCommand(port: number) {
    try {
      await navigator.clipboard.writeText(tunnelCommand(port))
      copiedPort = port
      setTimeout(() => {
        if (copiedPort === port) copiedPort = null
      }, 3000)
    } catch {
      // 剪贴板被拒绝时用户可手动选择复制
    }
  }

  async function deleteTunnel(port: number) {
    const t = tunnelOf(port)
    if (!t) return
    try {
      const res = await apiFetch(`/api/tunnels/${t.id}`, { method: 'DELETE' })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || `HTTP ${res.status}`)
      }
    } catch (e: any) {
      alert(e.message || String(e))
      return
    }
    openTunnelRows[port] = false
    delete tunnelTokens[port]
    await loadTunnels()
  }

  function localPortValid(st: RowState): boolean {
    const n = Number(st.localPort)
    return Number.isInteger(n) && n >= 1 && n <= 65535
  }

  async function runTest(port: number) {
    const st = rowStates[port]
    if (!st || !localPortValid(st) || st.testing) return
    st.testing = true
    st.testResult = null
    try {
      const res = await apiFetch('/api/forwards/test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          session_key: sessionKey,
          remote_port: port,
          local_port: Number(st.localPort),
          bind_lan: st.bindLan,
        }),
      })
      st.testResult = await res.json()
    } catch (e: any) {
      st.testResult = { ok: false, reason: 'remote_not_listening', detail: e.message || String(e) }
    } finally {
      st.testing = false
    }
  }

  async function createForward(port: number) {
    const st = rowStates[port]
    if (!st || !localPortValid(st) || !st.testResult?.ok || st.creating) return
    st.creating = true
    st.createError = ''
    st.createErrorCode = ''
    try {
      const res = await apiFetch('/api/forwards', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          session_key: sessionKey,
          remote_port: port,
          local_port: Number(st.localPort),
          bind_lan: st.bindLan,
        }),
      })
      const data = await res.json()
      if (!res.ok) {
        st.createErrorCode = data.code || ''
        throw new Error(data.error || `HTTP ${res.status}`)
      }
      st.testResult = null
      await loadForwards()
    } catch (e: any) {
      st.createError = e.message || String(e)
    } finally {
      st.creating = false
    }
  }

  async function deleteForward(id: string) {
    try {
      const res = await apiFetch(`/api/forwards/${id}`, { method: 'DELETE' })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || `HTTP ${res.status}`)
      }
    } catch (e: any) {
      alert(e.message || String(e))
      return
    }
    await loadForwards()
  }

  // 打开链接固定 127.0.0.1（本机访问）；开局域网后同伴自行用主机 IP 访问
  function forwardUrl(entry: ForwardEntry): string {
    return `http://127.0.0.1:${entry.local_port}`
  }

  function openLink(entry: ForwardEntry) {
    window.open(forwardUrl(entry), '_blank')?.focus()
  }

  // 反代模式：签发带 _pwt 的 /p/ 链接后新开标签页（服务端转发，无需本机安装）
  async function openProxyLink(port: number) {
    if (openingProxyPort !== null) return
    openingProxyPort = port
    try {
      const res = await apiFetch('/api/forwards/proxy-link', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_key: sessionKey, remote_port: port }),
      })
      const data = await res.json()
      if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`)
      window.open(data.url, '_blank')?.focus()
    } catch (e: any) {
      alert(e.message || String(e))
    } finally {
      openingProxyPort = null
    }
  }

  function reasonText(reason?: string): string {
    switch (reason) {
      case 'local_port_busy':
        return $_('portForward.reasonLocalPortBusy')
      case 'remote_not_listening':
        return $_('portForward.reasonRemoteNotListening')
      default:
        return reason || $_('portForward.testFailed')
    }
  }

  function createErrorText(st: RowState): string {
    switch (st.createErrorCode) {
      case 'duplicate':
        return $_('portForward.errDuplicate')
      case 'local_port_busy':
        return $_('portForward.reasonLocalPortBusy')
      case 'remote_unreachable':
        return $_('portForward.errRemoteUnreachable')
      default:
        return st.createError
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') onclose?.()
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="fixed inset-0 z-50 flex justify-end">
  <!-- Backdrop -->
  <div class="absolute inset-0 bg-black/60" onclick={() => onclose?.()} aria-hidden="true"></div>

  <!-- Panel -->
  <div class="relative w-full max-w-xl h-full bg-gray-900 border-l border-gray-700 flex flex-col shadow-2xl">
    <!-- Header -->
    <div class="flex items-center gap-2 px-4 py-3 border-b border-gray-700/50 shrink-0">
      <h2 class="text-sm font-semibold text-white flex-1">{$_('portForward.title')}</h2>
      <button
        onclick={() => onclose?.()}
        class="text-gray-400 hover:text-white transition-colors"
        title={$_('common.close')}
      >
        <X class="w-4 h-4" />
      </button>
    </div>

    <div class="flex-1 overflow-y-auto px-4 py-3 space-y-3">
      <!-- 当前打开方式说明（设置页可切换） -->
      <p class="text-[11px] text-gray-500">
        {openMode === 'proxy' ? $_('portForward.modeProxyHint') : $_('portForward.modeTunnelHint')}
      </p>
      <!-- Section: 监听端口 -->
      <section>
        <div class="flex items-center gap-2 mb-2">
          <h3 class="text-xs font-semibold text-gray-400 uppercase">{$_('portForward.scanTitle')}</h3>
          {#if scanMode}
            <span class="text-[10px] font-mono text-blue-400 bg-blue-900/20 px-1.5 py-0.5 rounded">
              {scanMode}{scanHostname ? ` · ${scanHostname}` : ''}
            </span>
          {/if}
          <button
            onclick={scan}
            disabled={scanning}
            class="ml-auto flex items-center gap-1 px-2 py-1 text-xs rounded border border-gray-700 text-gray-300 hover:bg-gray-800 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            <RefreshCw class="w-3 h-3 {scanning ? 'animate-spin' : ''}" />
            {scanning ? $_('portForward.scanning') : $_('portForward.refresh')}
          </button>
        </div>

        {#if scanError}
          <p class="text-xs text-red-400 mb-2">{$_('portForward.scanFailed')}: {scanError}</p>
        {/if}

        {#if scanning && rows.length === 0}
          <p class="text-xs text-gray-500">{$_('portForward.scanning')}</p>
        {:else if rows.length === 0}
          <p class="text-xs text-gray-500">{$_('portForward.noPorts')}</p>
        {:else}
          <div class="border border-gray-800 rounded">
            <!-- 标题栏 -->
            <div class="grid grid-cols-[minmax(0,1fr)_3.5rem_5rem_4rem_8.5rem] gap-2 items-center px-3 py-1.5 text-[11px] font-medium text-gray-400 bg-gray-800/50 border-b border-gray-800 select-none">
              <span>{$_('portForward.colProcess')}</span>
              <span>{$_('portForward.colPort')}</span>
              <span>{$_('portForward.colLocalPort')}</span>
              <span class="text-center">{$_('portForward.colLan')}</span>
              <span>{$_('portForward.colAction')}</span>
            </div>
            <div class="divide-y divide-gray-800">
              {#each rows as row (row.port)}
                {@const st = rowStates[row.port]}
                {#if st}
                  <div class="px-3 py-2">
                    <div class="grid grid-cols-[minmax(0,1fr)_3.5rem_5rem_4rem_8.5rem] gap-2 items-center">
                      <!-- 进程 -->
                      <span class="text-xs text-gray-400 truncate" title={row.process ?? ''}
                        >{row.process ?? '—'}</span
                      >
                      <!-- 端口 -->
                      <span class="font-mono text-sm text-white" title={row.address}>{row.port}</span>
                      <!-- 本地映射端口 -->
                      {#if row.forward}
                        <span class="font-mono text-sm text-emerald-400">{row.forward.local_port}</span>
                      {:else}
                        <input
                          type="number"
                          min="1"
                          max="65535"
                          value={st.localPort}
                          oninput={(e) => {
                            st.localPort = e.currentTarget.value
                            st.testResult = null
                          }}
                          class="w-full px-1.5 py-1 bg-gray-800 border border-gray-700 rounded text-xs text-white font-mono focus:outline-none focus:border-blue-500 {st.localPort && !localPortValid(st)
                            ? 'border-red-500'
                            : ''}"
                          placeholder="1024-65535"
                          aria-label={$_('portForward.colLocalPort')}
                        />
                      {/if}
                      <!-- 允许局域网（滑动开关） -->
                      {#if row.forward}
                        <label
                          class="relative inline-flex items-center h-5 mx-auto cursor-default opacity-80"
                          title={$_('portForward.colLan')}
                        >
                          <input
                            type="checkbox"
                            class="sr-only"
                            checked={row.forward.bind_lan}
                            disabled
                            aria-label={$_('portForward.colLan')}
                          />
                          <span
                            class="w-9 h-5 rounded-full transition-colors duration-200 {row.forward
                              .bind_lan
                              ? 'bg-blue-500'
                              : 'bg-gray-600'}"
                          ></span>
                          <span
                            class="absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full shadow transition-transform duration-200 {row
                              .forward.bind_lan
                              ? 'translate-x-4'
                              : ''}"
                          ></span>
                        </label>
                      {:else}
                        <label
                          class="relative inline-flex items-center h-5 mx-auto cursor-pointer"
                          title={$_('portForward.colLan')}
                        >
                          <input
                            type="checkbox"
                            class="sr-only"
                            checked={st.bindLan}
                            onchange={(e) => {
                              st.bindLan = e.currentTarget.checked
                              st.testResult = null
                            }}
                            aria-label={$_('portForward.colLan')}
                          />
                          <span
                            class="w-9 h-5 rounded-full transition-colors duration-200 {st.bindLan
                              ? 'bg-blue-500'
                              : 'bg-gray-600'}"
                          ></span>
                          <span
                            class="absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full shadow transition-transform duration-200 {st.bindLan
                              ? 'translate-x-4'
                              : ''}"
                          ></span>
                        </label>
                      {/if}
                      <!-- 操作区：打开 → 测试/连接/断开 → 隧道 -->
                      <div class="flex items-center gap-1">
                        {#if openMode === 'proxy'}
                          <!-- 反代模式：打开图标恒可见（服务端转发，无需本地 listener） -->
                          <button
                            onclick={() => openProxyLink(row.port)}
                            disabled={openingProxyPort === row.port}
                            class="p-1 text-blue-400 hover:text-blue-300 disabled:opacity-50 transition-colors"
                            title={$_('portForward.openViaProxy')}
                          >
                            <ExternalLink class="w-3.5 h-3.5" />
                          </button>
                        {:else if row.forward}
                          <button
                            onclick={() => openLink(row.forward!)}
                            class="p-1 text-blue-400 hover:text-blue-300 transition-colors"
                            title={forwardUrl(row.forward)}
                          >
                            <ExternalLink class="w-3.5 h-3.5" />
                          </button>
                        {/if}
                        {#if row.forward}
                          <button
                            onclick={() => deleteForward(row.forward!.id)}
                            class="flex items-center gap-1 px-2.5 py-1 text-xs rounded bg-red-600 hover:bg-red-500 text-white transition-colors"
                            title={forwardUrl(row.forward)}
                          >
                            <Unplug class="w-3.5 h-3.5" />
                            {$_('portForward.disconnect')}
                          </button>
                        {:else if st.testResult?.ok}
                          <button
                            onclick={() => createForward(row.port)}
                            disabled={st.creating || !localPortValid(st)}
                            class="flex items-center gap-1 px-2.5 py-1 text-xs rounded bg-blue-600 hover:bg-blue-500 text-white disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                          >
                            <PlugZap class="w-3.5 h-3.5" />
                            {st.creating ? $_('portForward.creating') : $_('portForward.connect')}
                          </button>
                        {:else}
                          <button
                            onclick={() => runTest(row.port)}
                            disabled={!localPortValid(st) || st.testing}
                            class="px-2.5 py-1 text-xs rounded bg-gray-800 hover:bg-gray-700 border border-gray-700 text-gray-200 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                          >
                            {st.testing ? $_('portForward.testing') : $_('portForward.test')}
                          </button>
                        {/if}
                        <!-- 隧道：用户本机 listener（millet-tunnel CLI） -->
                        <button
                          onclick={() => toggleTunnel(row.port)}
                          class="p-1 {tunnelOf(row.port) ? 'text-blue-400 hover:text-blue-300' : 'text-gray-400 hover:text-white'} transition-colors"
                          title={$_('portForward.tunnel')}
                        >
                          <Waypoints class="w-3.5 h-3.5" />
                        </button>
                      </div>
                    </div>
                    <!-- 行内错误反馈 -->
                    {#if st.testResult && !st.testResult.ok}
                      <p class="text-[11px] text-red-400 mt-1">{reasonText(st.testResult.reason)}</p>
                    {/if}
                    {#if st.createError}
                      <p class="text-[11px] text-red-400 mt-1">{createErrorText(st)}</p>
                    {/if}
                    <!-- 隧道子行：CLI 命令 + 连接状态 -->
                    {#if openTunnelRows[row.port]}
                      <div class="mt-2 rounded bg-gray-800/60 border border-gray-700/50 p-2 space-y-1.5">
                        <p class="text-[11px] text-gray-400">{$_('portForward.tunnelHint')}</p>
                        <div class="flex items-center gap-1.5">
                          <code
                            class="flex-1 min-w-0 px-2 py-1 rounded bg-gray-900 border border-gray-700 text-[11px] font-mono text-emerald-300 overflow-x-auto whitespace-nowrap"
                          >{tunnelCommand(row.port)}</code>
                          <button
                            onclick={() => copyTunnelCommand(row.port)}
                            class="shrink-0 flex items-center gap-1 px-2 py-1 text-[11px] rounded border border-gray-700 text-gray-300 hover:bg-gray-800 transition-colors"
                          >
                            {#if copiedPort === row.port}
                              <Check class="w-3 h-3" />
                              {$_('portForward.tunnelCopied')}
                            {:else}
                              <Copy class="w-3 h-3" />
                              {$_('portForward.tunnelCopy')}
                            {/if}
                          </button>
                        </div>
                        {#if tunnelOf(row.port)}
                          <div class="flex items-center gap-2 text-[11px]">
                            <span
                              class="flex items-center gap-1 {tunnelOf(row.port)!.connected
                                ? 'text-emerald-400'
                                : 'text-gray-400'}"
                            >
                              <span
                                class="w-1.5 h-1.5 rounded-full {tunnelOf(row.port)!.connected
                                  ? 'bg-emerald-400'
                                  : 'bg-gray-500'}"
                              ></span>
                              {tunnelOf(row.port)!.connected
                                ? $_('portForward.tunnelConnected')
                                : $_('portForward.tunnelDisconnected')}
                            </span>
                            <button
                              onclick={() => deleteTunnel(row.port)}
                              class="ml-auto flex items-center gap-1 px-2 py-1 rounded border border-red-800 text-red-400 hover:bg-red-950/50 transition-colors"
                            >
                              <Trash2 class="w-3 h-3" />
                              {$_('portForward.tunnelDelete')}
                            </button>
                          </div>
                        {/if}
                      </div>
                    {/if}
                  </div>
                {/if}
              {/each}
            </div>
          </div>
        {/if}
      </section>
    </div>
  </div>
</div>
