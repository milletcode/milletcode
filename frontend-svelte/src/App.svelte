<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { initBridge } from '$lib/wsBridge'
  import { subscribeWSStatus, initGlobalWebSocket, GLOBAL_WS_KEY } from '$lib/wsPool'
  import { fetchPending } from '$lib/stores/approval'
  import { initAttention } from '$lib/attention'
  import Sidebar from '$lib/components/Sidebar.svelte'
  import NotificationBar from '$lib/components/NotificationBar.svelte'
  import DashboardRoute from './routes/DashboardRoute.svelte'
  import SessionsRoute from './routes/SessionsRoute.svelte'
  import SessionDetailRoute from './routes/SessionDetailRoute.svelte'
  import ApprovalsRoute from './routes/ApprovalsRoute.svelte'
  import SettingsRoute from './routes/SettingsRoute.svelte'
  import SkillsRoute from './routes/SkillsRoute.svelte'
  import ProjectDetailRoute from './routes/ProjectDetailRoute.svelte'
  import ProjectSettingsRoute from './routes/ProjectSettingsRoute.svelte'
  import TerminalRoute from './routes/TerminalRoute.svelte'
  import ChangelogRoute from './routes/ChangelogRoute.svelte'

  import GlobalDebugPanel from '$lib/components/GlobalDebugPanel.svelte'

  let currentRoute = $state(window.location.pathname)
  let routeParams = $state<Record<string, string>>({})
  let routeVersion = $state(0)

  onMount(() => {
    // Initialize WebSocket bridge
    initBridge()

    // Broadcast-only connection: keeps approvals/questions/turn-end markers
    // flowing on pages that hold no per-project session connection
    initGlobalWebSocket()

    // Service worker for browser notifications on Android-class browsers
    // (no page-context Notification constructor there). Also routes
    // notification clicks back into this SPA via postMessage.
    if ('serviceWorker' in navigator && window.isSecureContext) {
      navigator.serviceWorker.register('/sw.js').catch(() => {})
      navigator.serviceWorker.addEventListener('message', (e) => {
        if (e.data?.type === 'navigate') navigate(e.data.path)
      })
    }

    // Own document.title, favicon alert and browser notifications
    initAttention(navigate)

    // Bootstrap pending decisions (badge/panel survive page refresh)
    fetchPending()

    // Re-bootstrap after the broadcast channel reconnects — approvals and
    // questions broadcast while offline were missed. (The aggregate status
    // excludes the global channel, so subscribe to it by key.)
    let wasConnected = false
    let refetchTimer: ReturnType<typeof setTimeout> | undefined
    const unsubscribe = subscribeWSStatus(GLOBAL_WS_KEY, (status) => {
      const nowConnected = status === 'connected'
      if (nowConnected && !wasConnected) {
        clearTimeout(refetchTimer)
        refetchTimer = setTimeout(fetchPending, 500)
      }
      wasConnected = nowConnected
    })

    // Listen for popstate (browser back/forward)
    window.addEventListener('popstate', handleRouteChange)
    return () => {
      unsubscribe()
      clearTimeout(refetchTimer)
      window.removeEventListener('popstate', handleRouteChange)
    }
  })

  function handleRouteChange() {
    currentRoute = window.location.pathname
    parseRouteParams()
  }

  export function navigate(path: string) {
    window.history.pushState({}, '', path)
    currentRoute = path.split('?')[0]
    routeVersion++
    parseRouteParams()
  }

  function parseRouteParams() {
    // /terminal/:org/:project — project may itself contain slashes
    const terminalMatch = currentRoute.match(/^\/terminal\/([^/]+)\/(.+)$/)
    if (terminalMatch) {
      routeParams = {
        org: terminalMatch[1],
        project: terminalMatch[2],
      }
      return
    }

    // /sessions/:org/:project/:agent/:name
    const sessionMatch = currentRoute.match(/^\/sessions\/([^/]+)\/([^/]+)\/([^/]+)\/([^/]+)$/)
    if (sessionMatch) {
      routeParams = {
        org: sessionMatch[1],
        project: sessionMatch[2],
        agent: sessionMatch[3],
        name: sessionMatch[4],
      }
      return
    }

    // /projects/:org/:project/settings
    const projectSettingsMatch = currentRoute.match(/^\/projects\/([^/]+)\/([^/]+)\/settings$/)
    if (projectSettingsMatch) {
      routeParams = {
        org: projectSettingsMatch[1],
        project: projectSettingsMatch[2],
      }
      return
    }

    // /projects/:org/:project
    const projectMatch = currentRoute.match(/^\/projects\/([^/]+)\/([^/]+)$/)
    if (projectMatch) {
      routeParams = {
        org: projectMatch[1],
        project: projectMatch[2],
      }
      return
    }

    routeParams = {}
  }

  // Parse initial route
  parseRouteParams()
</script>

<div class="flex h-screen bg-gray-950 text-gray-100">
  {#if currentRoute === '/terminal' || (currentRoute.startsWith('/terminal/') && Object.keys(routeParams).length === 2)}
    <!-- Terminal window: full-screen, no sidebar -->
    <div class="flex-1 overflow-hidden">
      <TerminalRoute {navigate} params={routeParams} />
    </div>
  {:else}
    <Sidebar onNavigate={navigate} currentRoute={currentRoute} />
    <div class="flex-1 flex flex-col overflow-hidden">
      <NotificationBar />
      <main class="flex-1 overflow-auto p-6">
        {#if currentRoute === '/' || currentRoute === '/dashboard'}
          <DashboardRoute {navigate} />
        {:else if currentRoute.startsWith('/sessions/') && Object.keys(routeParams).length === 4}
          <SessionDetailRoute params={routeParams} {navigate} />
        {:else if currentRoute === '/sessions'}
          {#key routeVersion}
            <SessionsRoute {navigate} />
          {/key}
        {:else if currentRoute.startsWith('/projects/') && Object.keys(routeParams).length === 2}
          {#if currentRoute.endsWith('/settings')}
            <ProjectSettingsRoute params={routeParams} {navigate} />
          {:else}
            <ProjectDetailRoute params={routeParams} {navigate} />
          {/if}
        {:else if currentRoute === '/approvals'}
          <ApprovalsRoute {navigate} />
        {:else if currentRoute === '/skills'}
          <SkillsRoute {navigate} />
        {:else if currentRoute === '/settings'}
          <SettingsRoute />
        {:else if currentRoute === '/changelog'}
          <ChangelogRoute />
        {:else}
          <div class="text-center text-gray-500 mt-20">
            <h1 class="text-2xl font-bold mb-4">{$_('error.notFound')}</h1>
            <p>{$_('error.pageNotFound')}</p>
          </div>
        {/if}
      </main>
    </div>
    <GlobalDebugPanel />
  {/if}
</div>
