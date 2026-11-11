<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { projects, fetchProjects, refreshProjects, setActiveProject } from '$lib/stores/project'
  import { apiFetch } from '$lib/api'
  import NewProjectModal from './NewProjectModal.svelte'

  interface Props {
    navigate: (path: string) => void
  }

  let { navigate }: Props = $props()

  let activeTag = $state<string | null>(null)
  let showCreateModal = $state(false)
  let discoveryPaths = $state<{ name: string; path: string; remote_mode: string; is_mobile_home: boolean }[]>([])
  let selectedIds = $state<Set<string>>(new Set())
  let showArchived = $state(false)
  let showWebOnly = $state(false)
  let showArchiveConfirm = $state(false)
  let showUnarchiveConfirm = $state(false)
  let showDeleteConfirm = $state(false)

  onMount(async () => {
    await fetchProjects()
    try {
      const res = await apiFetch('/api/projects/discovery-paths')
      const data = await res.json()
      discoveryPaths = data
    } catch (err) {
      console.error('Failed to fetch discovery paths:', err)
    }
  })

  const basePath = $derived.by(() => {
    if (!activeTag) return ''
    const dp = discoveryPaths.find((p) => p.name === activeTag)
    return dp?.path || ''
  })

  const tags = $derived.by(() => {
    const seen = new Set<string>()
    // Always include all configured discovery paths, even if they have no projects yet.
    for (const dp of discoveryPaths) {
      if (dp.name) seen.add(dp.name)
    }
    for (const p of $projects) {
      if (p.keyword) seen.add(p.keyword)
    }
    return Array.from(seen).sort()
  })

  const filtered = $derived(
    activeTag ? $projects.filter((p) => p.keyword === activeTag) : $projects
  )

  const withArchived = $derived(showArchived ? filtered : filtered.filter((p) => !p.archived))

  const visibleProjects = $derived(
    showWebOnly ? withArchived.filter((p) => p.frontend_port || p.backend_port) : withArchived
  )

  function toggleSelect(id: string) {
    const next = new Set(selectedIds)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    selectedIds = next
  }

  function clearSelection() {
    selectedIds = new Set()
  }

  const hasArchivedSelected = $derived(
    Array.from(selectedIds).some((id) => {
      const p = $projects.find((p) => p.id === id)
      return p?.archived
    })
  )

  const hasNonArchivedSelected = $derived(
    Array.from(selectedIds).some((id) => {
      const p = $projects.find((p) => p.id === id)
      return p && !p.archived
    })
  )

  async function handleArchiveSelected() {
    for (const id of selectedIds) {
      await fetch(`/api/projects/${id}/archive`, { method: 'POST' })
    }
    clearSelection()
    showArchiveConfirm = false
    refreshProjects()
  }

  async function handleUnarchiveSelected() {
    for (const id of selectedIds) {
      const p = $projects.find((p) => p.id === id)
      if (p?.archived) {
        await fetch(`/api/projects/${id}/unarchive`, { method: 'POST' })
      }
    }
    clearSelection()
    showUnarchiveConfirm = false
    refreshProjects()
  }

  async function handleDeleteSelected() {
    for (const id of selectedIds) {
      await fetch(`/api/projects/${id}`, { method: 'DELETE' })
    }
    clearSelection()
    showDeleteConfirm = false
    refreshProjects()
  }

  function handleProjectClick(projectId: string) {
    setActiveProject(projectId)
    // Find the project to get keyword and name
    const project = $projects.find(p => p.id === projectId)
    if (project) {
      // Use keyword and name directly (works for both local and remote projects)
      navigate(`/projects/${project.keyword}/${project.name}`)
    } else {
      // Fallback: extract from id (format: keyword/name)
      const parts = projectId.split('/')
      const keyword = parts[0] || 'unknown'
      const name = parts[1] || 'unknown'
      navigate(`/projects/${keyword}/${name}`)
    }
  }

  function handleCreated() {
    showCreateModal = false
    refreshProjects()
  }
</script>

<div>
  <div class="flex items-center justify-between mb-4">
    <h2 class="text-2xl font-bold text-white">{$_('projects.title')}</h2>
    <div class="flex items-center gap-2">
      {#if activeTag}
        {@const activeDp = discoveryPaths.find((d) => d.name === activeTag)}
        {@const activeIsSSH = activeDp?.remote_mode === 'ssh'}
        <button
          onclick={() => (showCreateModal = true)}
          class="px-6 py-3 rounded-xl text-sm font-bold text-white transition-all shadow-xl hover:scale-105 hover:shadow-2xl active:scale-95 {activeIsSSH
            ? 'bg-gradient-to-r from-blue-500 to-blue-600 shadow-blue-500/40 hover:shadow-blue-500/60'
            : 'bg-gradient-to-r from-cyan-500 to-cyan-600 shadow-cyan-500/40 hover:shadow-cyan-500/60'}"
        >
          + {$_('projects.createProject')}
        </button>
      {/if}
      <button
        onclick={refreshProjects}
        class="px-4 py-2 bg-gray-800 border border-gray-700 hover:bg-gray-700 rounded-lg text-sm font-medium text-gray-300 transition-colors"
      >
        {$_('projects.scanProjects')}
      </button>
      <!-- Web filter button -->
      <button
        onclick={() => (showWebOnly = !showWebOnly)}
        class="px-3 py-2.5 border rounded-lg text-sm font-medium transition-colors flex items-center justify-center {showWebOnly
          ? 'bg-green-600 border-green-500 text-white hover:bg-green-700'
          : 'bg-gray-800 border-gray-700 text-gray-100 hover:bg-gray-700'}"
        title={showWebOnly ? $_('projects.showAll') : $_('projects.showWebOnly')}
      >
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="w-5 h-5">
          <circle cx="12" cy="12" r="10" />
          <line x1="2" y1="12" x2="22" y2="12" />
          <path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" />
        </svg>
      </button>
      <!-- Eye button to toggle archived projects -->
      <button
        onclick={() => (showArchived = !showArchived)}
        class="px-3 py-2.5 border rounded-lg text-sm font-medium transition-colors flex items-center justify-center {showArchived
          ? 'bg-yellow-600 border-yellow-500 text-white hover:bg-yellow-700'
          : 'bg-gray-800 border-gray-700 text-gray-100 hover:bg-gray-700'}"
        title={showArchived ? $_('projects.hideArchived') : $_('projects.showArchived')}
      >
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="currentColor" class="w-5 h-5">
          {#if showArchived}
            <path fill-rule="evenodd" d="M3.28 2.22a.75.75 0 00-1.06 1.06l14.5 14.5a.75.75 0 101.06-1.06l-1.745-1.745a10.029 10.029 0 003.3-4.385 1.651 1.651 0 000-1.185A10.004 10.004 0 009.999 3a9.956 9.956 0 00-4.744 1.194L3.28 2.22zM7.752 6.69l1.092 1.092a2.5 2.5 0 013.374 3.373l1.092 1.092a4 4 0 00-5.558-5.558z" clip-rule="evenodd" />
          {:else}
            <path fill-rule="evenodd" d="M1.5 10a8.5 8.5 0 011.931-4.882C4.756 3.58 7.093 2.5 10 2.5s5.244 1.08 6.569 2.618A8.5 8.5 0 0118.5 10a8.5 8.5 0 01-1.931 4.882C15.244 16.42 12.907 17.5 10 17.5s-5.244-1.08-6.569-2.618A8.5 8.5 0 011.5 10zM10 7a3 3 0 100 6 3 3 0 000-6z" clip-rule="evenodd" />
          {/if}
        </svg>
      </button>
    </div>
  </div>

  <!-- Tag Filters -->
  {#if tags.length > 0}
    <div class="flex flex-wrap gap-2 mb-6">
      <button
        onclick={() => (activeTag = null)}
        class="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors {activeTag === null
          ? 'bg-blue-600 text-white'
          : 'bg-gray-800 text-gray-400 hover:text-white border border-gray-700'}"
      >
        {$_('common.all')} ({$projects.length})
      </button>
      {#each tags as tag}
        {@const count = $projects.filter((p) => p.keyword === tag).length}
        {@const dp = discoveryPaths.find((d) => d.name === tag)}
        {@const isMobile = dp?.is_mobile_home ?? false}
        {@const isSSH = dp?.remote_mode === 'ssh'}
        {@const activeColor = activeTag === tag ? (isSSH ? 'bg-blue-600 text-white' : 'bg-cyan-600 text-white') : ''}
        <button
          onclick={() => (activeTag = tag)}
          class="relative px-3 py-1.5 rounded-lg text-xs font-medium transition-colors {activeTag === tag
            ? activeColor
            : 'bg-gray-800 text-gray-400 hover:text-white border border-gray-700'}"
        >
          {tag} ({count})
          {#if isMobile}
            <span class="absolute -top-1 -right-1 flex items-center justify-center w-4 h-4 bg-purple-600 rounded-full" title="Mobile">
              <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" fill="currentColor" class="w-2.5 h-2.5 text-white" aria-label="Mobile category">
                <path d="M8 16.25a.75.75 0 0 1 .75-.75h2.5a.75.75 0 0 1 0 1.5h-2.5a.75.75 0 0 1-.75-.75ZM5 3.75A2.75 2.75 0 0 1 7.75 1h4.5A2.75 2.75 0 0 1 15 3.75v12.5a2.75 2.75 0 0 1-2.75 2.75h-4.5A2.75 2.75 0 0 1 5 16.25V3.75ZM7.75 2.5a1.25 1.25 0 0 0-1.25 1.25v12.5c0 .69.56 1.25 1.25 1.25h4.5c.69 0 1.25-.56 1.25-1.25V3.75A1.25 1.25 0 0 0 12.25 2.5h-4.5Z"/>
              </svg>
            </span>
          {/if}
        </button>
      {/each}
    </div>
  {/if}

  {#if visibleProjects.length === 0}
    <div class="flex flex-col items-center justify-center py-20 text-gray-500">
      <p class="text-lg mb-2">{$_('projects.noProjects')}</p>
      <p class="text-sm">
        {$_('projects.configureHint')}{' '}
        <a href="/settings" class="text-blue-400 hover:text-blue-300">
          {$_('projects.configureSettings')}
        </a>
        {$_('projects.configureHintSuffix')}
      </p>
    </div>
  {:else}
    <!-- Selection actions bar -->
    {#if selectedIds.size > 0}
      <div class="flex items-center gap-3 mb-4 px-4 py-3 bg-gray-800 border border-gray-700 rounded-lg">
        <span class="text-sm text-gray-300">
          {$_('projects.selectedCount', { values: { count: selectedIds.size } })}
        </span>
        <div class="flex-1"></div>
        {#if hasNonArchivedSelected}
          <button
            onclick={() => (showArchiveConfirm = true)}
            class="px-3 py-1.5 bg-yellow-600 border border-yellow-500 hover:bg-yellow-700 rounded-lg text-sm font-medium text-white transition-colors"
          >
            {$_('projects.archiveSelected')}
          </button>
        {/if}
        {#if hasArchivedSelected}
          <button
            onclick={() => (showUnarchiveConfirm = true)}
            class="px-3 py-1.5 bg-green-600 border border-green-500 hover:bg-green-700 rounded-lg text-sm font-medium text-white transition-colors"
          >
            {$_('projects.unarchiveSelected')}
          </button>
        {/if}
        <button
          onclick={() => (showDeleteConfirm = true)}
          class="px-3 py-1.5 bg-red-600 border border-red-500 hover:bg-red-700 rounded-lg text-sm font-medium text-white transition-colors"
        >
          {$_('projects.deleteSelected')}
        </button>
      </div>
    {/if}

    <!-- Project Grid -->
    <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
      {#each visibleProjects as project (project.id)}
        <div
          class="bg-gray-900 border border-gray-800 rounded-lg p-4 hover:border-gray-700 transition-colors cursor-pointer relative {selectedIds.has(project.id) ? 'ring-2 ring-blue-500' : ''}"
          onclick={() => handleProjectClick(project.id)}
          role="button"
          tabindex="0"
          onkeydown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') handleProjectClick(project.id)
          }}
        >
          <!-- Selection checkbox -->
          <input
            type="checkbox"
            checked={selectedIds.has(project.id)}
            onchange={() => toggleSelect(project.id)}
            onclick={(e) => e.stopPropagation()}
            class="absolute top-3 right-3 w-4 h-4 rounded border-gray-600 bg-gray-800 text-blue-600 focus:ring-blue-500"
          />

          <div class="flex items-start gap-3">
            <div class="flex-1 min-w-0">
              <div class="flex items-center gap-2 mb-1">
                <h3 class="text-base font-semibold text-white truncate">{project.name}</h3>
                {#if project.archived}
                  <span class="px-1.5 py-0.5 bg-gray-700 text-gray-400 rounded text-[10px]">
                    {$_('projects.archived')}
                  </span>
                {/if}
              </div>
              {#if project.description}
                <p class="text-xs text-gray-400 mt-2 line-clamp-2">{project.description}</p>
              {/if}
              {#if project.frontend_port || project.backend_port}
                <div class="flex gap-2 mt-2">
                  {#if project.frontend_port}
                    <span class="px-1.5 py-0.5 bg-green-900/50 text-green-400 rounded text-[10px]">
                      FE:{project.frontend_port}
                    </span>
                  {/if}
                  {#if project.backend_port}
                    <span class="px-1.5 py-0.5 bg-blue-900/50 text-blue-400 rounded text-[10px]">
                      BE:{project.backend_port}
                    </span>
                  {/if}
                </div>
              {/if}
            </div>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<!-- Confirm Dialogs -->
{#if showArchiveConfirm}
  <div class="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
    <div class="bg-gray-900 border border-gray-800 rounded-lg p-6 max-w-md">
      <h3 class="text-lg font-semibold text-white mb-4">{$_('projects.confirmArchiveTitle')}</h3>
      <p class="text-sm text-gray-400 mb-6">
        {$_('projects.confirmArchiveMessage', { values: { count: selectedIds.size } })}
      </p>
      <div class="flex gap-3 justify-end">
        <button
          onclick={() => (showArchiveConfirm = false)}
          class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-300"
        >
          {$_('common.cancel')}
        </button>
        <button
          onclick={handleArchiveSelected}
          class="px-4 py-2 bg-yellow-600 hover:bg-yellow-700 rounded-lg text-sm text-white"
        >
          {$_('projects.archive')}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showUnarchiveConfirm}
  <div class="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
    <div class="bg-gray-900 border border-gray-800 rounded-lg p-6 max-w-md">
      <h3 class="text-lg font-semibold text-white mb-4">{$_('projects.confirmUnarchive')}</h3>
      <p class="text-sm text-gray-400 mb-6">
        {$_('projects.unarchiveConfirmMessage', { values: { count: selectedIds.size } })}
      </p>
      <div class="flex gap-3 justify-end">
        <button
          onclick={() => (showUnarchiveConfirm = false)}
          class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-300"
        >
          {$_('common.cancel')}
        </button>
        <button
          onclick={handleUnarchiveSelected}
          class="px-4 py-2 bg-green-600 hover:bg-green-700 rounded-lg text-sm text-white"
        >
          {$_('projects.unarchive')}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showDeleteConfirm}
  <div class="fixed inset-0 bg-black/50 flex items-center justify-center z-50">
    <div class="bg-gray-900 border border-gray-800 rounded-lg p-6 max-w-md">
      <h3 class="text-lg font-semibold text-white mb-4">{$_('projects.confirmDeleteTitle')}</h3>
      <p class="text-sm text-red-400 mb-6">
        {$_('projects.confirmDeleteMessage', { values: { count: selectedIds.size } })}
      </p>
      <div class="flex gap-3 justify-end">
        <button
          onclick={() => (showDeleteConfirm = false)}
          class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-300"
        >
          {$_('common.cancel')}
        </button>
        <button
          onclick={handleDeleteSelected}
          class="px-4 py-2 bg-red-600 hover:bg-red-700 rounded-lg text-sm text-white"
        >
          {$_('projects.delete')}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showCreateModal && activeTag}
  {@const currentDp = discoveryPaths.find((d) => d.name === activeTag)}
  <NewProjectModal
    discoveryRoot={activeTag}
    basePath={basePath}
    isMobileHome={currentDp?.is_mobile_home ?? false}
    onClose={() => (showCreateModal = false)}
    onCreated={handleCreated}
  />
{/if}
