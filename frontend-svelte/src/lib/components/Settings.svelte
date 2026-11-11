<script lang="ts">
  import { onMount } from 'svelte'
  import { _ } from 'svelte-i18n'
  import { refreshProjects } from '$lib/stores/project'

  interface DiscoveryPathInfo {
    path: string
    hostname: string
    is_mobile_home: boolean
    ssh?: { config_path: string } | null
    paths?: Record<string, string>
  }

  interface Settings {
    env: string
    protect_mode?: boolean
    ssh?: { config_path: string; passphrase: string | null }
    agent: {
      default_agent: string | null
      approval_timeout_secs: number
      instances: Record<string, { name: string; enabled: boolean }>
    }
    logs: {
      session_log_dir: string | null
      session_log_enabled: boolean
      output: string
      level: string
      max_files: number
    }
    project: {
      scan_interval_secs: number
      scanner_enabled: boolean
      discovery_paths?: Record<string, DiscoveryPathInfo>
      sessions: { max_subscriptions_per_project: string; max_total_sessions: string }
    }
    web: { base_url: string | null }
    git: { issue: { url: string; token: string } }
  }

  interface AuthInfo {
    mode: string
    basic_auth_user: string | null
    password_configured: boolean
    proxy_domain: string | null
  }

  interface GptApiConfig {
    host: string
    model: string[]
    api_key: string
    models_url?: string
  }

  interface ApiModel {
    id: string
  }

  interface ModelOption {
    id: string
    // Saved in config but absent from the gateway's current /v1/models response.
    stale: boolean
  }

  interface Agent {
    id: string
    name: string
    enabled: boolean
  }

  interface SshHostInfo {
    alias: string
    hostname: string | null
    user: string | null
    port: number | null
    identity_file: string | null
  }

  interface CustomCommand {
    id: string
    title: string
    command: string
    type: string
  }

  let agents = $state<Agent[]>([])
  let settings = $state<Settings | null>(null)
  let sshHosts = $state<SshHostInfo[]>([])
  let authInfo = $state<AuthInfo | null>(null)
  let primary = $state<GptApiConfig | null>(null)
  let secondary = $state<GptApiConfig | null>(null)
  let saved = $state(false)
  let saveError = $state('')
  let models = $state<{ primary: ApiModel[]; secondary: ApiModel[] }>({
    primary: [],
    secondary: [],
  })
  let fetchingModels = $state<{ primary: boolean; secondary: boolean }>({
    primary: false,
    secondary: false,
  })
  let modelFetchError = $state<{ primary: string; secondary: string }>({
    primary: '',
    secondary: '',
  })
  let modelsFetched = $state<{ primary: boolean; secondary: boolean }>({
    primary: false,
    secondary: false,
  })
  let activeRole = $state<'primary' | 'secondary'>('primary')
  let refreshing = $state(false)
  let activeTab = $state<'config' | 'settings'>('settings')

  // Custom commands state
  let customCommands = $state<CustomCommand[]>([])
  let newCmdTitle = $state('')
  let newCmdCommand = $state('')
  let newCmdType = $state('default')
  let cmdSaving = $state(false)
  let cmdResetting = $state(false)
  let protectedCommands = $state('')
  let historyCount = $state(1000)
  let protectedSaving = $state(false)
  let protectedSaved = $state(false)
  // 技能安装记录保留条数（skills.redb，0 = 永久）
  let skillsHistoryCount = $state(100)
  let skillsHistorySaving = $state(false)
  let skillsHistorySaved = $state(false)
  let autoUpdate = $state(true)
  let autoUpdateSaving = $state(false)
  // 闲置提醒：全局开关 + 阈值（秒，30..=900）
  let idleReminderEnabled = $state(true)
  let idleSeconds = $state(120)
  let idleSaving = $state(false)
  // 端口打开方式：proxy = 反向代理（默认）；tunnel = 客户端隧道
  let openMode = $state<'proxy' | 'tunnel'>('proxy')
  let openModeSaving = $state(false)

  onMount(async () => {
    try {
      const [agentsRes, settingsRes, gptApiRes, authRes, activeRoleRes, commandsRes, sshHostsRes, protectedRes, autoUpdateRes, openModeRes, idleReminderRes, skillsHistoryRes] = await Promise.all([
        fetch('/api/agents').then((r) => r.json()),
        fetch('/api/settings').then((r) => r.json()),
        fetch('/api/config/gpt_api').then((r) => r.json()),
        fetch('/api/settings/auth').then((r) => r.json()),
        fetch('/api/config/gpt_api/active_role').then((r) => r.json()),
        fetch('/api/commands').then((r) => r.json()),
        fetch('/api/settings/ssh-hosts').then((r) => r.json()),
        fetch('/api/config/protected_commands').then((r) => r.json()),
        fetch('/api/config/auto_update').then((r) => r.json()),
        // 未设置时 404，保持默认 proxy
        fetch('/api/config/forwards.open_mode').then((r) => (r.ok ? r.json() : null)),
        fetch('/api/config/idle_reminder').then((r) => r.json()),
        fetch('/api/skills/history-count').then((r) => (r.ok ? r.json() : null)),
      ])

      agents = agentsRes
      settings = settingsRes
      if (gptApiRes.primary) primary = gptApiRes.primary
      if (gptApiRes.secondary) secondary = gptApiRes.secondary
      authInfo = authRes
      if (activeRoleRes.role) activeRole = activeRoleRes.role
      if (commandsRes.commands) customCommands = commandsRes.commands
      if (Array.isArray(sshHostsRes)) sshHosts = sshHostsRes
      if (Array.isArray(protectedRes.commands)) protectedCommands = protectedRes.commands.join('\n')
      if (typeof protectedRes.history_count === 'number') historyCount = protectedRes.history_count
      if (typeof autoUpdateRes.enabled === 'boolean') autoUpdate = autoUpdateRes.enabled
      if (openModeRes === 'tunnel') openMode = 'tunnel'
      if (typeof idleReminderRes.enabled === 'boolean') idleReminderEnabled = idleReminderRes.enabled
      if (typeof idleReminderRes.idle_seconds === 'number') idleSeconds = idleReminderRes.idle_seconds
      if (typeof skillsHistoryRes?.count === 'number') skillsHistoryCount = skillsHistoryRes.count
    } catch (err) {
      console.error('Failed to load settings:', err)
    }
  })

  async function setOpenMode(mode: 'proxy' | 'tunnel') {
    if (openModeSaving || openMode === mode) return
    openModeSaving = true
    try {
      const res = await fetch('/api/config/forwards.open_mode', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        // 后端存 JSON 字符串值："proxy" / "tunnel"
        body: JSON.stringify(mode),
      })
      if (res.ok) openMode = mode
    } catch (err) {
      console.error('Failed to set open mode:', err)
    } finally {
      openModeSaving = false
    }
  }

  async function toggleAutoUpdate() {
    if (autoUpdateSaving) return
    autoUpdateSaving = true
    try {
      const res = await fetch('/api/config/auto_update', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: !autoUpdate }),
      })
      if (res.ok) {
        const data = await res.json()
        if (typeof data.enabled === 'boolean') autoUpdate = data.enabled
      }
    } catch (err) {
      console.error('Failed to toggle auto update:', err)
    } finally {
      autoUpdateSaving = false
    }
  }

  async function saveIdleReminder(enabled: boolean, seconds = idleSeconds) {
    if (idleSaving) return
    idleSaving = true
    try {
      const res = await fetch('/api/config/idle_reminder', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled, idle_seconds: Math.min(900, Math.max(30, seconds)) }),
      })
      if (res.ok) {
        const data = await res.json()
        if (typeof data.enabled === 'boolean') idleReminderEnabled = data.enabled
        if (typeof data.idle_seconds === 'number') idleSeconds = data.idle_seconds
      }
    } catch (err) {
      console.error('Failed to save idle reminder settings:', err)
    } finally {
      idleSaving = false
    }
  }

  async function fetchModels(role: 'primary' | 'secondary') {
    const config = role === 'primary' ? primary : secondary
    if (!config || !config.host) return

    fetchingModels = { ...fetchingModels, [role]: true }
    modelFetchError = { ...modelFetchError, [role]: '' }

    try {
      const isMasked = !config.api_key || config.api_key === '*********'
      if (!isMasked) {
        const saveRes = await fetch(`/api/config/settings.gpt_api.${role}`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(config),
        })
        if (!saveRes.ok) {
          const err = await saveRes.json().catch(() => ({ error: 'Save failed' }))
          modelFetchError = { ...modelFetchError, [role]: err.error }
          return
        }
      }

      const res = await fetch(`/api/config/gpt_api/${role}/models`)
      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Unknown error' }))
        modelFetchError = {
          ...modelFetchError,
          [role]: err.error || `Failed (${res.status})`,
        }
        return
      }

      const data = await res.json()
      const list: ApiModel[] = (data.data || []).sort((a: ApiModel, b: ApiModel) =>
        a.id.localeCompare(b.id)
      )
      models = { ...models, [role]: list }
      modelsFetched = { ...modelsFetched, [role]: true }
    } catch (e: unknown) {
      modelFetchError = {
        ...modelFetchError,
        [role]: e instanceof Error ? e.message : 'Network error',
      }
    } finally {
      fetchingModels = { ...fetchingModels, [role]: false }
    }
  }

  async function saveGptApi(role: 'primary' | 'secondary') {
    const config = role === 'primary' ? primary : secondary
    if (!config) return

    saveError = ''
    try {
      const isMasked = !config.api_key || config.api_key === '*********'
      const payload = isMasked ? { ...config, api_key: undefined } : config
      const res = await fetch(`/api/config/settings.gpt_api.${role}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      })

      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Unknown error' }))
        saveError = err.error || 'Failed to save'
        return
      }

      saved = true
      setTimeout(() => (saved = false), 2000)
      refreshProjects()
    } catch {
      saveError = 'Network error'
    }
  }

  function updateConfig(role: 'primary' | 'secondary', field: keyof GptApiConfig, value: string) {
    if (role === 'primary') {
      primary = primary ? { ...primary, [field]: value } : null
    } else {
      secondary = secondary ? { ...secondary, [field]: value } : null
    }
  }

  function toggleModel(role: 'primary' | 'secondary', modelId: string) {
    if (role === 'primary') {
      if (!primary) return
      const selected = primary.model || []
      const updated = selected.includes(modelId)
        ? selected.filter((m) => m !== modelId)
        : [...selected, modelId]
      primary = { ...primary, model: updated }
    } else {
      if (!secondary) return
      const selected = secondary.model || []
      const updated = selected.includes(modelId)
        ? selected.filter((m) => m !== modelId)
        : [...selected, modelId]
      secondary = { ...secondary, model: updated }
    }
  }

  // A gateway's /v1/models is not a complete view of what is saved: it can stop
  // serving a model that stays in config.model, and it is only fetched on demand.
  // Since select_model_by_priority reads the whole saved list, anything missing
  // here would keep affecting model selection while being invisible and
  // unremovable. So render the union.
  //
  // Saved-only entries are flagged "stale" only after a fetch has actually run —
  // before that we simply haven't asked the gateway, so calling them missing
  // would be wrong.
  function modelOptions(
    config: GptApiConfig | null,
    fetched: ApiModel[],
    fetchedOnce: boolean
  ): ModelOption[] {
    const saved = config?.model || []
    const fetchedIds = new Set(fetched.map((m) => m.id))
    return [
      ...fetched.map((m) => ({ id: m.id, stale: false })),
      ...saved.filter((id) => !fetchedIds.has(id)).map((id) => ({ id, stale: fetchedOnce })),
    ]
  }

  function selectAllModels(role: 'primary' | 'secondary', roleModels: ApiModel[]) {
    if (role === 'primary') {
      if (!primary) return
      const selected = primary.model || []
      const allSelected = roleModels.every((m) => selected.includes(m.id))
      primary = { ...primary, model: allSelected ? [] : roleModels.map((m) => m.id) }
    } else {
      if (!secondary) return
      const selected = secondary.model || []
      const allSelected = roleModels.every((m) => selected.includes(m.id))
      secondary = { ...secondary, model: allSelected ? [] : roleModels.map((m) => m.id) }
    }
  }

  async function deleteGptApi(role: 'primary' | 'secondary') {
    saveError = ''
    try {
      const res = await fetch(`/api/config/settings.gpt_api.${role}`, {
        method: 'DELETE',
      })

      if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'Delete failed' }))
        saveError = err.error || 'Failed to delete'
        return
      }

      if (role === 'primary') primary = null
      else secondary = null

      saved = true
      setTimeout(() => (saved = false), 2000)
    } catch {
      saveError = 'Network error'
    }
  }

  function initConfig(role: 'primary' | 'secondary') {
    const config = { host: '', model: [], api_key: '', models_url: '' }
    if (role === 'primary') primary = config
    else secondary = config
  }

  // Base URL actually used for model discovery: models_url wins when set.
  function modelsBase(config: GptApiConfig): string {
    const override = (config.models_url || '').trim()
    return override || (config.host || '').trim()
  }

  function modelsUrlPreview(config: GptApiConfig): string {
    const base = modelsBase(config).replace(/\/+$/, '')
    return base ? `${base}/v1/models` : ''
  }

  async function handleActiveRoleChange(role: 'primary' | 'secondary') {
    saveError = ''
    try {
      const res = await fetch('/api/config/gpt_api/active_role', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ role }),
      })

      if (res.ok) {
        activeRole = role
      } else {
        const err = await res.json().catch(() => ({ error: `HTTP ${res.status}` }))
        saveError = err.error || 'Failed to switch active role'
      }
    } catch (e) {
      console.error('Failed to set active role:', e)
      saveError = 'Network error'
    }
  }

  async function refreshClaudeSettings() {
    refreshing = true
    saveError = ''
    try {
      const res = await fetch('/api/config/gpt_api/refresh', { method: 'POST' })
      const data = await res.json()
      if (res.ok) {
        saved = true
        setTimeout(() => (saved = false), 2000)
      } else {
        saveError = data.error || 'Failed to refresh'
      }
    } catch (e) {
      console.error('Failed to refresh config:', e)
      saveError = 'Network error'
    } finally {
      refreshing = false
    }
  }

  async function toggleAgent(agent: Agent) {
    const newEnabled = !agent.enabled
    try {
      const res = await fetch(`/api/agents/${agent.id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: newEnabled }),
      })
      if (res.ok) {
        agents = agents.map((a) => (a.id === agent.id ? { ...a, enabled: newEnabled } : a))
      }
    } catch {
      // ignore
    }
  }

  async function saveProtectedCommands() {
    protectedSaving = true
    protectedSaved = false
    try {
      const commands = protectedCommands
        .split('\n')
        .map((line) => line.trim())
        .filter(Boolean)
      const res = await fetch('/api/config/protected_commands', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          commands,
          history_count: Math.max(0, Math.floor(Number(historyCount) || 0)),
        }),
      })
      if (res.ok) {
        protectedSaved = true
        setTimeout(() => (protectedSaved = false), 2000)
      }
    } catch {
      // ignore
    } finally {
      protectedSaving = false
    }
  }

  async function saveSkillsHistoryCount() {
    skillsHistorySaving = true
    skillsHistorySaved = false
    try {
      const res = await fetch('/api/skills/history-count', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ count: Math.max(0, Math.floor(Number(skillsHistoryCount) || 0)) }),
      })
      if (res.ok) {
        skillsHistorySaved = true
        setTimeout(() => (skillsHistorySaved = false), 2000)
      }
    } catch {
      // ignore
    } finally {
      skillsHistorySaving = false
    }
  }

  // Custom commands functions
  async function fetchCommands() {
    try {
      const res = await fetch('/api/commands')
      const data = await res.json()
      if (data.commands) customCommands = data.commands
    } catch (err) {
      console.error('Failed to fetch commands:', err)
    }
  }

  async function createCommand() {
    if (!newCmdTitle.trim() || !newCmdCommand.trim()) return
    cmdSaving = true
    try {
      const res = await fetch('/api/commands', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          title: newCmdTitle.trim(),
          command: newCmdCommand.trim(),
          type: newCmdType,
        }),
      })
      if (res.ok) {
        newCmdTitle = ''
        newCmdCommand = ''
        newCmdType = 'default'
        await fetchCommands()
      }
    } catch (err) {
      console.error('Failed to create command:', err)
    } finally {
      cmdSaving = false
    }
  }

  async function deleteCommand(id: string) {
    try {
      const res = await fetch(`/api/commands/${id}`, { method: 'DELETE' })
      if (res.ok) {
        await fetchCommands()
      }
    } catch (err) {
      console.error('Failed to delete command:', err)
    }
  }

  async function resetCommands() {
    cmdResetting = true
    try {
      const res = await fetch('/api/commands/reset', { method: 'POST' })
      if (res.ok) {
        await fetchCommands()
      }
    } catch (err) {
      console.error('Failed to reset commands:', err)
    } finally {
      cmdResetting = false
    }
  }
</script>

<div>
  <h2 class="text-2xl font-bold text-white mb-6">{$_('settings.title')}</h2>

  <!-- Tab Switcher -->
  <div class="mb-6 inline-flex rounded-lg bg-gray-800 p-0.5">
    <button
      onclick={() => activeTab = 'settings'}
      class="px-4 py-1.5 rounded-md text-sm font-medium transition-colors {activeTab === 'settings'
        ? 'bg-blue-600 text-white'
        : 'text-gray-400 hover:text-white'}"
    >
      {$_('settings.tabSettings')}
    </button>
    {#if settings?.env === 'debug'}
    <button
      onclick={() => activeTab = 'config'}
      class="px-4 py-1.5 rounded-md text-sm font-medium transition-colors {activeTab === 'config'
        ? 'bg-blue-600 text-white'
        : 'text-gray-400 hover:text-white'}"
    >
      {$_('settings.tabConfig')}
    </button>
    {/if}
  </div>

  {#if activeTab === 'settings'}
    <!-- ========== USER SETTINGS TAB ========== -->

    <!-- Agents -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <h3 class="text-lg font-semibold text-white mb-4">
        {$_('settings.agentsTitle')}
      </h3>

      {#if agents.length === 0}
        <p class="text-gray-500 text-sm">{$_('settings.noAgents')}</p>
      {:else}
        <div class="space-y-3">
          {#each agents as agent (agent.id)}
            <div
              class="flex items-center gap-4 bg-gray-800 rounded-lg p-4 border border-gray-700"
            >
              <div class="flex-1">
                <p class="text-sm font-medium text-white">{agent.name}</p>
                <p class="text-xs text-gray-500">{agent.id}</p>
              </div>
              <button
                onclick={() => toggleAgent(agent)}
                class="px-3 py-1 rounded text-xs font-medium transition-colors {agent.enabled
                  ? 'bg-green-500/20 text-green-400 hover:bg-green-500/30'
                  : 'bg-gray-700 text-gray-500 hover:bg-gray-600'}"
              >
                {agent.enabled ? $_('settings.enabledStatus') : $_('settings.disabledStatus')}
              </button>
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <!-- Protected commands (保护指令) -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <h3 class="text-lg font-semibold text-white mb-1">
        {$_('settings.protectedTitle')}
      </h3>
      <p class="text-xs text-gray-500 mb-4">{$_('settings.protectedDesc')}</p>
      <textarea
        bind:value={protectedCommands}
        rows="6"
        placeholder={$_('settings.protectedPlaceholder')}
        class="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white font-mono placeholder-gray-600 focus:outline-none focus:border-blue-500"
      ></textarea>
      <div class="flex items-center gap-2 mt-3">
        <label class="text-xs text-gray-400" for="history-count">{$_('settings.historyCountLabel')}</label>
        <input
          id="history-count"
          type="number"
          min="0"
          bind:value={historyCount}
          class="w-24 bg-gray-800 border border-gray-700 rounded-lg px-2 py-1 text-sm text-white focus:outline-none focus:border-blue-500"
        />
        <span class="text-xs text-gray-500">{$_('settings.historyCountHint')}</span>
      </div>
      <div class="flex items-center gap-3 mt-3">
        <button
          onclick={saveProtectedCommands}
          disabled={protectedSaving}
          class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 disabled:bg-gray-600 rounded text-sm font-medium text-white transition-colors"
        >
          {protectedSaving ? $_('settings.fetching') : $_('settings.save')}
        </button>
        {#if protectedSaved}
          <span class="text-xs text-green-400">{$_('settings.saved')}</span>
        {/if}
      </div>
    </div>

    <!-- Skill install records retention (技能安装记录保留) -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <div class="flex items-center gap-2">
        <label class="text-xs text-gray-400" for="skills-history-count">{$_('settings.skillsHistoryCountLabel')}</label>
        <input
          id="skills-history-count"
          type="number"
          min="0"
          bind:value={skillsHistoryCount}
          class="w-24 bg-gray-800 border border-gray-700 rounded-lg px-2 py-1 text-sm text-white focus:outline-none focus:border-blue-500"
        />
        <span class="text-xs text-gray-500">{$_('settings.skillsHistoryCountHint')}</span>
      </div>
      <div class="flex items-center gap-3 mt-3">
        <button
          onclick={saveSkillsHistoryCount}
          disabled={skillsHistorySaving}
          class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 disabled:bg-gray-600 rounded text-sm font-medium text-white transition-colors"
        >
          {skillsHistorySaving ? $_('settings.fetching') : $_('settings.save')}
        </button>
        {#if skillsHistorySaved}
          <span class="text-xs text-green-400">{$_('settings.saved')}</span>
        {/if}
      </div>
    </div>

    <!-- Port open mode (端口打开方式) -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <h3 class="text-lg font-semibold text-white mb-1">{$_('settings.openModeTitle')}</h3>
      <p class="text-xs text-gray-500 mb-4">{$_('settings.openModeDesc')}</p>
      <div class="space-y-3">
        <label class="flex items-start gap-2 cursor-pointer select-none">
          <input
            type="radio"
            name="port-open-mode"
            checked={openMode === 'proxy'}
            onchange={() => setOpenMode('proxy')}
            disabled={openModeSaving}
            class="mt-0.5 w-4 h-4 border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-0"
          />
          <span>
            <span class="block text-sm text-gray-300">{$_('settings.openModeProxy')}</span>
            {#if openMode === 'proxy'}
              <span class="block text-xs text-gray-500">{$_('settings.openModeProxyHint')}</span>
            {/if}
          </span>
        </label>
        <label class="flex items-start gap-2 cursor-pointer select-none">
          <input
            type="radio"
            name="port-open-mode"
            checked={openMode === 'tunnel'}
            onchange={() => setOpenMode('tunnel')}
            disabled={openModeSaving}
            class="mt-0.5 w-4 h-4 border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-0"
          />
          <span>
            <span class="block text-sm text-gray-300">{$_('settings.openModeTunnel')}</span>
            {#if openMode === 'tunnel'}
              <span class="block text-xs text-gray-500">{$_('settings.openModeTunnelHint')}</span>
            {/if}
          </span>
        </label>
      </div>
    </div>

    <!-- Idle Reminder -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <h3 class="text-lg font-semibold text-white mb-4">{$_('settings.idleReminderLabel')}</h3>
      <label class="flex items-start gap-2 mb-3 cursor-pointer select-none">
        <input
          type="checkbox"
          checked={idleReminderEnabled}
          onchange={() => saveIdleReminder(!idleReminderEnabled)}
          disabled={idleSaving}
          class="mt-0.5 w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-0"
        />
        <span>
          <span class="block text-sm text-gray-300">{$_('settings.idleReminderLabel')}</span>
          <span class="block text-xs text-gray-500">{$_('settings.idleReminderHint')}</span>
        </span>
      </label>
      {#if idleReminderEnabled}
        <div class="flex items-center gap-2">
          <label for="idle-seconds" class="text-sm text-gray-300">{$_('settings.idleSecondsLabel')}</label>
          <input
            id="idle-seconds"
            type="number"
            min="30"
            max="900"
            step="10"
            bind:value={idleSeconds}
            onchange={() => saveIdleReminder(idleReminderEnabled)}
            disabled={idleSaving}
            class="w-20 bg-gray-800 border border-gray-600 rounded px-2 py-1 text-sm text-white focus:outline-none focus:border-blue-500 disabled:opacity-50"
          />
        </div>
      {/if}
    </div>

    <!-- Custom Commands -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-lg font-semibold text-white">{$_('settings.customCommandsTitle')}</h3>
        <button
          onclick={resetCommands}
          disabled={cmdResetting}
          class="px-3 py-1.5 bg-red-600/20 hover:bg-red-600/30 disabled:bg-gray-600 rounded text-xs text-red-400 transition-colors"
        >
          {cmdResetting ? $_('settings.resetting') : $_('settings.resetDefault')}
        </button>
      </div>

      <label class="flex items-start gap-2 mb-4 cursor-pointer select-none">
        <input
          type="checkbox"
          checked={autoUpdate}
          onchange={toggleAutoUpdate}
          disabled={autoUpdateSaving}
          class="mt-0.5 w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-0"
        />
        <span>
          <span class="block text-sm text-gray-300">{$_('settings.autoUpdateLabel')}</span>
          <span class="block text-xs text-gray-500">{$_('settings.autoUpdateHint')}</span>
        </span>
      </label>

      <!-- Add new command -->
      <div class="mb-4 p-4 bg-gray-800 rounded-lg border border-gray-700">
        <div class="grid grid-cols-3 gap-2 mb-2">
          <input
            type="text"
            bind:value={newCmdTitle}
            placeholder={$_('settings.cmdTitlePlaceholder')}
            class="bg-gray-700 border border-gray-600 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
          />
          <input
            type="text"
            bind:value={newCmdCommand}
            placeholder={$_('settings.cmdCommandPlaceholder')}
            class="bg-gray-700 border border-gray-600 rounded px-3 py-1.5 text-sm text-white placeholder-gray-500 focus:outline-none focus:border-blue-500"
          />
          <select
            bind:value={newCmdType}
            class="bg-gray-700 border border-gray-600 rounded px-3 py-1.5 text-sm text-white focus:outline-none focus:border-blue-500"
          >
            <option value="default">{$_('settings.cmdTypeAction')}</option>
            <option value="git">git</option>
          </select>
        </div>
        <button
          onclick={createCommand}
          disabled={cmdSaving || !newCmdTitle.trim() || !newCmdCommand.trim()}
          class="w-full px-3 py-1.5 bg-blue-600 hover:bg-blue-700 disabled:bg-gray-600 rounded text-sm text-white transition-colors"
        >
          {cmdSaving ? $_('settings.adding') : $_('settings.addCommand')}
        </button>
      </div>

      <!-- Commands list -->
      <div class="space-y-2">
        {#if customCommands.length === 0}
          <p class="text-gray-500 text-sm text-center py-4">{$_('settings.noCustomCommands')}</p>
        {:else}
          {#each customCommands as cmd (cmd.id)}
            <div class="flex items-center gap-3 p-3 bg-gray-800 rounded-lg border border-gray-700">
              <div class="flex-1 min-w-0">
                <div class="flex items-center gap-2 mb-1">
                  <span class="text-sm font-medium text-white">{cmd.title}</span>
                  <span class="text-xs px-1.5 py-0.5 bg-gray-700 text-gray-400 rounded">{cmd.type}</span>
                </div>
                <p class="text-xs text-gray-500 truncate font-mono">{cmd.command}</p>
              </div>
              <button
                onclick={() => deleteCommand(cmd.id)}
                class="p-1.5 text-gray-500 hover:text-red-400 transition-colors"
                title={$_('common.delete')}
              >
                <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                </svg>
              </button>
            </div>
          {/each}
        {/if}
      </div>
    </div>

    <!-- GPT API Keys -->
    <div class="bg-gray-900 rounded-xl border border-gray-800 p-6">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-lg font-semibold text-white">{$_('settings.gptApiTitle')}</h3>
        <button
          onclick={refreshClaudeSettings}
          disabled={refreshing}
          class="flex items-center gap-2 px-3 py-1.5 bg-blue-600 hover:bg-blue-700 disabled:bg-gray-600 rounded text-sm text-white transition-colors"
        >
          <svg class="w-4 h-4 {refreshing ? 'animate-spin' : ''}" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
          </svg>
          {$_('settings.refreshConfig')}
        </button>
      </div>

      <!-- Active Role Tabs -->
      <div class="mb-4 flex items-center gap-2">
        <span class="text-sm text-gray-400 mr-1">{$_('settings.activeRole')}:</span>
        <div class="inline-flex rounded-lg bg-gray-800 p-0.5">
          {#each ['primary', 'secondary'] as role}
            <button
              onclick={() => handleActiveRoleChange(role as 'primary' | 'secondary')}
              class="px-3 py-1 rounded-md text-sm font-medium transition-colors {activeRole === role
                ? 'bg-blue-600 text-white'
                : 'text-gray-400 hover:text-white'}"
            >
              {role === 'primary' ? $_('settings.primary') : $_('settings.secondary')}
            </button>
          {/each}
        </div>
      </div>

      {#if saveError}
        <p class="text-red-400 text-sm mb-4">{saveError}</p>
      {/if}
      {#if saved}
        <p class="text-green-400 text-sm mb-4">{$_('settings.saved')}</p>
      {/if}

      {#each ['primary', 'secondary'] as role}
        {@const config = role === 'primary' ? primary : secondary}
        {@const label = role.charAt(0).toUpperCase() + role.slice(1)}
        {@const roleModels = models[role as 'primary' | 'secondary']}
        {@const isFetching = fetchingModels[role as 'primary' | 'secondary']}
        {@const fetchErr = modelFetchError[role as 'primary' | 'secondary']}
        {@const options = modelOptions(
          config,
          roleModels,
          modelsFetched[role as 'primary' | 'secondary']
        )}
        {@const staleCount = options.filter((o) => o.stale).length}
        {@const fetchedOnce = modelsFetched[role as 'primary' | 'secondary']}

        <div class="mb-6 last:mb-0">
          <div class="flex items-center justify-between mb-3">
            <h4 class="text-sm font-medium text-gray-300">{label}</h4>
            {#if config}
              <div class="flex gap-2">
                <button
                  onclick={() => saveGptApi(role as 'primary' | 'secondary')}
                  class="px-3 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs font-medium text-white transition-colors"
                >
                  {$_('settings.save')}
                </button>
                <button
                  onclick={() => deleteGptApi(role as 'primary' | 'secondary')}
                  class="px-3 py-1 bg-red-600/20 hover:bg-red-600/30 rounded text-xs font-medium text-red-400 transition-colors"
                >
                  {$_('settings.delete')}
                </button>
              </div>
            {/if}
          </div>

          {#if !config}
            <button
              onclick={() => initConfig(role as 'primary' | 'secondary')}
              class="px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-sm text-gray-400 transition-colors"
            >
              {$_('settings.addKey', { values: { label } })}
            </button>
          {:else}
            <div class="space-y-3">
              <div>
                <label class="text-xs text-gray-500 mb-1 block">
                  {$_('settings.host')}
                </label>
                <input
                  type="text"
                  value={config.host}
                  oninput={(e) => updateConfig(role as 'primary' | 'secondary', 'host', e.currentTarget.value)}
                  placeholder="https://api.example.com"
                  class="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white font-mono placeholder-gray-500 focus:outline-none focus:border-blue-500"
                />
              </div>

              <div>
                <label class="text-xs text-gray-500 mb-1 block">
                  {$_('settings.modelsUrl')}
                </label>
                <input
                  type="text"
                  value={config.models_url ?? ''}
                  oninput={(e) => updateConfig(role as 'primary' | 'secondary', 'models_url', e.currentTarget.value)}
                  placeholder={modelsUrlPreview(config)}
                  class="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white font-mono placeholder-gray-500 focus:outline-none focus:border-blue-500"
                />
                <p class="text-xs text-gray-500 mt-1">
                  {$_('settings.modelsUrlHint')}
                </p>
              </div>

              <div>
                <label class="text-xs text-gray-500 mb-1 block">
                  {$_('settings.model')}
                </label>
                <div class="flex gap-2 mb-2">
                  <button
                    onclick={() => fetchModels(role as 'primary' | 'secondary')}
                    disabled={isFetching || !config.host}
                    class="px-4 py-2 bg-gray-700 hover:bg-gray-600 rounded-lg text-sm font-medium text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed whitespace-nowrap"
                  >
                    {isFetching ? $_('settings.fetching') : $_('settings.fetchModels')}
                  </button>
                </div>

                {#if fetchErr}
                  <p class="text-red-400 text-xs mt-1">{fetchErr}</p>
                {/if}

                {#if options.length > 0}
                  <div>
                    <div class="flex items-center justify-between mb-2">
                      <p class="text-gray-500 text-xs">
                        {#if fetchedOnce}
                          {$_('settings.modelsAvailable', { values: { count: roleModels.length } })}
                          {#if staleCount > 0}
                            <span class="text-amber-500/80">
                              · {$_('settings.modelsStale', { values: { count: staleCount } })}
                            </span>
                          {/if}
                        {:else}
                          {$_('settings.modelsConfigured', {
                            values: { count: config?.model?.length ?? 0 },
                          })}
                        {/if}
                      </p>
                      <button
                        onclick={() => selectAllModels(role as 'primary' | 'secondary', roleModels)}
                        class="text-xs text-blue-400 hover:text-blue-300"
                      >
                        {$_('settings.toggleAll')}
                      </button>
                    </div>
                    <div class="grid grid-cols-2 gap-1 max-h-48 overflow-y-auto bg-gray-800 rounded-lg p-3 border border-gray-700">
                      {#each options as m (m.id)}
                        {@const isSelected = (config?.model || []).includes(m.id)}
                        <label
                          class="flex items-center gap-2 px-2 py-1.5 rounded hover:bg-gray-700 cursor-pointer text-sm"
                          title={m.stale ? $_('settings.modelStaleHint') : undefined}
                        >
                          <input
                            type="checkbox"
                            checked={isSelected}
                            onchange={() => toggleModel(role as 'primary' | 'secondary', m.id)}
                            class="w-4 h-4 rounded border-gray-600 bg-gray-700 text-blue-500 focus:ring-blue-500 focus:ring-offset-0"
                          />
                          <span
                            class="font-mono text-xs truncate {m.stale ? 'text-amber-400' : 'text-white'}"
                          >
                            {m.id}
                          </span>
                        </label>
                      {/each}
                    </div>
                  </div>
                {/if}
              </div>

              <div>
                <label class="text-xs text-gray-500 mb-1 block">
                  {$_('settings.apiKey')}
                </label>
                <input
                  type="password"
                  value={config.api_key === '*********' ? '' : config.api_key}
                  oninput={(e) => updateConfig(role as 'primary' | 'secondary', 'api_key', e.currentTarget.value)}
                  placeholder={config.api_key === '*********'
                    ? $_('settings.maskedPlaceholder')
                    : 'sk-...'}
                  class="w-full bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white font-mono placeholder-gray-500 focus:outline-none focus:border-blue-500"
                />
              </div>
            </div>
          {/if}
        </div>
      {/each}
    </div>

  {:else}
    <!-- ========== SYSTEM CONFIG TAB ========== -->
    {#if settings}

      <!-- SSH Section -->
      {#if !settings.protect_mode}
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">SSH</h3>
        <div class="grid grid-cols-2 gap-4 text-sm mb-4">
          <div>
            <span class="text-gray-500">{$_('settings.sshConfigPath')}</span>
            <p class="text-white font-mono">{settings.ssh?.config_path || '—'}</p>
          </div>
        </div>

        <!-- SSH Hosts -->
        <div>
          <h5 class="text-xs font-medium text-gray-500 mb-2">{$_('settings.sshHosts')}</h5>
          {#if sshHosts.length === 0}
            <p class="text-xs text-gray-600">{$_('settings.noSshHosts')}</p>
          {:else}
            <div class="grid grid-cols-2 gap-2">
              {#each sshHosts as host (host.alias)}
                <div class="bg-gray-800 rounded-lg p-3 border border-gray-700">
                  <div class="flex items-center gap-2 mb-1">
                    <span class="text-sm font-medium text-white">{host.alias}</span>
                    {#if host.hostname}
                      <span class="text-xs px-1.5 py-0.5 bg-gray-700 text-gray-300 rounded font-mono">
                        {host.hostname}
                      </span>
                    {/if}
                  </div>
                  <div class="text-xs text-gray-500 space-y-0.5">
                    {#if host.user}
                      <div>User: <span class="font-mono">{host.user}</span></div>
                    {/if}
                    {#if host.port}
                      <div>Port: <span class="font-mono">{host.port}</span></div>
                    {/if}
                    {#if host.identity_file}
                      <div>Identity: <span class="font-mono">{host.identity_file}</span></div>
                    {/if}
                  </div>
                </div>
              {/each}
            </div>
          {/if}
        </div>
      </div>
      {/if}

      <!-- Agent Section -->
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">Agent</h3>
        <div class="grid grid-cols-2 gap-4 text-sm">
          <div>
            <span class="text-gray-500">{$_('settings.defaultAgent')}</span>
            <p class="text-white font-mono">{settings.agent.default_agent || '—'}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.approvalTimeout')}</span>
            <p class="text-white font-mono">{settings.agent.approval_timeout_secs}s</p>
          </div>
        </div>
      </div>

      <!-- Git Issue Section (read-only, managed in config.toml) -->
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">{$_('settings.gitIssueTitle')}</h3>
        <p class="text-xs text-gray-500 mb-4">{$_('settings.gitIssueHint')}</p>
        <div class="grid grid-cols-2 gap-4 text-sm">
          <div>
            <span class="text-gray-500">{$_('settings.gitIssueUrl')}</span>
            <p class="text-white font-mono">{settings.git?.issue?.url || '—'}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.gitIssueToken')}</span>
            <p class="text-white font-mono">{settings.git?.issue?.token || '—'}</p>
          </div>
        </div>
      </div>

      <!-- Web Section -->
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">Web</h3>
        <div class="grid grid-cols-2 gap-4 text-sm">
          <div>
            <span class="text-gray-500">Base URL</span>
            <p class="text-white font-mono">{settings.web.base_url || '—'}</p>
          </div>
        </div>
      </div>

      <!-- Logs Section -->
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">Logs</h3>
        <div class="grid grid-cols-2 gap-4 text-sm">
          <div>
            <span class="text-gray-500">{$_('settings.sessionLogEnabled')}</span>
            <p class="text-white font-mono">{settings.logs.session_log_enabled ? $_('settings.yes') : $_('settings.no')}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.sessionLogDir')}</span>
            <p class="text-white font-mono">{settings.logs.session_log_dir || '—'}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.logOutput')}</span>
            <p class="text-white font-mono">{settings.logs.output || '—'}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.logLevel')}</span>
            <p class="text-white font-mono">{settings.logs.level || '—'}</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.logMaxFiles')}</span>
            <p class="text-white font-mono">{settings.logs.max_files}</p>
          </div>
        </div>
      </div>

      <!-- Project Section -->
      <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
        <h3 class="text-lg font-semibold text-white mb-4">Project</h3>
        <div class="grid grid-cols-2 gap-4 text-sm">
          <div>
            <span class="text-gray-500">{$_('settings.scanInterval')}</span>
            <p class="text-white font-mono">{settings.project.scan_interval_secs}s</p>
          </div>
          <div>
            <span class="text-gray-500">{$_('settings.projectScanner')}</span>
            <p class="text-white font-mono">{settings.project.scanner_enabled ? $_('settings.enabled') : $_('settings.disabled')}</p>
          </div>
        </div>
      </div>

      <!-- Auth -->
      {#if authInfo && !settings.protect_mode}
        <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
          <h3 class="text-lg font-semibold text-white mb-4">
            {$_('settings.authTitle')}
          </h3>
          <div class="grid grid-cols-2 gap-4 text-sm">
            <div>
              <span class="text-gray-500">{$_('settings.authType')}</span>
              <p class="text-white font-mono">{authInfo.mode || '—'}</p>
            </div>
            <div>
              <span class="text-gray-500">{$_('settings.authUser')}</span>
              <p class="text-white font-mono">
                {authInfo.basic_auth_user || '—'}
              </p>
            </div>
            <div>
              <span class="text-gray-500">{$_('settings.authPassword')}</span>
              <p
                class="text-sm font-medium {authInfo.password_configured ? 'text-green-400' : 'text-red-400'}"
              >
                {authInfo.password_configured
                  ? $_('settings.passwordConfigured')
                  : $_('settings.passwordNotConfigured')}
              </p>
            </div>
            {#if authInfo.mode === 'oauth2_proxy'}
              <div>
                <span class="text-gray-500">{$_('settings.oauthProxyDomain')}</span>
                <p class="text-white font-mono">{authInfo.proxy_domain || '—'}</p>
              </div>
            {/if}
          </div>
        </div>
      {/if}

      <!-- Discovery Paths (Debug only) -->
      {#if settings.env === 'debug' && !settings.protect_mode && settings.project.discovery_paths}
        <div class="bg-gray-900 rounded-xl border border-gray-800 p-6 mb-6">
          <h3 class="text-lg font-semibold text-white mb-4">Discovery Paths</h3>
          <div class="grid grid-cols-2 gap-2">
            {#each Object.entries(settings.project.discovery_paths) as [keyword, dp]}
              <div class="bg-gray-800 rounded-lg p-3 border border-gray-700">
                <div class="flex items-center gap-2 mb-2">
                  <span class="text-sm font-medium text-white">{keyword}</span>
                  <span class="text-xs px-1.5 py-0.5 bg-gray-700 text-gray-300 rounded font-mono">
                    {dp.hostname || 'localhost'}
                  </span>
                  {#if dp.is_mobile_home}
                    <span class="text-xs" title={$_('settings.mobileProject')}>📱</span>
                  {/if}
                </div>
                <div class="text-xs text-gray-500 font-mono">{dp.path}</div>
                {#if dp.ssh}
                  <div class="text-xs text-gray-500 mt-1">
                    SSH: {dp.ssh.config_path}
                  </div>
                {/if}
              </div>
            {/each}
          </div>
        </div>
      {/if}
    {/if}
  {/if}
</div>
