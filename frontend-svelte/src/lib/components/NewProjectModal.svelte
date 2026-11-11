<script lang="ts">
  import { _ } from 'svelte-i18n'

  type AppType = 'web' | 'devops' | 'harmonyos' | 'ios' | 'android'
  type CreateMode = 'create' | 'clone'
  type RepoProto = 'git' | 'https'
  type SubmitStep = 'idle' | 'creating' | 'initializing' | 'done'

  interface Props {
    discoveryRoot: string
    basePath: string
    isMobileHome: boolean
    onClose: () => void
    onCreated: () => void
  }

  let { discoveryRoot, basePath, isMobileHome, onClose, onCreated }: Props = $props()

  const APP_TYPES: { value: AppType; labelKey: string }[] = [
    { value: 'web', labelKey: 'projects.appTypeWeb' },
    { value: 'devops', labelKey: 'projects.appTypeDevOps' },
    ...(isMobileHome ? [
      { value: 'harmonyos' as AppType, labelKey: 'projects.appTypeHarmonyOS' },
      { value: 'ios' as AppType, labelKey: 'projects.appTypeIOS' },
      { value: 'android' as AppType, labelKey: 'projects.appTypeAndroid' },
    ] : []),
  ]

  const BACKEND_LANGS = ['Rust', 'Golang', 'Python', 'Java'] as const
  const FRONTEND_LANGS = ['Svelte', 'React', 'Vue', 'ArkTS', 'Swift'] as const
  const STYLES = ['tailwindcss'] as const

  const DEFAULTS: Record<AppType, { backend: string; frontend: string | null }> = {
    web: { backend: 'Rust', frontend: 'Svelte' },
    devops: { backend: 'Golang', frontend: null },
    ios: { backend: 'Rust', frontend: 'Swift' },
    android: { backend: 'Java', frontend: null },
    harmonyos: { backend: 'Rust', frontend: 'ArkTS' },
  }

  const WEB_NAME_RE = /^[a-z][a-z0-9-]*$/
  const MOBILE_NAME_RE = /^[A-Z][a-zA-Z0-9]*$/

  const ZEN_OF_PYTHON = [
    'Beautiful is better than ugly.',
    'Explicit is better than implicit.',
    'Simple is better than complex.',
    'Complex is better than complicated.',
    'Flat is better than nested.',
    'Sparse is better than dense.',
    'Readability counts.',
    'Special cases aren\'t special enough to break the rules.',
    'Although practicality beats purity.',
    'Errors should never pass silently.',
    'Unless explicitly silenced.',
    'In the face of ambiguity, refuse the temptation to guess.',
    'There should be one — and preferably only one — obvious way to do it.',
    'Although that way may not be obvious at first unless you\'re Dutch.',
    'Now is better than never.',
    'Although never is often better than right now.',
    'If the implementation is hard to explain, it\'s a bad idea.',
    'If the implementation is easy to explain, it may be a good idea.',
    'Namespaces are one honking great idea — let\'s do more of those!',
  ]

  function randomZen(): string {
    return ZEN_OF_PYTHON[Math.floor(Math.random() * ZEN_OF_PYTHON.length)]
  }

  let mode = $state<CreateMode>('create')
  let appType = $state<AppType>('web')
  let name = $state('')
  let description = $state('')
  let backendLang = $state<string>('Rust')
  let frontendLang = $state<string | null>('Svelte')
  let style = $state<string>('tailwindcss')
  let showAdvanced = $state(false)
  let repoUrl = $state('')
  let repoProto = $state<RepoProto>('git')
  let httpUser = $state('')
  let httpPass = $state('')
  let branch = $state('')
  let cloneTags = $state(true)
  let depth = $state<string>('')
  let submitStep = $state<SubmitStep>('idle')
  let error = $state<string | null>(null)
  let zen = $state(randomZen())

  const isMobile = $derived(appType !== 'web' && appType !== 'devops')
  const nameRegex = $derived(isMobile ? MOBILE_NAME_RE : WEB_NAME_RE)
  const nameValid = $derived(name.length > 0 && nameRegex.test(name))
  const hasFrontend = $derived(frontendLang !== null)

  // Update defaults when app type changes
  function handleAppTypeChange(newType: AppType) {
    appType = newType
    const defaults = DEFAULTS[newType]
    backendLang = defaults.backend
    frontendLang = defaults.frontend
    if (!defaults.frontend) {
      style = ''
    } else if (!style) {
      style = 'tailwindcss'
    }
  }

  function buildInitPrompt(): string {
    // Build the skill command with parameters
    const params: string[] = [appType]

    if (backendLang) params.push(`--backend ${backendLang.toLowerCase()}`)
    if (frontendLang) params.push(`--frontend ${frontendLang.toLowerCase()}`)
    if (style) params.push(`--style ${style.toLowerCase()}`)
    if (description.trim()) params.push(`--description "${description.trim()}"`)

    return `/milletcode-project-init ${params.join(' ')} ${name}`
  }

  async function handleSubmitCreate() {
    error = null
    if (!nameValid) { error = $_('projects.nameInvalid'); return }

    submitStep = 'creating'
    try {
      // Step 1: Create project directory
      const projRes = await fetch('/api/projects', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name,
          app_type: appType,
          keyword: discoveryRoot,
          description: description.trim() || null,
          backend_lang: backendLang.toLowerCase(),
          frontend_lang: frontendLang?.toLowerCase() || null,
          style: style || null,
        }),
      })
      const proj = await projRes.json().catch(() => ({}))
      if (!projRes.ok) { error = proj.error || $_('projects.createFailed'); submitStep = 'idle'; return }

      submitStep = 'initializing'

      // Step 2: Create default session
      const sessionRes = await fetch('/api/sessions', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          project_id: proj.id,
          agent_id: 'claude',
          prompt: buildInitPrompt(),
          session_name: 'default',
          overwrite: true,
        }),
      })
      // Session creation failure is non-fatal
      if (!sessionRes.ok) {
        console.warn('Failed to create session:', await sessionRes.text())
      }

      submitStep = 'done'
      onCreated()
    } catch (e) {
      error = e instanceof Error ? e.message : $_('projects.createFailed')
      submitStep = 'idle'
    }
  }

  async function handleSubmitClone() {
    error = null
    if (!repoUrl.trim()) { error = $_('projects.repoUrlRequired'); return }

    const url = repoUrl.trim()
    const hasUser = httpUser.trim().length > 0
    const hasPass = httpPass.length > 0
    if (repoProto === 'https') {
      if (!/^https?:\/\//i.test(url)) { error = $_('projects.repoUrlHttpRequired'); return }
      if (hasPass && !hasUser) { error = $_('projects.clonePassNeedsUser'); return }
    }

    submitStep = 'creating'
    try {
      const body: Record<string, unknown> = {
        repo_url: url,
        keyword: discoveryRoot,
        clone_tags: cloneTags,
      }
      if (repoProto === 'https' && hasUser) {
        body.username = httpUser.trim()
        body.password = httpPass
      }
      if (branch.trim()) body.branch = branch.trim()
      if (depth.trim()) {
        const d = parseInt(depth, 10)
        if (!isNaN(d) && d > 0) body.depth = d
      }

      const res = await fetch('/api/projects/clone', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) { error = data.error || $_('projects.cloneFailed'); submitStep = 'idle'; return }

      submitStep = 'done'
      onCreated()
    } catch (e) {
      error = e instanceof Error ? e.message : $_('projects.cloneFailed')
      submitStep = 'idle'
    }
  }

  const handleSubmit = $derived(mode === 'clone' ? handleSubmitClone : handleSubmitCreate)
  const canSubmit = $derived(mode === 'clone'
    ? repoUrl.trim().length > 0
    : nameValid)
  const isSubmitting = $derived(submitStep !== 'idle' && submitStep !== 'done')

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget && !isSubmitting) onClose()
  }

  function getSubmitButtonText(): string {
    if (submitStep === 'creating') return '创建目录...'
    if (submitStep === 'initializing') return '初始化...'
    if (submitStep === 'done') return '完成'
    return mode === 'clone' ? $_('projects.clone') : $_('projects.create')
  }
</script>

<!-- Backdrop -->
<div
  class="fixed inset-0 z-[60] bg-black/50 flex items-center justify-center p-4"
  onclick={handleBackdropClick}
  role="presentation"
>
  <!-- Panel -->
  <div class="w-full max-w-lg bg-gray-900 border border-gray-800 rounded-xl shadow-2xl max-h-[85vh] flex flex-col">
    <!-- Header -->
    <div class="flex items-center justify-between px-6 py-4 border-b border-gray-800 shrink-0">
      <h3 class="text-lg font-semibold text-white">
        {mode === 'clone' ? $_('projects.cloneProject') : $_('projects.createProject')}
      </h3>
      <button
        onclick={onClose}
        disabled={isSubmitting}
        class="text-gray-500 hover:text-white transition-colors disabled:opacity-50"
      >
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" class="w-5 h-5">
          <path fill-rule="evenodd" d="M5.47 5.47a.75.75 0 011.06 0L12 10.94l5.47-5.47a.75.75 0 111.06 1.06L13.06 12l5.47 5.47a.75.75 0 11-1.06 1.06L12 13.06l-5.47 5.47a.75.75 0 01-1.06-1.06L10.94 12 5.47 6.53a.75.75 0 010-1.06z" clip-rule="evenodd" />
        </svg>
      </button>
    </div>

    <!-- Body -->
    <div class="px-6 py-5 space-y-5 overflow-y-auto flex-1">
      <!-- Mode Switcher -->
      <div class="flex gap-2 p-1 bg-gray-800 rounded-lg">
        <button
          type="button"
          onclick={() => (mode = 'create')}
          disabled={isSubmitting}
          class="flex-1 px-3 py-2 rounded-md text-sm font-medium transition-colors {mode === 'create'
            ? 'bg-blue-600 text-white'
            : 'text-gray-400 hover:text-white'}"
        >
          {$_('projects.createMode')}
        </button>
        <button
          type="button"
          onclick={() => (mode = 'clone')}
          disabled={isSubmitting}
          class="flex-1 px-3 py-2 rounded-md text-sm font-medium transition-colors {mode === 'clone'
            ? 'bg-blue-600 text-white'
            : 'text-gray-400 hover:text-white'}"
        >
          {$_('projects.cloneMode')}
        </button>
      </div>

      {#if mode === 'create'}
        <!-- App Type -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.appType')}
          </label>
          <div class="grid grid-cols-5 gap-2">
            {#each APP_TYPES as at}
              <button
                type="button"
                disabled={isSubmitting}
                onclick={() => handleAppTypeChange(at.value)}
                class="px-2 py-2 rounded-lg text-xs font-medium transition-colors border {appType === at.value
                  ? 'bg-blue-600 text-white border-blue-500'
                  : 'bg-gray-800 text-gray-400 hover:text-white border-gray-700'}"
              >
                {$_(at.labelKey)}
              </button>
            {/each}
          </div>
        </div>

        <!-- Project Name -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.projectName')}
          </label>
          <input
            type="text"
            autofocus
            value={name}
            oninput={(e) => (name = e.currentTarget.value)}
            disabled={isSubmitting}
            placeholder={isMobile ? 'MyProject' : 'my-project'}
            class="w-full bg-gray-800 border rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors {name.length > 0 && !nameValid
              ? 'border-red-500 focus:border-red-500'
              : 'border-gray-700 focus:border-blue-500'}"
          />
          {#if name.length > 0 && !nameValid}
            <p class="mt-1 text-xs text-red-400">{$_('projects.nameInvalid')}</p>
          {/if}
          <p
            onclick={() => (zen = randomZen())}
            class="mt-1 text-xs text-gray-500 cursor-default select-none"
          >
            {zen}
          </p>
        </div>

        <!-- Project Description -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            项目描述
          </label>
          <textarea
            value={description}
            oninput={(e) => (description = e.currentTarget.value)}
            disabled={isSubmitting}
            maxlength={300}
            rows={3}
            placeholder="简要描述项目用途（将用于初始化 Claude 会话）"
            class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors resize-none"
          ></textarea>
          <p class="mt-1 text-xs text-gray-500 text-right">
            {description.length}/300
          </p>
        </div>

        <!-- Advanced Options Toggle -->
        <div class="border-t border-gray-800 pt-4">
          <button
            type="button"
            onclick={() => (showAdvanced = !showAdvanced)}
            disabled={isSubmitting}
            class="w-full flex items-center justify-center gap-2 text-sm text-gray-400 hover:text-white transition-colors"
          >
            <span>高级选项</span>
            <svg
              xmlns="http://www.w3.org/2000/svg"
              viewBox="0 0 24 24"
              fill="currentColor"
              class="w-4 h-4 transition-transform {showAdvanced ? 'rotate-180' : ''}"
            >
              <path fill-rule="evenodd" d="M12.53 16.28a.75.75 0 01-1.06 0l-7.5-7.5a.75.75 0 011.06-1.06L12 14.69l6.97-6.97a.75.75 0 111.06 1.06l-7.5 7.5z" clip-rule="evenodd" />
            </svg>
          </button>
        </div>

        <!-- Advanced Options Content -->
        {#if showAdvanced}
          <div class="space-y-4">
            <!-- Backend Language -->
            <div>
              <label class="block text-sm font-medium text-gray-300 mb-2">
                后端语言
              </label>
              <div class="flex flex-wrap gap-2">
                {#each BACKEND_LANGS as lang}
                  <button
                    type="button"
                    disabled={isSubmitting}
                    onclick={() => (backendLang = backendLang === lang ? '' : lang)}
                    class="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors border {backendLang === lang
                      ? 'bg-purple-600 text-white border-purple-500'
                      : 'bg-gray-800 text-gray-400 hover:text-white border-gray-700'}"
                  >
                    {lang}
                  </button>
                {/each}
              </div>
            </div>

            <!-- Frontend Language -->
            <div>
              <label class="block text-sm font-medium text-gray-300 mb-2">
                前端语言
              </label>
              <div class="flex flex-wrap gap-2">
                {#each FRONTEND_LANGS as lang}
                  <button
                    type="button"
                    disabled={isSubmitting}
                    onclick={() => (frontendLang = frontendLang === lang ? null : lang)}
                    class="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors border {frontendLang === lang
                      ? 'bg-purple-600 text-white border-purple-500'
                      : 'bg-gray-800 text-gray-400 hover:text-white border-gray-700'}"
                  >
                    {lang}
                  </button>
                {/each}
              </div>
            </div>

            <!-- Style Framework (only if frontend selected) -->
            {#if hasFrontend}
              <div>
                <label class="block text-sm font-medium text-gray-300 mb-2">
                  样式框架
                </label>
                <div class="flex flex-wrap gap-2">
                  {#each STYLES as s}
                    <button
                      type="button"
                      disabled={isSubmitting}
                      onclick={() => (style = style === s ? '' : s)}
                      class="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors border {style === s
                        ? 'bg-purple-600 text-white border-purple-500'
                        : 'bg-gray-800 text-gray-400 hover:text-white border-gray-700'}"
                    >
                      {s}
                    </button>
                  {/each}
                </div>
              </div>
            {/if}
          </div>
        {/if}
      {:else}
        <!-- Repo Protocol -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.repoProto')}
          </label>
          <div class="flex gap-2 p-1 bg-gray-800 rounded-lg">
            <button
              type="button"
              onclick={() => (repoProto = 'git')}
              disabled={isSubmitting}
              class="flex-1 px-3 py-2 rounded-md text-sm font-medium transition-colors {repoProto === 'git'
                ? 'bg-blue-600 text-white'
                : 'text-gray-400 hover:text-white'}"
            >
              {$_('projects.repoProtoGit')}
            </button>
            <button
              type="button"
              onclick={() => (repoProto = 'https')}
              disabled={isSubmitting}
              class="flex-1 px-3 py-2 rounded-md text-sm font-medium transition-colors {repoProto === 'https'
                ? 'bg-blue-600 text-white'
                : 'text-gray-400 hover:text-white'}"
            >
              {$_('projects.repoProtoHttps')}
            </button>
          </div>
        </div>

        <!-- Repo URL -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.repoUrl')}
          </label>
          <input
            type="text"
            autofocus
            value={repoUrl}
            oninput={(e) => (repoUrl = e.currentTarget.value)}
            disabled={isSubmitting}
            placeholder={repoProto === 'https' ? 'https://github.com/org/repo.git' : 'git@github.com:org/repo.git'}
            class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors"
          />
        </div>

        {#if repoProto === 'https'}
          <!-- HTTPS Credentials -->
          <div class="space-y-3">
            <div>
              <label class="block text-sm font-medium text-gray-300 mb-2">
                {$_('projects.cloneUsername')}
              </label>
              <input
                type="text"
                value={httpUser}
                oninput={(e) => (httpUser = e.currentTarget.value)}
                disabled={isSubmitting}
                autocomplete="off"
                placeholder="username"
                class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors"
              />
            </div>
            <div>
              <label class="block text-sm font-medium text-gray-300 mb-2">
                {$_('projects.clonePassword')}
              </label>
              <input
                type="password"
                value={httpPass}
                oninput={(e) => (httpPass = e.currentTarget.value)}
                disabled={isSubmitting}
                autocomplete="new-password"
                placeholder="••••••••"
                class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors"
              />
            </div>
            <p class="text-xs text-gray-500">{$_('projects.cloneCredHint')}</p>
          </div>
        {/if}

        <!-- Branch -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.cloneBranch')}
          </label>
          <input
            type="text"
            value={branch}
            oninput={(e) => (branch = e.currentTarget.value)}
            disabled={isSubmitting}
            placeholder={$_('projects.cloneBranchHint')}
            class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors"
          />
        </div>

        <!-- Clone Tags -->
        <div class="flex items-center gap-3">
          <button
            type="button"
            onclick={() => (cloneTags = !cloneTags)}
            disabled={isSubmitting}
            class="relative w-10 h-5 rounded-full transition-colors {cloneTags ? 'bg-blue-600' : 'bg-gray-700'}"
          >
            <span
              class="absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full transition-transform {cloneTags ? 'translate-x-5' : 'translate-x-0'}"
            />
          </button>
          <span class="text-sm text-gray-300">{$_('projects.cloneTags')}</span>
        </div>

        <!-- Depth -->
        <div>
          <label class="block text-sm font-medium text-gray-300 mb-2">
            {$_('projects.cloneDepth')}
          </label>
          <input
            type="number"
            min="1"
            value={depth}
            oninput={(e) => (depth = e.currentTarget.value)}
            disabled={isSubmitting}
            placeholder={$_('projects.cloneDepthHint')}
            class="w-full bg-gray-800 border border-gray-700 focus:border-blue-500 rounded-lg px-3 py-2 text-sm text-white placeholder-gray-600 focus:outline-none transition-colors"
          />
        </div>
      {/if}

      <!-- Target Directory (read-only) -->
      <div>
        <label class="block text-sm font-medium text-gray-300 mb-2">
          {$_('projects.targetDirectory')}
        </label>
        <div class="px-3 py-2 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-400 font-mono">
          {basePath}/{mode === 'clone' ? (repoUrl.split('/').pop()?.replace(/\.git$/, '') || '<repo-name>') : (name || '<project-name>')}/
        </div>
      </div>

      <!-- Error -->
      {#if error}
        <div class="bg-red-900/30 border border-red-700 rounded-lg p-3">
          <p class="text-red-300 text-sm">{error}</p>
        </div>
      {/if}
    </div>

    <!-- Footer -->
    <div class="flex items-center justify-end gap-3 px-6 py-4 border-t border-gray-800 shrink-0">
      <button
        onclick={onClose}
        disabled={isSubmitting}
        class="px-4 py-2 rounded-lg text-sm font-medium text-gray-400 hover:text-white bg-gray-800 hover:bg-gray-700 border border-gray-700 transition-colors disabled:opacity-50"
      >
        {$_('common.cancel')}
      </button>
      <button
        onclick={handleSubmit}
        disabled={isSubmitting || !canSubmit}
        class="px-4 py-2 rounded-lg text-sm font-medium text-white bg-emerald-600 hover:bg-emerald-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
      >
        {getSubmitButtonText()}
      </button>
    </div>
  </div>
</div>
