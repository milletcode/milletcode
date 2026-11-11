<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { StopCircle, Play, RefreshCw, Trash2 } from 'lucide-svelte'
  import { sessions, fetchSessions, viewMode, setViewMode, type Session } from '$lib/stores/session'
  import SessionRow from '$lib/components/SessionRow.svelte'

  interface Props {
    navigate: (path: string) => void
  }

  let { navigate }: Props = $props()

  let search = $state('')
  let resumingId = $state<string | null>(null)

  onMount(async () => {
    // Always refresh sessions when this component mounts
    await fetchSessions()
  })

  const filteredSessions = $derived(
    search.trim()
      ? $sessions.filter((s) => {
          const q = search.trim().toLowerCase()
          return (
            s.agent_id.toLowerCase().includes(q) ||
            s.prompt.toLowerCase().includes(q) ||
            s.id.toLowerCase().includes(q)
          )
        })
      : $sessions
  )

  const listSessions = $derived(
    [...filteredSessions].sort((a, b) => {
      const at = (a as any).create_time || 0
      const bt = (b as any).create_time || 0
      return bt - at
    })
  )

  // path -> [project -> sessions], both levels sorted, sessions newest first
  const grouped = $derived.by<[string, [string, Session[]][]][]>(() => {
    const paths = new Map<string, Map<string, Session[]>>()
    for (const s of filteredSessions) {
      // Composite key "org/project/agent/name" — org is the discovery path keyword
      const seg = s.id.split('/')
      const path = seg[0] || 'Unknown'
      const project = seg[1] || 'Unknown'
      if (!paths.has(path)) paths.set(path, new Map())
      const projects = paths.get(path)!
      if (!projects.has(project)) projects.set(project, [])
      projects.get(project)!.push(s)
    }
    return Array.from(paths.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([path, projects]) => [
        path,
        Array.from(projects.entries())
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([project, list]) => [
            project,
            [...list].sort((a, b) => (b.create_time || 0) - (a.create_time || 0)),
          ]),
      ])
  })

  function statusColor(status: string): string {
    switch (status.toLowerCase()) {
      case 'running':
      case 'starting':
      case 'waitingapproval':
        return 'bg-green-500'
      case 'idle':
        return 'bg-blue-500'
      case 'stopped':
        return 'bg-gray-500'
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-gray-600'
    }
  }

  const isRunningStatus = (s: string) =>
    s === 'running' || s === 'waitingapproval' || s === 'starting'

  async function handleResume(session: any) {
    resumingId = session.id
    try {
      const key = session.session_key || session.id
      const parts = key.split('/')
      if (parts.length === 4) {
        // Restore the session process first
        const restoreRes = await fetch(`/api/sessions/${parts.join('/')}/restore`, { method: 'POST' })
        if (restoreRes.ok) {
          await fetchSessions()
        }
        navigate(`/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}`)
        return
      }
      navigate(`/sessions/${session.id}`)
    } finally {
      resumingId = null
    }
  }

  async function handleStop(session: any) {
    const key = session.session_key || session.id
    const parts = key.split('/')
    if (parts.length !== 4) return
    try {
      await fetch(`/api/sessions/${parts.join('/')}/stop`, { method: 'POST' })
      await fetchSessions()
    } catch (e) {
      console.error('Failed to stop session:', e)
    }
  }

  async function handleDelete(session: any) {
    if (!confirm($_('sessions.confirmDelete'))) return
    const key = session.session_key || session.id
    const parts = key.split('/')
    if (parts.length !== 4) return
    try {
      await fetch(`/api/sessions/${parts.join('/')}`, { method: 'DELETE' })
      await fetchSessions()
    } catch (e) {
      console.error('Failed to delete session:', e)
    }
  }
</script>

<div>
  <div class="flex items-center justify-between mb-6">
    <h2 class="text-2xl font-bold text-white">{$_('sessions.title')}</h2>
    <div class="flex items-center gap-2">
      <div class="flex gap-1 bg-gray-800 rounded-lg p-1">
        <button
          onclick={() => setViewMode('list')}
          class="px-3 py-1 rounded text-sm transition-colors {$viewMode === 'list' ? 'bg-blue-600 text-white' : 'text-gray-400 hover:text-white'}"
        >
          {$_('sessions.listMode')}
        </button>
        <button
          onclick={() => setViewMode('card')}
          class="px-3 py-1 rounded text-sm transition-colors {$viewMode === 'card' ? 'bg-blue-600 text-white' : 'text-gray-400 hover:text-white'}"
        >
          {$_('sessions.cardMode')}
        </button>
      </div>
    </div>
  </div>

  <div class="mb-4">
    <input
      type="text"
      bind:value={search}
      placeholder={$_('sessions.filterPlaceholder')}
      class="w-full bg-gray-900 border border-gray-800 rounded-lg px-4 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
    />
  </div>

  {#if listSessions.length === 0}
    <div class="flex items-center justify-center py-20 text-gray-500">
      {search ? $_('sessions.noMatch') : $_('sessions.noSessions')}
    </div>
  {:else if $viewMode === 'list'}
    <div class="space-y-0">
      {#each listSessions as session (session.id)}
        <SessionRow
          {session}
          {navigate}
          {resumingId}
          onResume={handleResume}
          onStop={handleStop}
          onDelete={handleDelete}
        />
      {/each}
    </div>
  {:else}
    <div class="space-y-6">
      {#each grouped as [discoveryPath, projects]}
        <div class="bg-gray-900 border border-gray-800 rounded-xl p-5">
          <h3 class="text-sm font-medium text-gray-400 mb-4">{discoveryPath}</h3>
          <div class="space-y-4">
            {#each projects as [project, projectSessions]}
              <div class="bg-gray-800/40 border border-gray-700/50 rounded-lg p-4">
                <h4 class="flex items-center gap-2 mb-2">
                  <span class="w-1 h-4 bg-blue-500 rounded-full"></span>
                  <span class="text-base font-semibold text-white">{project}</span>
                </h4>
                <div class="divide-y divide-gray-700/40">
                  {#each projectSessions as session (session.id)}
                    {@const seg = session.id.split('/')}
                    {@const env = seg[3] || session.id}
                    {@const agent = seg[2] || ''}
                    {@const running = isRunningStatus(session.status)}
                    <div
                      class="group flex items-center gap-3 px-2 py-2 -mx-2 rounded hover:bg-gray-700/30 transition-colors cursor-pointer"
                      role="button"
                      tabindex="0"
                      onclick={() => handleResume(session)}
                      onkeydown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault()
                          handleResume(session)
                        }
                      }}
                    >
                      <span class="w-2 h-2 rounded-full flex-shrink-0 {statusColor(session.status)} {running ? 'animate-pulse' : ''}"></span>
                      <span class="text-sm font-mono font-medium text-white truncate">{env}</span>
                      <span class="text-xs text-blue-400 bg-blue-900/20 px-1.5 py-0.5 rounded flex-shrink-0">{agent}</span>
                      <span class="flex-1"></span>
                      <div class="flex items-center gap-1">
                        {#if running}
                          <button
                            onclick={(e) => { e.stopPropagation(); handleStop(session) }}
                            class="p-1.5 rounded hover:bg-gray-600 transition-colors text-amber-500"
                            title={$_('sessions.stop')}
                          >
                            <StopCircle class="w-4 h-4" />
                          </button>
                        {:else}
                          <button
                            onclick={(e) => { e.stopPropagation(); handleResume(session) }}
                            disabled={resumingId === session.id}
                            class="p-1.5 rounded hover:bg-gray-600 transition-colors text-green-500 disabled:opacity-50"
                            title={$_('sessions.start')}
                          >
                            {#if resumingId === session.id}
                              <RefreshCw class="w-4 h-4 animate-spin" />
                            {:else}
                              <Play class="w-4 h-4" />
                            {/if}
                          </button>
                          <button
                            onclick={(e) => { e.stopPropagation(); handleDelete(session) }}
                            class="p-1.5 rounded hover:bg-gray-600 transition-colors text-red-500"
                            title={$_('sessions.delete')}
                          >
                            <Trash2 class="w-4 h-4" />
                          </button>
                        {/if}
                      </div>
                    </div>
                  {/each}
                </div>
              </div>
            {/each}
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>
