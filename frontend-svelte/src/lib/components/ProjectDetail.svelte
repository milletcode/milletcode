<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { projects, fetchProjects, setActiveProject } from '$lib/stores/project'
  import { sessions, setSessions, setActiveSession } from '$lib/stores/session'
  import { subscribeSession } from '$lib/wsPool'
  import SessionRow from './SessionRow.svelte'
  import AgentSelect from './AgentSelect.svelte'
  import { resumeSession } from '$lib/utils/sessionActions'
  import { apiFetch } from '$lib/api'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate }: Props = $props()

  // Extract org/project from params
  const org = $derived(params.org)
  const projectName = $derived(params.project)

  // Find project by matching keyword and name (works for both local and remote projects)
  const project = $derived(
    org && projectName
      ? $projects.find((p) => p.keyword === org && p.name === projectName)
      : undefined
  )

  // Use project.id ("keyword/name") for backend API calls
  const projectId = $derived(project?.id)

  let projectsLoading = $state(true)
  let enabledAgents = $state<Array<{ id: string; name: string; release_channel?: string }>>([])
  let agentId = $state('claude')
  let prompt = $state('')
  let creating = $state(false)
  let errorMsg = $state<string | null>(null)
  let sessionName = $state('default')
  let editingName = $state(false)
  let resumingId = $state<string | null>(null)
  let existingSession = $state<{
    session_key: string
    id: string
    status: string
    prompt: string
  } | null>(null)

  const projectSessions = $derived(projectId ? $sessions.filter((s) => s.project_id === projectId) : [])

  onMount(async () => {
    // Load projects if not already in store
    if ($projects.length === 0) {
      await fetchProjects()
    }
    projectsLoading = false

    // Load enabled agents from backend
    try {
      const res = await apiFetch('/api/agents')
      if (!res.ok) {
        const errorText = await res.text()
        console.error('[ProjectDetail] Agents API error:', res.status, errorText)
        return
      }
      const agents = await res.json()
      const enabled = agents
        .filter((a: any) => a.enabled)
        .map((a: any) => ({ id: a.id, name: a.name, release_channel: a.release_channel }))
      enabledAgents = enabled
      if (enabled.length > 0 && !enabled.find((a: { id: string }) => a.id === agentId)) {
        agentId = enabled[0].id
      }
    } catch (error) {
      console.error('[ProjectDetail] Failed to load agents:', error)
    }

    // Load sessions when projectId is available
    if (projectId && !projectsLoading) {
      setActiveProject(projectId)
      await loadSessions()
    }
  })

  async function loadSessions() {
    if (!projectId) return
    try {
      const res = await apiFetch(`/api/projects/${projectId}/sessions`)
      const data = await res.json()
      setSessions(data)
    } catch {}
  }

  async function handleStop(session: any) {
    const key = session.session_key || session.id
    const parts = key.split('/')
    if (parts.length !== 4) return
    try {
      await apiFetch(`/api/sessions/${parts.join('/')}/stop`, { method: 'POST' })
      await loadSessions()
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
      await apiFetch(`/api/sessions/${parts.join('/')}`, { method: 'DELETE' })
      await loadSessions()
    } catch (e) {
      console.error('Failed to delete session:', e)
    }
  }

  function slugify(text: string): string {
    return text
      .toLowerCase()
      .split(/\s+/)
      .join('-')
      .replace(/[^a-z0-9-_]/g, '')
  }

  function buildSessionKey(): string | null {
    if (!project || !sessionName) return null
    const agentConfig = enabledAgents.find((a) => a.id === agentId)
    const agentSlug = agentConfig?.id || agentId
    const nameSlug = slugify(sessionName) || 'default'
    const parts = project.path.split('/').filter(Boolean)
    const projName = parts[parts.length - 1] || 'unknown'
    // Use project.keyword (discovery path key) instead of extracting from path
    const keyword = project.keyword || 'default'
    return `${keyword}/${projName}/${agentSlug}/${nameSlug}`
  }

  async function handleStartClick() {
    if (!prompt.trim()) return

    const key = buildSessionKey()
    if (!key) return

    existingSession = null
    creating = true
    try {
      const res = await apiFetch(`/api/sessions/${key}`)
      if (res.ok) {
        const data = await res.json()
        if (data && !data.error) {
          existingSession = {
            session_key: key,
            id: data.id || data.session_id,
            status: data.status,
            prompt: data.prompt,
          }
          return
        }
      }
    } catch {
      // ignore
    } finally {
      creating = false
    }

    // No existing session — create directly
    await doCreateSession(false)
  }

  async function doCreateSession(overwrite: boolean) {
    if (!prompt.trim()) return
    creating = true
    errorMsg = null
    try {
      const body: Record<string, unknown> = {
        project_id: projectId,
        agent_id: agentId,
        prompt,
        session_name: sessionName || 'default',
        overwrite,
      }
      const res = await apiFetch('/api/sessions', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      if (!res.ok) {
        const errData = await res.json().catch(() => ({ error: 'Unknown error' }))
        errorMsg = errData.error || `Failed (${res.status})`
        return
      }
      const data = await res.json()
      if (data.session_key) {
        const parts = data.session_key.split('/')
        if (parts.length === 4) {
          const projectKey = `${parts[0]}/${parts[1]}/${parts[2]}`
          setActiveSession(data.session_id)
          subscribeSession(data.session_id, projectKey)
          window.location.href = `/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}`
          return
        }
      }
      navigate('/sessions')
    } catch (e: unknown) {
      errorMsg = e instanceof Error ? e.message : 'Network error'
    } finally {
      creating = false
    }
  }

  function handleResumeExisting() {
    if (!existingSession) return
    setActiveSession(existingSession.id)
    const parts = existingSession.session_key.split('/')
    if (parts.length === 4) {
      window.location.href = `/sessions/${parts[0]}/${parts[1]}/${parts[2]}/${parts[3]}`
    }
  }

  async function handleResumeSession(session: any) {
    resumingId = session.id
    try {
      await resumeSession({
        sessionId: session.id,
        sessionKey: session.session_key ?? null,
        navigate,
        setActiveSession,
      })
    } finally {
      resumingId = null
    }
  }
</script>

{#if projectsLoading}
  <div class="flex items-center justify-center py-20 text-gray-500">
    {$_('common.loading')}
  </div>
{:else if !project}
  <div class="flex items-center justify-center py-20 text-gray-500">
    {$_('common.noData')}
  </div>
{:else}
  <div>
    <!-- Project Header -->
    <div class="mb-6 relative">
      <button
        onclick={() => navigate('/')}
        class="text-sm text-gray-400 hover:text-white mb-3 transition-colors"
      >
        &larr; {$_('projects.title')}
      </button>
      <div class="flex items-start justify-between">
        <div>
          <h2 class="text-2xl font-bold text-white">{project.name}</h2>
          <p class="text-sm text-gray-400 mt-1">
            {project.path}
          </p>
          {#if project.frontend_port || project.backend_port}
            <div class="flex gap-2 mt-2">
              {#if project.frontend_port}
                <span class="px-2 py-0.5 bg-green-900/50 text-green-400 rounded text-xs">
                  Frontend: {project.frontend_port}
                </span>
              {/if}
              {#if project.backend_port}
                <span class="px-2 py-0.5 bg-blue-900/50 text-blue-400 rounded text-xs">
                  Backend: {project.backend_port}
                </span>
              {/if}
            </div>
          {/if}
          {#if project.description}
            <p class="text-sm text-gray-500 mt-2">{project.description}</p>
          {/if}
        </div>
        <button
          onclick={() => navigate(`/projects/${org}/${projectName}/settings`)}
          class="p-2 bg-gray-800 border border-gray-700 rounded-lg text-gray-400 hover:text-white hover:bg-gray-700 transition-colors"
          title="项目设置"
        >
          <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
            <path stroke-linecap="round" stroke-linejoin="round" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
            <path stroke-linecap="round" stroke-linejoin="round" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
          </svg>
        </button>
      </div>
    </div>

    <!-- New Session -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <h3 class="text-lg font-semibold text-white mb-4">{$_('sessions.title')}</h3>
      <div class="flex flex-col gap-3">
        <div class="flex gap-3">
          <AgentSelect agents={enabledAgents} bind:selected={agentId} />
          <div class="flex-1 relative">
            <!-- Session name badge -->
            {#if editingName}
              <input
                autofocus
                type="text"
                bind:value={sessionName}
                onblur={() => (editingName = false)}
                onkeydown={(e) => {
                  if (e.key === 'Enter') editingName = false
                }}
                class="absolute -top-2.5 -left-1 w-32 bg-gray-800 border border-blue-500 rounded px-2 py-1 text-xs text-white focus:outline-none z-10"
              />
            {:else}
              <button
                onclick={() => (editingName = true)}
                class="absolute -top-2.5 -left-1 z-10 px-2 py-0.5 bg-gray-700 hover:bg-gray-600 border border-gray-600 rounded text-xs text-gray-300 hover:text-white transition-colors"
                title="Click to edit session name"
              >
                {sessionName || 'default'}
                <svg class="w-3 h-3 inline ml-1 opacity-50" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536l-6 6H4v-1.999h10z" />
                </svg>
              </button>
            {/if}
            <input
              type="text"
              bind:value={prompt}
              onkeydown={(e) => {
                if (e.key === 'Enter') handleStartClick()
              }}
              placeholder={$_('sessions.typeMessage')}
              class="flex-1 w-full bg-gray-800 border border-gray-700 rounded-lg px-4 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
            />
          </div>
          <button
            onclick={handleStartClick}
            disabled={creating || !prompt.trim()}
            class="px-6 py-2 bg-blue-600 border border-blue-500 hover:bg-blue-700 rounded-lg text-sm font-medium text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {#if creating}
              {$_('common.loading')}
            {:else}
              {$_('sessions.start')}
            {/if}
          </button>
        </div>
        {#if errorMsg}
          <div class="text-sm text-red-400">{errorMsg}</div>
        {/if}
        {#if existingSession}
          <div class="flex items-center gap-3 p-3 bg-yellow-900/30 border border-yellow-700 rounded-lg">
            <span class="text-sm text-yellow-200">
              {$_('sessions.existingSession', { values: { status: existingSession.status } })}
            </span>
            <button
              onclick={handleResumeExisting}
              class="px-3 py-1 bg-green-600 hover:bg-green-700 rounded text-xs text-white"
            >
              {$_('sessions.resume')}
            </button>
            <button
              onclick={() => doCreateSession(true)}
              class="px-3 py-1 bg-red-600 hover:bg-red-700 rounded text-xs text-white"
            >
              {$_('sessions.overwrite')}
            </button>
          </div>
        {/if}
      </div>
    </div>

    <!-- Existing Sessions -->
    {#if projectSessions.length > 0}
      <div class="bg-gray-900 rounded-xl border border-gray-800">
        <div class="px-6 py-4 border-b border-gray-800">
          <h3 class="text-lg font-semibold text-white">{$_('sessions.existing')}</h3>
        </div>
        <div class="divide-y divide-gray-800">
          {#each projectSessions as session (session.id)}
            <SessionRow {session} {navigate} resumingId={resumingId} onResume={handleResumeSession} onStop={handleStop} onDelete={handleDelete} />
          {/each}
        </div>
      </div>
    {/if}
  </div>
{/if}
