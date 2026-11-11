<script lang="ts">
  import { onMount } from 'svelte'
  import { projects, fetchProjects } from '$lib/stores/project'
  import { apiFetch } from '$lib/api'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate }: Props = $props()

  const org = $derived(params.org)
  const projectName = $derived(params.project)

  // Find project by matching keyword and name (works for both local and remote projects)
  const project = $derived(
    org && projectName
      ? $projects.find((p) => p.keyword === org && p.name === projectName)
      : undefined
  )

  // Direct URL entry hits this page before the projects store is populated —
  // fetch it, then let the effect below run once the derived project appears.
  let initialized = false
  $effect(() => {
    if (!project?.id || initialized) return
    initialized = true
    void init()
  })

  onMount(async () => {
    if (!$projects.length) await fetchProjects()
    if (!project?.id) loading = false
  })

  async function init() {
    try {
      const cmdRes = await apiFetch(`/api/projects/${project!.id}/terminal/commands`)
      if (cmdRes.ok) terminalCommands = await cmdRes.json()
      await loadEnvSettings()
      await loadIssueConfig()
    } catch (e) {
      console.error('Failed to load settings:', e)
    } finally {
      loading = false
    }
  }

  interface EnvVar {
    key: string
    value: string
  }

  interface TerminalCommand {
    id: string
    name: string
    command: string
  }

  // --- env vars (envs.redb): environments → key-value pairs ---
  let envs = $state<string[]>([])
  let selectedEnv = $state('')
  let envVars = $state<EnvVar[]>([])
  let loadedVars: EnvVar[] = []
  let addingEnv = $state(false)
  let newEnvName = $state('')
  let envSaved = $state(false)

  // --- 需求接收 (issue intake): per-project gitea credentials for /git issue ---
  let issueUrl = $state('')
  let issueToken = $state('')
  let issueHasToken = $state(false)
  let issueMaskedToken = $state('')
  let issueConfigured = $state(false)
  let issueSaving = $state(false)
  let issueSaved = $state(false)

  async function loadIssueConfig() {
    if (!project?.id) return
    const res = await apiFetch(`/api/projects/${project.id}/issue-config`)
    if (res.ok) {
      const data = await res.json()
      issueUrl = data.url || ''
      issueToken = ''
      issueHasToken = !!data.has_token
      issueMaskedToken = data.token || ''
      issueConfigured = !!(data.url || data.has_token)
      issueSaved = false
    }
  }

  async function saveIssueConfig() {
    if (!project?.id) return
    issueSaving = true
    try {
      const res = await apiFetch(`/api/projects/${project.id}/issue-config`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: issueUrl.trim(), token: issueToken.trim() }),
      })
      if (res.ok) {
        await loadIssueConfig()
        issueSaved = true
      }
    } finally {
      issueSaving = false
    }
  }

  async function clearIssueConfig() {
    if (!project?.id) return
    issueSaving = true
    try {
      await apiFetch(`/api/projects/${project.id}/issue-config`, { method: 'DELETE' })
      await loadIssueConfig()
    } finally {
      issueSaving = false
    }
  }

  let terminalCommands = $state<TerminalCommand[]>([])
  let loading = $state(true)
  let saving = $state(false)
  let cmdSaved = $state(false)

  async function loadEnvSettings() {
    if (!project?.id) return
    const envsRes = await apiFetch(`/api/projects/${project.id}/envs`)
    if (envsRes.ok) {
      const data = await envsRes.json()
      envs = data.envs || []
      selectedEnv = envs.includes('dev') ? 'dev' : (envs[0] || 'dev')
      await loadEnvVars()
    }
  }

  async function loadEnvVars() {
    if (!project?.id || !selectedEnv) return
    const res = await apiFetch(`/api/projects/${project.id}/envs/${encodeURIComponent(selectedEnv)}`)
    if (res.ok) {
      const data = await res.json()
      envVars = (data.vars || []).map((v: EnvVar) => ({ ...v }))
      loadedVars = envVars.map((v) => ({ ...v }))
      envSaved = false
    }
  }

  async function selectEnv(env: string) {
    if (selectedEnv === env) return
    selectedEnv = env
    envVars = []
    loadedVars = []
    await loadEnvVars()
  }

  function addEnv() {
    const name = newEnvName.trim()
    if (!name || name.includes('/')) return
    if (!envs.includes(name)) envs = [...envs, name]
    newEnvName = ''
    addingEnv = false
    selectEnv(name)
  }

  function addEnvVar() {
    envVars = [...envVars, { key: '', value: '' }]
  }

  function removeEnvVar(i: number) {
    envVars = envVars.filter((_, idx) => idx !== i)
    envSaved = false
  }

  function updateEnvVar(i: number, field: keyof EnvVar, value: string) {
    envVars = envVars.map((v, idx) => (idx === i ? { ...v, [field]: value } : v))
    envSaved = false
  }

  /// Diff against the loaded snapshot: upsert new/changed, delete removed.
  async function saveEnvVars() {
    if (!project?.id || !selectedEnv) return
    saving = true
    try {
      const current = envVars.filter((v) => v.key.trim() && !v.key.includes('/'))
      const currentKeys = new Set(current.map((v) => v.key))

      for (const v of current) {
        const before = loadedVars.find((l) => l.key === v.key)
        if (!before || before.value !== v.value) {
          await apiFetch(`/api/projects/${project.id}/envs/${encodeURIComponent(selectedEnv)}`, {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ key: v.key.trim(), value: v.value }),
          })
        }
      }
      for (const v of loadedVars) {
        if (!currentKeys.has(v.key)) {
          await apiFetch(
            `/api/projects/${project.id}/envs/${encodeURIComponent(selectedEnv)}/${encodeURIComponent(v.key)}`,
            { method: 'DELETE' }
          )
        }
      }
      if (!envs.includes(selectedEnv)) envs = [...envs, selectedEnv]
      await loadEnvVars()
      envSaved = true
    } finally {
      saving = false
    }
  }

  function addCommand() {
    terminalCommands = [...terminalCommands, { id: crypto.randomUUID(), name: '', command: '' }]
  }

  function removeCommand(i: number) {
    terminalCommands = terminalCommands.filter((_, idx) => idx !== i)
  }

  function updateCommand(i: number, field: keyof TerminalCommand, value: string) {
    terminalCommands = terminalCommands.map((c, idx) => (idx === i ? { ...c, [field]: value } : c))
    cmdSaved = false
  }

  async function saveCommands() {
    if (!project?.id) return
    saving = true
    try {
      const res = await apiFetch(`/api/projects/${project.id}/terminal/commands`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(terminalCommands.filter((c) => c.name.trim())),
      })
      if (res.ok) cmdSaved = true
    } finally {
      saving = false
    }
  }

  function backUrl() {
    return `/projects/${org}/${projectName}`
  }
</script>

{#if loading}
  <div class="flex items-center justify-center py-20 text-gray-500">Loading...</div>
{:else if !project}
  <div class="flex items-center justify-center py-20 text-gray-500">Project not found</div>
{:else}
  <div>
    <div class="mb-6">
      <button
        onclick={() => navigate(backUrl())}
        class="text-sm text-gray-400 hover:text-white mb-3 transition-colors"
      >
        &larr; {project.name}
      </button>
      <h2 class="text-2xl font-bold text-white">项目设置</h2>
      <p class="text-sm text-gray-400 mt-1">{project.path}</p>
    </div>

    <!-- Issue Intake (需求接收) -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <div class="flex items-center justify-between mb-1">
        <h3 class="text-lg font-semibold text-white">需求接收</h3>
        {#if issueConfigured}
          <button
            onclick={clearIssueConfig}
            disabled={issueSaving}
            class="px-3 py-1 bg-gray-800 border border-gray-700 hover:bg-gray-700 rounded text-xs text-gray-300 transition-colors disabled:opacity-50"
          >
            清除（回退系统配置）
          </button>
        {/if}
      </div>
      <p class="text-xs text-gray-500 mb-4">
        配置后，本项目会话里的 /git issue 技能优先使用这里的凭证（项目级优先于系统级 [git.issue]，重启会话后生效）。
      </p>
      <div class="space-y-2">
        <div class="grid grid-cols-[100px_1fr] gap-2 items-center">
          <span class="text-sm text-gray-400">Gitea 地址</span>
          <input
            type="text"
            bind:value={issueUrl}
            oninput={() => (issueSaved = false)}
            placeholder="https://git.example.com/api/v1"
            class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500 font-mono"
          />
        </div>
        <div class="grid grid-cols-[100px_1fr] gap-2 items-center">
          <span class="text-sm text-gray-400">访问令牌</span>
          <input
            type="password"
            bind:value={issueToken}
            oninput={() => (issueSaved = false)}
            placeholder={issueHasToken ? `已配置（${issueMaskedToken}），留空保持不变` : 'repo scope 的访问令牌'}
            class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
          />
        </div>
      </div>
      <div class="flex items-center gap-3 mt-4">
        <button
          onclick={saveIssueConfig}
          disabled={issueSaving}
          class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 rounded text-sm text-white transition-colors disabled:opacity-50"
        >
          {issueSaving ? '保存中...' : '保存'}
        </button>
        {#if issueSaved}
          <span class="text-xs text-green-400">已保存</span>
        {/if}
      </div>
    </div>

    <!-- Env Vars -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-lg font-semibold text-white">环境变量</h3>
        <button
          onclick={addEnvVar}
          class="px-3 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white transition-colors"
        >
          + 添加
        </button>
      </div>

      <!-- Environments row -->
      <div class="flex items-center gap-2 flex-wrap mb-4">
        {#each envs as env (env)}
          <button
            onclick={() => selectEnv(env)}
            class={selectedEnv === env
              ? 'px-2.5 py-0.5 rounded-full text-xs bg-blue-600 text-white'
              : 'px-2.5 py-0.5 rounded-full text-xs bg-gray-800 border border-gray-700 text-gray-300 hover:bg-gray-700'}
          >
            {env}
          </button>
        {/each}
        {#if addingEnv}
          <input
            bind:value={newEnvName}
            onkeydown={(e) => e.key === 'Enter' && addEnv()}
            placeholder="dev"
            class="w-24 px-2 py-0.5 bg-gray-800 border border-gray-700 rounded-full text-xs text-white focus:outline-none focus:border-blue-500"
          />
          <button
            onclick={addEnv}
            class="px-2 py-0.5 bg-blue-600 hover:bg-blue-700 rounded-full text-xs text-white"
          >
            确定
          </button>
          <button
            onclick={() => (addingEnv = false)}
            class="text-gray-500 hover:text-gray-300 text-xs"
          >
            ×
          </button>
        {:else}
          <button
            onclick={() => (addingEnv = true)}
            class="px-2.5 py-0.5 rounded-full text-xs text-blue-400 border border-dashed border-blue-800 hover:bg-blue-900/30"
          >
            + 环境
          </button>
        {/if}
      </div>

      {#if envVars.length === 0}
        <div class="text-sm text-gray-500 text-center py-4">「{selectedEnv}」暂无环境变量</div>
      {:else}
        <div class="space-y-2">
          {#each envVars as env, i}
            <div class="grid grid-cols-[1fr_1.5fr_auto] gap-2 items-center">
              <input
                type="text"
                value={env.key}
                oninput={(e) => updateEnvVar(i, 'key', e.currentTarget.value)}
                placeholder="KEY"
                class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm font-mono text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
              />
              <input
                type="text"
                value={env.value}
                oninput={(e) => updateEnvVar(i, 'value', e.currentTarget.value)}
                placeholder="value"
                class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
              />
              <button
                onclick={() => removeEnvVar(i)}
                class="p-1.5 text-gray-400 hover:text-red-400 transition-colors"
                title="删除"
              >
                <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
                  <path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                </svg>
              </button>
            </div>
          {/each}
        </div>
      {/if}
      <div class="flex items-center gap-3 mt-4">
        <button
          onclick={saveEnvVars}
          disabled={saving}
          class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 rounded text-sm text-white transition-colors disabled:opacity-50"
        >
          {saving ? '保存中...' : `保存（${selectedEnv}）`}
        </button>
        {#if envSaved}
          <span class="text-xs text-green-400">已保存</span>
        {/if}
      </div>
    </div>

    <!-- Terminal Commands -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-lg font-semibold text-white">终端命令</h3>
        <button
          onclick={addCommand}
          class="px-3 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white transition-colors"
        >
          + 添加
        </button>
      </div>
      {#if terminalCommands.length === 0}
        <div class="text-sm text-gray-500 text-center py-4">暂无终端命令</div>
      {:else}
        <div class="space-y-2">
          {#each terminalCommands as cmd, i}
            <div class="grid grid-cols-[1fr_2fr_auto] gap-2 items-center">
              <input
                type="text"
                value={cmd.name}
                oninput={(e) => updateCommand(i, 'name', e.currentTarget.value)}
                placeholder="名称"
                class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
              />
              <input
                type="text"
                value={cmd.command}
                oninput={(e) => updateCommand(i, 'command', e.currentTarget.value)}
                placeholder="命令"
                class="bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500 font-mono"
              />
              <button
                onclick={() => removeCommand(i)}
                class="p-1.5 text-gray-400 hover:text-red-400 transition-colors"
                title="删除"
              >
                <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
                  <path stroke-linecap="round" stroke-linejoin="round" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                </svg>
              </button>
            </div>
          {/each}
        </div>
      {/if}
      <div class="flex items-center gap-3 mt-4">
        <button
          onclick={saveCommands}
          disabled={saving}
          class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 rounded text-sm text-white transition-colors disabled:opacity-50"
        >
          {saving ? '保存中...' : '保存终端命令'}
        </button>
        {#if cmdSaved}
          <span class="text-xs text-green-400">已保存</span>
        {/if}
      </div>
    </div>
  </div>
{/if}
