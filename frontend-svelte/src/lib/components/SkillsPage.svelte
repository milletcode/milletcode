<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { apiFetch } from '$lib/api'

  interface OfficialNode {
    node: string
    status: string
    installed_at: string | null
    reason: string | null
  }

  interface OfficialSkill {
    name: string
    description: string
    icon: string
    installed: boolean
    installed_at: string | null
    nodes: OfficialNode[]
  }

  interface NodeReport {
    node: string
    ok: boolean
    reason: string | null
  }

  interface SkillTask {
    id: string
    kind: string
    skill: string
    status: 'running' | 'success' | 'partial' | 'failed'
    total_nodes: number
    done_nodes: number
    per_node: NodeReport[]
    summary: string
  }

  interface InstallSession {
    node: string
    agent_id: string
    session_key: string
    status: string
  }

  interface InstallRecord {
    id: string
    prompt: string
    created_at: number
    finished_at?: number | null
    agent_total: number
    node_total: number
    status: string
    sessions: InstallSession[]
    stats_mode: string
  }

  let { navigate: _navigate }: { navigate: (path: string) => void } = $props()

  let official = $state<OfficialSkill[]>([])
  let overwrite = $state(true)
  let loading = $state(true)
  // debug 模式（settings.env === 'debug'）才展示按节点安装细节；
  // 普通用户只看「已安装」徽章，不暴露 local 无 Agent 之类的内部节点语义
  let debugMode = $state(false)

  // per-skill running task ids
  let runningTasks = $state<Record<string, SkillTask>>({})
  let lastSummary = $state<Record<string, string>>({})

  // third-party install records (masonry + infinite scroll)
  const PAGE_SIZE = 20
  let records = $state<InstallRecord[]>([])
  let hasMoreRecords = $state(false)
  let statsMode = $state('total')
  let recordsOffset = 0
  let recordsLoading = $state(false)
  let recordsLoaded = $state(false)

  // third-party install dialogs: risk confirm → single prompt input
  let showRiskConfirm = $state(false)
  let showPromptDialog = $state(false)
  let thirdPrompt = $state('')
  let creating = $state(false)
  let createError = $state('')

  async function loadAll() {
    try {
      const settingsRes = await apiFetch('/api/settings')
      if (settingsRes.ok) {
        const settings = await settingsRes.json()
        debugMode = settings.env === 'debug'
      }
      const officialRes = await apiFetch('/api/skills/official')
      if (officialRes.ok) {
        const data = await officialRes.json()
        official = data.skills ?? []
        overwrite = data.overwrite ?? true
      }
    } finally {
      loading = false
    }
    await loadRecords(true)
  }

  async function loadRecords(reset: boolean) {
    if (recordsLoading) return
    if (reset) recordsOffset = 0
    recordsLoading = true
    try {
      const res = await apiFetch(`/api/skills/third?offset=${recordsOffset}&limit=${PAGE_SIZE}`)
      if (!res.ok) return
      const data = await res.json()
      const page: InstallRecord[] = data.records ?? []
      statsMode = data.stats_mode === 'detail' ? 'detail' : 'total'
      hasMoreRecords = !!data.has_more
      records = reset ? page : [...records, ...page]
      recordsOffset += page.length
    } finally {
      recordsLoading = false
      recordsLoaded = true
    }
  }

  // 静默刷新当前已加载的记录（运行中的记录每 5s 轮询一次）
  let pollTimer: ReturnType<typeof setTimeout> | null = null
  $effect(() => {
    if (pollTimer) {
      clearTimeout(pollTimer)
      pollTimer = null
    }
    if (!records.some((r) => r.status === 'running')) return
    pollTimer = setTimeout(async () => {
      try {
        const res = await apiFetch(`/api/skills/third?offset=0&limit=${Math.max(records.length, PAGE_SIZE)}`)
        if (!res.ok) return
        const data = await res.json()
        statsMode = data.stats_mode === 'detail' ? 'detail' : 'total'
        hasMoreRecords = !!data.has_more
        records = data.records ?? []
      } catch {
        // ignore transient poll errors
      }
    }, 5000)
  })

  function sentinelAction(node: HTMLElement) {
    const io = new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting && hasMoreRecords && !recordsLoading) {
        loadRecords(false)
      }
    })
    io.observe(node)
    return {
      destroy() {
        io.disconnect()
      },
    }
  }

  $effect(() => {
    loadAll()
  })

  async function toggleOverwrite() {
    const next = !overwrite
    overwrite = next
    await apiFetch('/api/skills/overwrite', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enabled: next }),
    })
  }

  function pollTask(skillName: string, taskId: string) {
    const timer = setInterval(async () => {
      const res = await apiFetch(`/api/skills/tasks/${taskId}`)
      if (!res.ok) return
      const task: SkillTask = await res.json()
      if (task.status === 'running') return
      clearInterval(timer)
      runningTasks = { ...runningTasks, [skillName]: task }
      const ok = task.per_node.filter((n) => n.ok && !n.reason).length
      const skipped = task.total_nodes - ok
      // 非 debug 下成功的结果不打摘要（含 local 跳过计数），卡片徽章「已安装」即全部展示；
      // 失败/部分失败仍要给出反馈
      if (debugMode || task.status !== 'success') {
        lastSummary = {
          ...lastSummary,
          [skillName]: $_('skills.taskDone', { values: { ok, skip: skipped } }),
        }
      }
      setTimeout(() => {
        const { [skillName]: _t, ...rest } = runningTasks
        runningTasks = rest
        const { [skillName]: _s, ...restSummary } = lastSummary
        lastSummary = restSummary
        loadAll()
      }, 6000)
    }, 2000)
  }

  async function installOfficial(skill: OfficialSkill) {
    const res = await apiFetch(`/api/skills/official/${encodeURIComponent(skill.name)}/install`, { method: 'POST' })
    if (!res.ok) {
      const data = await res.json().catch(() => ({}))
      lastSummary = { ...lastSummary, [skill.name]: data.error ?? 'failed' }
      return
    }
    const { task_id } = await res.json()
    runningTasks = { ...runningTasks, [skill.name]: { id: task_id, kind: 'install', skill: skill.name, status: 'running', total_nodes: 0, done_nodes: 0, per_node: [], summary: '' } }
    pollTask(skill.name, task_id)
  }

  async function uninstallOfficial(skill: OfficialSkill) {
    const res = await apiFetch(`/api/skills/official/${encodeURIComponent(skill.name)}/uninstall`, { method: 'POST' })
    if (!res.ok) return
    const { task_id } = await res.json()
    runningTasks = { ...runningTasks, [skill.name]: { id: task_id, kind: 'uninstall', skill: skill.name, status: 'running', total_nodes: 0, done_nodes: 0, per_node: [], summary: '' } }
    pollTask(skill.name, task_id)
  }

  function startThirdInstall() {
    createError = ''
    showRiskConfirm = true
  }

  function acceptRisk() {
    showRiskConfirm = false
    thirdPrompt = ''
    showPromptDialog = true
  }

  async function submitInstall() {
    if (!thirdPrompt.trim()) return
    creating = true
    createError = ''
    try {
      const res = await apiFetch('/api/skills/third', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ prompt: thirdPrompt.trim() }),
      })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) {
        createError = data.error ?? 'failed'
        return
      }
      showPromptDialog = false
      await loadRecords(true)
    } catch (e: any) {
      createError = e?.message ?? 'failed'
    } finally {
      creating = false
    }
  }

  function fmtTime(ms: number): string {
    try {
      return new Date(ms).toLocaleString()
    } catch {
      return ''
    }
  }

  function statusClass(status: string): string {
    switch (status) {
      case 'success':
        return 'bg-green-500/20 text-green-400'
      case 'partial':
        return 'bg-amber-500/20 text-amber-400'
      case 'failed':
      case 'error':
        return 'bg-red-500/20 text-red-400'
      default:
        return 'bg-blue-500/20 text-blue-400'
    }
  }

  const nodeStatusKey: Record<string, string> = {
    installed: 'skills.nodeInstalled',
    skipped: 'skills.nodeSkipped',
    protected: 'skills.nodeProtected',
    failed: 'skills.nodeFailed',
  }

  function nodeStatusClass(status: string): string {
    switch (status) {
      case 'installed':
        return 'bg-green-500/20 text-green-400'
      case 'skipped':
        return 'bg-gray-700/60 text-gray-400'
      case 'protected':
        return 'bg-amber-500/20 text-amber-400'
      default:
        return 'bg-red-500/20 text-red-400'
    }
  }
</script>

<div class="max-w-5xl mx-auto">
  <div class="flex items-center justify-between mb-6">
    <h1 class="text-2xl font-bold text-white">{$_('skills.title')}</h1>
    <label class="flex items-center gap-3 cursor-pointer select-none">
      <div>
        <div class="text-sm text-white">{$_('skills.overwrite')}</div>
        <div class="text-xs text-gray-500">{$_('skills.overwriteDesc')}</div>
      </div>
      <button
        role="switch"
        aria-checked={overwrite}
        onclick={toggleOverwrite}
        class="relative inline-flex h-6 w-11 items-center rounded-full transition-colors {overwrite ? 'bg-blue-600' : 'bg-gray-700'}"
      >
        <span class="inline-block h-4 w-4 transform rounded-full bg-white transition-transform {overwrite ? 'translate-x-6' : 'translate-x-1'}"></span>
      </button>
    </label>
  </div>

  {#if loading}
    <div class="text-gray-500 text-center mt-20">{$_('common.loading')}</div>
  {:else}
    <!-- 官方技能 -->
    <h2 class="text-lg font-semibold text-white mb-3">{$_('skills.official')}</h2>
    {#if official.length === 0}
      <div class="text-gray-500 text-sm mb-8">{$_('skills.officialEmpty')}</div>
    {:else}
      <div class="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4 mb-8">
        {#each official as skill (skill.name)}
          <div class="bg-gray-900 border border-gray-800 rounded-xl p-5 flex flex-col">
            <div class="flex items-start gap-3 mb-3">
              <div class="w-10 h-10 rounded-lg bg-gray-800 flex items-center justify-center text-blue-400 flex-shrink-0">
                {@html skill.icon}
              </div>
              <div class="min-w-0">
                <div class="flex items-center gap-2">
                  <h3 class="text-white font-medium truncate">{skill.name}</h3>
                  {#if skill.installed}
                    <span class="text-[10px] px-1.5 py-0.5 rounded bg-green-500/20 text-green-400">{$_('skills.installed')}</span>
                  {/if}
                </div>
                <p class="text-xs text-gray-500 mt-1 line-clamp-2">{skill.description}</p>
              </div>
            </div>
            {#if debugMode && skill.nodes.length > 0}
              <div class="flex flex-wrap gap-1 mb-3">
                {#each skill.nodes as n (n.node)}
                  <span
                    class="text-[10px] px-1.5 py-0.5 rounded {nodeStatusClass(n.status)}"
                    title={n.reason ?? ''}
                  >{n.node}: {$_(nodeStatusKey[n.status] ?? n.status)}</span>
                {/each}
              </div>
            {/if}
            <div class="mt-auto">
              {#if runningTasks[skill.name]}
                <div class="flex items-center gap-2 text-sm text-gray-400">
                  <svg class="w-4 h-4 animate-spin" viewBox="0 0 24 24" fill="none">
                    <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                    <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
                  </svg>
                  {runningTasks[skill.name].kind === 'install' ? $_('skills.installing') : $_('skills.uninstalling')}
                </div>
              {:else if lastSummary[skill.name]}
                <div class="text-xs text-gray-400">{lastSummary[skill.name]}</div>
              {:else}
                <div class="flex gap-2 justify-end">
                  {#if skill.installed}
                    <button
                      onclick={() => uninstallOfficial(skill)}
                      class="px-4 py-1.5 bg-gray-800 hover:bg-gray-700 border border-gray-700 rounded-lg text-sm text-gray-300 transition-colors"
                    >
                      {$_('skills.uninstall')}
                    </button>
                  {:else}
                    <button
                      onclick={() => installOfficial(skill)}
                      class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 border border-blue-500 rounded-lg text-sm text-white transition-colors"
                    >
                      {$_('skills.install')}
                    </button>
                  {/if}
                </div>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    {/if}

    <!-- 第三方技能 -->
    <div class="flex items-center justify-between mb-3">
      <h2 class="text-lg font-semibold text-white flex items-center gap-2">
        {$_('skills.thirdParty')}
        <span class="px-2 py-0.5 rounded text-xs font-medium bg-amber-500/15 text-amber-300 border border-amber-500/30">beta</span>
      </h2>
      <button
        onclick={startThirdInstall}
        class="px-4 py-2 bg-blue-600 hover:bg-blue-700 rounded-lg text-sm font-medium text-white transition-colors"
      >
        {$_('skills.installThird')}
      </button>
    </div>
    {#if !recordsLoaded}
      <div class="text-gray-500 text-sm">{$_('common.loading')}</div>
    {:else if records.length === 0}
      <div class="text-gray-500 text-sm">{$_('skills.noRecords')}</div>
    {:else}
      <div class="space-y-3">
        {#each records as r (r.id)}
          <div class="bg-gray-900 border border-gray-800 rounded-xl p-4 flex items-center gap-6 flex-wrap">
            <div class="flex items-center gap-5 flex-shrink-0">
              {#if statsMode === 'detail'}
                <div class="text-center min-w-[3rem]">
                  <div class="text-2xl font-bold text-white leading-none">{r.node_total}</div>
                  <div class="text-[10px] text-gray-500 mt-1">{$_('skills.fieldNodeCount')}</div>
                </div>
                <div class="w-px h-9 bg-gray-800"></div>
              {/if}
              <div class="text-center min-w-[3rem]">
                <div class="text-2xl font-bold text-blue-400 leading-none">{r.agent_total}</div>
                <div class="text-[10px] text-gray-500 mt-1">{$_('skills.fieldAgentTotal')}</div>
              </div>
            </div>
            <div class="w-px h-9 bg-gray-800 flex-shrink-0"></div>
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-2 mb-1 flex-wrap">
                <span class="text-[10px] px-1.5 py-0.5 rounded {statusClass(r.status)}">
                  {$_(`skills.status${r.status.charAt(0).toUpperCase()}${r.status.slice(1)}`)}
                </span>
                {#if r.status === 'running'}
                  <svg class="w-3 h-3 animate-spin text-blue-400" viewBox="0 0 24 24" fill="none">
                    <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                    <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
                  </svg>
                {/if}
                <span class="text-xs text-gray-500">{fmtTime(r.created_at)}</span>
              </div>
              <p class="text-xs text-gray-400 whitespace-pre-wrap break-words line-clamp-3">{r.prompt}</p>
            </div>
          </div>
        {/each}
      </div>
      {#if hasMoreRecords}
        <div use:sentinelAction class="h-10 flex items-center justify-center text-xs text-gray-500">
          {recordsLoading ? $_('common.loading') : ''}
        </div>
      {/if}
    {/if}
  {/if}
</div>

<!-- 风险确认 dialog -->
{#if showRiskConfirm}
  <div class="fixed inset-0 bg-black/50 flex items-center justify-center z-[60]" role="presentation">
    <div class="bg-gray-900 border border-red-900/60 rounded-xl p-6 w-full max-w-lg mx-4">
      <h3 class="text-lg font-semibold text-red-400 mb-4">{$_('skills.riskTitle')}</h3>
      <p class="text-sm text-gray-300 leading-relaxed mb-6">{$_('skills.riskMessage')}</p>
      <div class="flex gap-3 justify-end">
        <button
          onclick={() => (showRiskConfirm = false)}
          class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-300"
        >
          {$_('common.cancel')}
        </button>
        <button
          onclick={acceptRisk}
          class="px-4 py-2 bg-red-600 hover:bg-red-700 rounded-lg text-sm text-white"
        >
          {$_('skills.riskAck')}
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- 安装提示词 dialog -->
{#if showPromptDialog}
  <div class="fixed inset-0 bg-black/50 flex items-center justify-center z-[60]" role="presentation">
    <div class="bg-gray-900 border border-gray-800 rounded-xl p-6 w-full max-w-lg mx-4">
      <h3 class="text-lg font-semibold text-white mb-4 flex items-center gap-2">
        {$_('skills.promptTitle')}
        <span class="px-2 py-0.5 rounded text-xs font-medium bg-amber-500/15 text-amber-300 border border-amber-500/30">beta</span>
      </h3>
      <label class="block text-sm text-gray-400 mb-1" for="third-prompt">{$_('skills.promptLabel')}</label>
      <textarea
        id="third-prompt"
        rows="6"
        value={thirdPrompt}
        oninput={(e) => (thirdPrompt = e.currentTarget.value)}
        placeholder={$_('skills.promptPlaceholder')}
        class="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500 resize-y"
      ></textarea>
      {#if createError}
        <div class="text-sm text-red-400 mt-2">{createError}</div>
      {/if}
      <div class="flex gap-3 justify-end mt-6">
        <button
          onclick={() => (showPromptDialog = false)}
          class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-300"
        >
          {$_('common.cancel')}
        </button>
        <button
          onclick={submitInstall}
          disabled={!thirdPrompt.trim() || creating}
          class="px-4 py-2 bg-blue-600 hover:bg-blue-700 rounded-lg text-sm text-white disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {creating ? $_('common.loading') : $_('skills.startInstall')}
        </button>
      </div>
    </div>
  </div>
{/if}
