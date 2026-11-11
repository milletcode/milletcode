<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { FolderOpen, MessageSquare, Bell, Settings as SettingsIcon, Package, Globe, PanelLeftClose, PanelLeftOpen, LogOut } from 'lucide-svelte'
  import { totalPendingCount } from '$lib/stores/approval'
  import { locale } from 'svelte-i18n'
  import { apiFetch } from '$lib/api'
  import logo64 from '../../assets/logo-64.png'

  interface Props {
    onNavigate: (path: string) => void
    currentRoute: string
  }

  let { onNavigate, currentRoute }: Props = $props()

  let collapsed = $state(false)

  // Status indicator state
  let sessionCount = $state(0)
  let taskCount = $state(0)
  let processCount = $state(0)
  let showSessionTooltip = $state(false)
  let showTaskTooltip = $state(false)
  let showProcessTooltip = $state(false)
  let version = $state('')

  // git describe 输出可能是 "v0.9.2-2-gd42c075"：主版本 v0.9.2，
  // 副版本 "+2·d42c075"（落后 tag 的 commit 数 + 哈希）小字单独展示，
  // 避免长后缀撑坏为 vX.X.X 设计的排版。
  const versionParts = $derived.by(() => {
    const m = version.match(/^(.+)-(\d+)-g([0-9a-f]+)$/)
    if (!m) return { main: version, sub: '', count: 0, hash: '' }
    return { main: m[1], sub: `+${m[2]}·${m[3]}`, count: Number(m[2]), hash: m[3] }
  })

  let pollInterval: ReturnType<typeof setInterval> | null = null

  onMount(() => {
    const saved = localStorage.getItem('sidebar-collapsed')
    collapsed = saved === 'true'

    // Fetch version from version.json (git tag driven)
    // no-store：version.json 是固定文件名，旧版部署曾给它发 immutable，浏览器
    // 半年内都不回源，Sidebar 就一直显示第一次拿到的版本号。这里显式绕过 HTTP
    // 缓存，已经中过招的浏览器不用强刷也能立刻恢复。
    fetch('/version.json', { cache: 'no-store' })
      .then((r) => r.json())
      .then((data) => { version = data.version || '' })
      .catch(() => {})

    // Poll all stats from server
    const fetchStats = async () => {
      try {
        const res = await apiFetch('/api/settings/online')
        const data = await res.json()
        sessionCount = data.total_connections || 0
        taskCount = data.total_subscriptions || 0
        processCount = data.process_count || 0
      } catch {
        sessionCount = 0
        taskCount = 0
        processCount = 0
      }
    }
    fetchStats()
    pollInterval = setInterval(fetchStats, 120000) // 2 minutes
  })

  onDestroy(() => {
    if (pollInterval) clearInterval(pollInterval)
  })

  $effect(() => {
    localStorage.setItem('sidebar-collapsed', String(collapsed))
  })

  const navItems = $derived([
    { path: '/dashboard', label: $_('nav.projects'), icon: FolderOpen },
    { path: '/sessions', label: $_('nav.sessions'), icon: MessageSquare },
    {
      path: '/approvals',
      label: $_('nav.messages'),
      icon: Bell,
      badge: $totalPendingCount > 0 ? ($totalPendingCount > 10 ? '10+' : String($totalPendingCount)) : undefined,
    },
    { path: '/skills', label: $_('nav.skills'), icon: Package },
  ])

  function switchLanguage() {
    $locale = $locale === 'zh-CN' ? 'en' : 'zh-CN'
    localStorage.setItem('lang', $locale)
  }

  function isActive(path: string): boolean {
    return currentRoute === path || (path === '/dashboard' && currentRoute === '/')
  }

  // alt / ctrl / cmd + click mirrors the browser convention for <a target="_blank">
  function handleNavClick(e: MouseEvent, path: string) {
    if (e.altKey || e.ctrlKey || e.metaKey) {
      // 同源页面无 tabnabbing 风险；noopener/置空 opener 会让 Chromium 把
      // 新标签放到后台，这里显式 focus 保证前台打开
      window.open(path, '_blank')?.focus()
      return
    }
    onNavigate(path)
  }

  function toggleSessionTooltip() {
    showSessionTooltip = !showSessionTooltip
    showTaskTooltip = false
    showProcessTooltip = false
  }

  function toggleTaskTooltip() {
    showTaskTooltip = !showTaskTooltip
    showSessionTooltip = false
    showProcessTooltip = false
  }

  function toggleProcessTooltip() {
    showProcessTooltip = !showProcessTooltip
    showSessionTooltip = false
    showTaskTooltip = false
  }

  function handleLogout() {
    // Backend expires every dispatched cookie (host-only + configured parent
    // domain, HttpOnly included), then bounces to `/`.
    window.location.href = '/api/logout'
  }
</script>

<aside class="{collapsed ? 'w-16' : 'w-56'} bg-gray-900 border-r border-gray-800 flex flex-col transition-all duration-200">
  <div class="{collapsed ? 'px-2 py-4' : 'p-6'}">
    <div class="flex items-center {collapsed ? 'justify-center' : 'gap-3 mb-1'}">
      <img src={logo64} alt="MilletCode" class="w-8 h-8 rounded-lg flex-shrink-0" />
      {#if !collapsed}
        <div class="min-w-0">
          <h1 class="text-lg font-bold text-white whitespace-nowrap">{$_('brand.name')}</h1>
          {#if version}
            <!-- 版本号即更新日志入口 -->
            <p class="text-xs text-gray-600 font-mono">
              <button
                onclick={() => onNavigate('/changelog')}
                class="hover:text-blue-400 transition-colors"
                title={$_('changelog.title')}
              >
                {versionParts.main}
              </button>
              {#if versionParts.sub}
                <button
                  onclick={() => onNavigate('/changelog')}
                  class="text-[10px] text-amber-400/80 bg-amber-900/20 rounded px-1 py-px ml-0.5 hover:text-amber-300 transition-colors"
                  title={$_('brand.versionSuffix', { values: { count: versionParts.count, hash: versionParts.hash } })}
                >{versionParts.sub}</button>
              {/if}
            </p>
          {/if}
        </div>
      {/if}
    </div>
    {#if !collapsed}
      <p class="text-[10px] text-gray-500 whitespace-nowrap tracking-tight mb-2">{$_('brand.tagline')}</p>
    {/if}
  </div>
  <nav class="flex-1 px-2 flex flex-col">
    <div class="flex-1">
      {#each navItems as item}
        <button
          onclick={(e) => handleNavClick(e, item.path)}
          class="flex items-center {collapsed ? 'justify-center px-2' : 'px-4'} py-2.5 rounded-lg mb-1 text-sm font-medium transition-colors relative w-full {isActive(item.path)
            ? 'bg-gray-800 text-white'
            : 'text-gray-400 hover:text-white hover:bg-gray-800/50'}"
          title={collapsed ? item.label : undefined}
        >
          {#if item.icon === FolderOpen}
            <FolderOpen class="w-5 h-5 flex-shrink-0" />
          {:else if item.icon === MessageSquare}
            <MessageSquare class="w-5 h-5 flex-shrink-0" />
          {:else if item.icon === Bell}
            <Bell class="w-5 h-5 flex-shrink-0" />
          {:else if item.icon === Package}
            <Package class="w-5 h-5 flex-shrink-0" />
          {:else if item.icon === SettingsIcon}
            <SettingsIcon class="w-5 h-5 flex-shrink-0" />
          {/if}
          {#if !collapsed}
            <span class="ml-3">{item.label}</span>
          {/if}
          {#if item.badge != null}
            <span class="{collapsed ? 'absolute -top-1 -right-1' : 'absolute right-2 top-1/2 -translate-y-1/2'} inline-flex items-center justify-center min-w-5 h-5 px-1 text-[10px] font-bold text-white bg-amber-500 rounded-full animate-pulse">
              {item.badge}
            </span>
          {/if}
        </button>
      {/each}
    </div>

    <!-- Status indicators (centered, hidden when collapsed) -->
    {#if !collapsed}
      <div class="flex items-center justify-center py-3 gap-3">
        <!-- 会话数 -->
        <div class="relative">
          <button
            onclick={toggleSessionTooltip}
            class="flex items-center justify-center w-8 h-8 rounded hover:bg-gray-800/50 transition-colors"
            title={$_('connection.sessions')}
          >
            <div class="w-[16px] h-[16px] rounded-full {sessionCount > 0 ? 'bg-green-500' : 'bg-white'}"></div>
          </button>
          {#if showSessionTooltip}
            <div class="absolute bottom-full left-1/2 -translate-x-1/2 mb-1 px-2 py-1 bg-gray-700 text-white text-xs rounded whitespace-nowrap z-50">
              {$_('connection.sessions')}: {sessionCount}
            </div>
          {/if}
        </div>

        <!-- 任务数 -->
        <div class="relative">
          <button
            onclick={toggleTaskTooltip}
            class="flex items-center justify-center w-8 h-8 rounded hover:bg-gray-800/50 transition-colors"
            title={$_('connection.tasks')}
          >
            <div class="w-[16px] h-[16px] rounded-full {taskCount > 0 ? 'bg-green-500' : 'bg-white'}"></div>
          </button>
          {#if showTaskTooltip}
            <div class="absolute bottom-full left-1/2 -translate-x-1/2 mb-1 px-2 py-1 bg-gray-700 text-white text-xs rounded whitespace-nowrap z-50">
              {$_('connection.tasks')}: {taskCount}
            </div>
          {/if}
        </div>

        <!-- 进程数 -->
        <div class="relative">
          <button
            onclick={toggleProcessTooltip}
            class="flex items-center justify-center w-8 h-8 rounded hover:bg-gray-800/50 transition-colors"
            title={$_('connection.process')}
          >
            <div class="w-[16px] h-[16px] rounded-full {processCount > 0 ? 'bg-green-500' : 'bg-white'}"></div>
          </button>
          {#if showProcessTooltip}
            <div class="absolute bottom-full left-1/2 -translate-x-1/2 mb-1 px-2 py-1 bg-gray-700 text-white text-xs rounded whitespace-nowrap z-50">
              {$_('connection.process')}: {processCount}
            </div>
          {/if}
        </div>
      </div>
    {/if}
  </nav>
  <div class="pb-2 px-2 flex flex-col gap-1">
    <button
      onclick={switchLanguage}
      class="flex items-center {collapsed ? 'justify-center px-2' : 'px-4'} py-2.5 rounded-lg text-sm font-medium text-gray-500 hover:text-gray-300 hover:bg-gray-800/50 transition-colors"
      title="Switch Language / 切换语言"
    >
      <Globe class="w-5 h-5 flex-shrink-0" />
      {#if !collapsed}
        <span class="ml-3">{$locale === 'zh-CN' ? 'En' : '中文'}</span>
      {/if}
    </button>
    <button
      onclick={(e) => handleNavClick(e, '/settings')}
      class="flex items-center {collapsed ? 'justify-center px-2' : 'px-4'} py-2.5 rounded-lg mb-1 text-sm font-medium transition-colors w-full {isActive('/settings')
        ? 'bg-gray-800 text-white'
        : 'text-gray-400 hover:text-white hover:bg-gray-800/50'}"
      title={collapsed ? $_('nav.settings') : undefined}
    >
      <SettingsIcon class="w-5 h-5 flex-shrink-0" />
      {#if !collapsed}
        <span class="ml-3">{$_('nav.settings')}</span>
      {/if}
    </button>
    <button
      onclick={() => (collapsed = !collapsed)}
      class="flex items-center {collapsed ? 'justify-center px-2' : 'px-4'} py-2.5 rounded-lg text-sm font-medium text-gray-500 hover:text-gray-300 hover:bg-gray-800/50 transition-colors"
      title={collapsed ? '展开侧边栏' : '折叠侧边栏'}
    >
      {#if collapsed}
        <PanelLeftOpen class="w-5 h-5 flex-shrink-0" />
      {:else}
        <PanelLeftClose class="w-5 h-5 flex-shrink-0" />
      {/if}
      {#if !collapsed}
        <span class="ml-3">折叠</span>
      {/if}
    </button>

    <!-- Logout button -->
    <button
      onclick={handleLogout}
      class="flex items-center {collapsed ? 'justify-center px-2' : 'px-4'} py-2.5 rounded-lg text-sm font-medium text-gray-500 hover:text-red-400 hover:bg-gray-800/50 transition-colors"
      title="登出"
    >
      <LogOut class="w-5 h-5 flex-shrink-0" />
      {#if !collapsed}
        <span class="ml-3">登出</span>
      {/if}
    </button>
  </div>
</aside>
