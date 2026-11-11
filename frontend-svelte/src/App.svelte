<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { initBridge } from '$lib/wsBridge'
  import { subscribeWSStatus } from '$lib/wsPool'
  import { fetchPending, totalPendingCount } from '$lib/stores/approval'
  import { startFaviconAlert, stopFaviconAlert } from '$lib/favicon'
  import { pageTitle } from '$lib/pageTitle'
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

  $effect(() => {
    const brand = `${$_('brand.name')} ${$_('brand.tagline')}`
    document.title = $pageTitle ? `${$pageTitle} | ${brand}` : brand
  })

  $effect(() => {
    if ($totalPendingCount > 0) startFaviconAlert()
    else stopFaviconAlert()
  })

  onMount(() => {
    // Initialize WebSocket bridge
    initBridge()

    // Bootstrap pending decisions (badge/panel survive page refresh)
    fetchPending()

    // Re-bootstrap after a WS reconnect — events broadcast while offline were missed
    let wasConnected = false
    let refetchTimer: ReturnType<typeof setTimeout> | undefined
    const unsubscribe = subscribeWSStatus(undefined, (status) => {
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
