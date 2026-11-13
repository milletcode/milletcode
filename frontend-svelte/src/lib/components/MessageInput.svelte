<script lang="ts">
  import { _ } from 'svelte-i18n'
  import { onMount } from 'svelte'
  import { sendWSMessage } from '$lib/wsPool'
  import { sessions, appendSessionMessage, appendWSMessage, setTurnComplete } from '$lib/stores/session'
  import { pendingInput, clearPendingInput } from '$lib/stores/approval'
  import { apiFetch } from '$lib/api'
  import { ArrowLeftRight, Bell, Link as LinkIcon } from 'lucide-svelte'
  import PortForward from './PortForward.svelte'
  import gitIcon from '../../assets/git-dark.svg'

  interface Props {
    sessionId: string
    sessionKey?: string
    projectKey: string
    projectId?: string
    disabled?: boolean
    busy?: boolean
    oninterrupt?: () => void
    onsent?: () => void
  }

  let {
    sessionId,
    sessionKey,
    projectKey,
    projectId,
    disabled = false,
    busy = false,
    oninterrupt,
    onsent,
  }: Props = $props()

  interface Model {
    id: string
    owned_by?: string
  }

  interface SlashCommand {
    name: string
    description: string
  }

  type SendMode = 'enter' | 'ctrl-enter'

  let input = $state('')
  let inputEl: HTMLTextAreaElement
  let models = $state<Model[]>([])
  let selectedModel = $state<string | null>(null)
  let showModelDropdown = $state(false)
  // Model switching can be disabled per discovery path (sync_models = false)
  let syncModels = $state(true)

  // Slash command state
  let slashCommands = $state<SlashCommand[]>([])
  let filteredCommands = $state<SlashCommand[]>([])
  let showSlashMenu = $state(false)
  let selectedCommandIndex = $state(0)

  // Custom commands state
  interface CustomCommand {
    id: string
    title: string
    command: string
    type: string
  }
  let customCommands = $state<CustomCommand[]>([])
  let showOpsMenu = $state(false)
  let showGitMenu = $state(false)
  let showPortForward = $state(false)
  let envLoading = $state(false)
  // 闲置提醒：本会话的启用开关，由后端持久化（redb）并在服务端扫描触发 —
  // 浏览器标签页关闭/休眠后依然生效。前端只负责读写这个开关。
  let idleNotifyEnabled = $state(false)
  // Send mode: 'enter' (mobile-friendly) or 'ctrl-enter' (desktop-safe)
  let sendMode = $state<SendMode>('ctrl-enter')

  // File upload state
  interface UploadedFile {
    filename: string
    path: string
    category: string
    size: number
  }

  let uploadedFiles = $state<UploadedFile[]>([])
  let isUploading = $state(false)
  let fileInput: HTMLInputElement

  // Last message sent in this session, restored into the composer on interrupt
  let lastSent = $state<{ id: string; text: string } | null>(null)

  function handleInterrupt() {
    if (!sessionId || !busy) return
    sendWSMessage({ type: 'interrupt_session', session_id: sessionId }, projectKey)
    oninterrupt?.()
    // Copy the interrupted turn's input back so the user can fix and resend.
    // Don't clobber a draft typed while the agent was running.
    if (lastSent?.id === sessionId && !input.trim()) {
      input = lastSent.text
      setTimeout(() => inputEl?.focus(), 0)
    }
    lastSent = null
  }

  // Persist send mode to localStorage
  $effect(() => {
    localStorage.setItem('message-send-mode', sendMode)
  })

  // Watch for pending input from AskUserQuestion and auto-send
  $effect(() => {
    const pending = $pendingInput.get(sessionId)
    if (pending && !disabled) {
      input = pending
      clearPendingInput(sessionId)
      // Wait for input to be set, then send
      setTimeout(() => handleSend(), 0)
    }
  })

  function toggleSendMode() {
    sendMode = sendMode === 'enter' ? 'ctrl-enter' : 'enter'
  }

  // 按会话从持久化记录恢复闲置提醒开关
  $effect(() => {
    idleNotifyEnabled = false
    if (!sessionId) return
    fetch(`/api/sessions/${sessionId}`)
      .then((r) => (r.ok ? r.json() : null))
      .then((s) => {
        if (s && typeof s.idle_notify_enabled === 'boolean') idleNotifyEnabled = s.idle_notify_enabled
      })
      .catch(() => {})
  })

  async function toggleIdleNotify() {
    if (!sessionId) return
    const next = !idleNotifyEnabled
    idleNotifyEnabled = next
    try {
      const res = await fetch(`/api/sessions/${sessionId}/idle-notify`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: next }),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
    } catch (err) {
      console.error('Idle notify toggle failed:', err)
      idleNotifyEnabled = !next
    }
  }

  // --- 项目连接：点击新开网页；未配置时首次点击弹配置气泡 ---
  let linkUrl = $state('')
  let showLinkPanel = $state(false)
  let linkInput = $state('')
  let linkSaving = $state(false)

  $effect(() => {
    linkUrl = ''
    showLinkPanel = false
    if (!projectId) return
    apiFetch(`/api/projects/${projectId}/link`)
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (d && typeof d.url === 'string') linkUrl = d.url
      })
      .catch(() => {})
  })

  // --- 弹层统一开合：点 [data-popover] 以外的任意位置关闭全部 ---
  function closeAllPanels() {
    showModelDropdown = false
    showOpsMenu = false
    showGitMenu = false
    showEnvPanel = false
    showLinkPanel = false
    showRestartPanel = false
  }

  function handleWindowClick(e: MouseEvent) {
    const t = e.target as HTMLElement | null
    if (!t) return
    if (t.closest('[data-popover]')) return
    closeAllPanels()
  }

  function toggleModelDropdown() {
    const next = !showModelDropdown
    closeAllPanels()
    showModelDropdown = next
  }

  function toggleLinkPanel() {
    const next = !showLinkPanel
    closeAllPanels()
    showLinkPanel = next
    if (next) linkInput = linkUrl
  }

  function handleLinkClick(e: MouseEvent) {
    if (linkLongPressed) {
      linkLongPressed = false
      return
    }
    if (e.altKey || !linkUrl) {
      toggleLinkPanel()
      return
    }
    // window.open 带第三参 features 串在移动端会被当弹窗拦截或降级为当前页
    // 跳转；程序化 <a target=_blank> 在用户手势同步上下文里可靠新开标签。
    const a = document.createElement('a')
    a.href = linkUrl
    a.target = '_blank'
    a.rel = 'noopener noreferrer'
    document.body.appendChild(a)
    a.click()
    a.remove()
  }

  function handleLinkContextMenu(e: MouseEvent) {
    e.preventDefault()
    if (!showLinkPanel) toggleLinkPanel()
  }

  // 触屏长按 = 右键（iOS Safari 不派发 contextmenu，需自计时）
  let linkPressTimer: ReturnType<typeof setTimeout> | null = null
  let linkLongPressed = false

  function handleLinkTouchStart() {
    linkLongPressed = false
    linkPressTimer = setTimeout(() => {
      linkLongPressed = true
      if (!showLinkPanel) toggleLinkPanel()
    }, 500)
  }

  function handleLinkTouchEnd() {
    if (linkPressTimer) {
      clearTimeout(linkPressTimer)
      linkPressTimer = null
    }
  }

  function clearLinkInput() {
    linkInput = ''
  }

  async function saveProjectLink() {
    if (!projectId) return
    linkSaving = true
    try {
      const res = await apiFetch(`/api/projects/${projectId}/link`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: linkInput }),
      })
      if (res.ok) {
        const data = await res.json()
        linkUrl = data.url || ''
        showLinkPanel = false
      }
    } catch (err) {
      console.error('Save link failed:', err)
    } finally {
      linkSaving = false
    }
  }

  // Handle file selection
  async function handleFileSelect(e: Event) {
    const input = e.target as HTMLInputElement
    const files = Array.from(input.files || [])

    for (const file of files) {
      await uploadFile(file)
    }

    // Clear input to allow selecting same file again
    input.value = ''
  }

  // Upload single file
  async function uploadFile(file: File) {
    isUploading = true

    try {
      const formData = new FormData()
      // Append File directly - preserves type and metadata
      formData.append('file', file, file.name)

      // Pass project_key so backend can forward to remote host if needed
      const uploadUrl = projectKey
        ? `/api/uploads?project_key=${encodeURIComponent(projectKey)}`
        : '/api/uploads'

      const res = await fetch(uploadUrl, {
        method: 'POST',
        body: formData,
      })

      if (!res.ok) {
        const error = await res.text()
        console.error('Upload failed:', error)
        return
      }

      const data = await res.json()
      uploadedFiles = [...uploadedFiles, data]
    } catch (err) {
      console.error('Upload error:', err)
    } finally {
      isUploading = false
    }
  }

  // Remove uploaded file
  function removeFile(index: number) {
    uploadedFiles = uploadedFiles.filter((_, i) => i !== index)
  }

  // Format file size
  function formatFileSize(bytes: number): string {
    if (bytes < 1024) return bytes + ' B'
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB'
    return (bytes / (1024 * 1024)).toFixed(1) + ' MB'
  }

  onMount(async () => {
    // Load send mode from localStorage
    const savedMode = localStorage.getItem('message-send-mode')
    if (savedMode === 'enter' || savedMode === 'ctrl-enter') {
      sendMode = savedMode
    }

    // Model switching may be disabled for this discovery path
    try {
      const res = await apiFetch('/api/settings')
      if (res.ok) {
        const data = await res.json()
        // projectKey is "keyword/project/agent" — the flag lives on the keyword
        const keyword = projectKey.split('/')[0]
        const dp = data?.project?.discovery_paths?.[keyword]
        if (dp && dp.sync_models === false) syncModels = false
      }
    } catch (err) {
      console.error('Failed to fetch discovery path settings:', err)
    }

    // Fetch current model from settings.json first
    let settingsExists = false
    try {
      const res = await apiFetch('/api/config/gpt_api/current_model')
      if (res.ok) {
        const data = await res.json()
        if (data.model) {
          selectedModel = data.model
          settingsExists = true
        }
      } else if (res.status === 404) {
        // settings.json doesn't exist
        settingsExists = false
      }
    } catch (err) {
      console.error('Failed to fetch current model:', err)
    }

    // Fetch GPT API config to get enabled models
    try {
      const res = await apiFetch('/api/config/gpt_api')
      if (res.ok) {
        const data = await res.json()
        // List the models of the active role — the same role the backend picks the
        // running model from. Offering the other role's models would show choices
        // that can't be honoured.
        const role = data.active_role === 'secondary' ? 'secondary' : 'primary'
        const activeConfig = data[role]
        if (activeConfig && Array.isArray(activeConfig.model)) {
          // Convert enabled model IDs to Model objects
          models = activeConfig.model.map((id: string) => ({ id }))
          // If settings.json doesn't exist, don't auto-select a model
          // User will see "请先配置 GPT API" message
          if (!settingsExists && models.length > 0) {
            // Keep selectedModel as null/empty to indicate GPT API not configured
            selectedModel = null
          }
        }
      }
    } catch (err) {
      console.error('Failed to fetch models config:', err)
    }

    // Fetch slash commands (with agent-specific skills)
    try {
      // Get agent_id from current session
      const currentSession = $sessions.find(s => s.id === sessionId)
      const agentId = currentSession?.agent_id || ''

      const url = agentId
        ? `/api/slash-commands?agent_id=${encodeURIComponent(agentId)}`
        : '/api/slash-commands'
      const res = await apiFetch(url)
      if (res.ok) {
        const data = await res.json()
        if (Array.isArray(data)) {
          slashCommands = data
        }
      }
    } catch (err) {
      console.error('Failed to fetch slash commands:', err)
    }

    // Fetch custom commands
    try {
      const res = await apiFetch('/api/commands')
      if (res.ok) {
        const data = await res.json()
        if (data.commands) {
          customCommands = data.commands
        }
      }
    } catch (err) {
      console.error('Failed to fetch custom commands:', err)
    }
  })

  function handleSend(append = false) {
    if ((!input.trim() && uploadedFiles.length === 0) || !sessionId || disabled) return

    // Close slash menu if open
    showSlashMenu = false

    // Build final message with file references
    let finalMessage = input.trim()

    if (uploadedFiles.length > 0) {
      const fileRefs = uploadedFiles.map((f, i) => `[附件${i + 1} - ${f.filename}](/api/uploads/preview?path=${encodeURIComponent(f.path)})`).join('\n')
      finalMessage = finalMessage
        ? `${finalMessage}\n\n请查看以下文件的内容:\n${fileRefs}`
        : `请查看以下文件的内容:\n${fileRefs}`
    }

    // Remember what was sent so an interrupt can restore it for editing — but
    // only for a regular send: an appended question is already delivered into
    // the running turn, restoring it would read as "never sent".
    if (!append) {
      lastSent = { id: sessionId, text: finalMessage }
    }

    // Append user message to stores
    const userMsg = {
      role: 'user' as const,
      type: 'user' as const,
      content: finalMessage,
      timestamp: Date.now(),
    }
    appendSessionMessage(sessionId, userMsg)
    appendWSMessage(sessionId, userMsg)

    // Send via WebSocket — backend ClientMessage::SendMessage expects type "send_message" + "content"
    sendWSMessage(
      {
        type: 'send_message',
        session_id: sessionId,
        content: finalMessage,
      },
      projectKey
    )
    // A fresh turn starts only for a regular send — an appended question rides
    // the running turn (queued into the agent's stdin), so the interrupt lock
    // and turn-complete flags must stay untouched.
    if (!append) {
      onsent?.()
      setTurnComplete(sessionId, false)
    }

    // Clear input and uploaded files
    input = ''
    uploadedFiles = []
    // Reset textarea height
    if (inputEl) {
      inputEl.style.height = 'auto'
    }
    inputEl?.focus()
  }

  // Follow-up while the agent is running: same transport as handleSend, but
  // never interrupts — the backend writes it to the agent's stdin where the
  // CLI queues it into the running turn.
  function handleAppend() {
    if (!busy) return
    handleSend(true)
  }

  function handleInput(e: Event) {
    // Use the Svelte-bound `input` variable directly — more reliable than e.target.value
    const value = input

    // Check if input starts with /
    if (value.startsWith('/')) {
      const query = value.slice(1).toLowerCase()
      filteredCommands = slashCommands.filter(cmd =>
        cmd.name.toLowerCase().includes(query) ||
        cmd.description.toLowerCase().includes(query)
      )
      showSlashMenu = filteredCommands.length > 0
      selectedCommandIndex = 0
    } else {
      showSlashMenu = false
    }

    // Auto-resize textarea
    handleAutoResize(e)
  }

  function handleAutoResize(e: Event) {
    const target = e.target as HTMLTextAreaElement
    target.style.height = 'auto'
    target.style.height = Math.min(target.scrollHeight, window.innerHeight * 0.5) + 'px'
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (showSlashMenu) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        selectedCommandIndex = selectedCommandIndex < filteredCommands.length - 1
          ? selectedCommandIndex + 1
          : selectedCommandIndex
      } else if (e.key === 'ArrowUp') {
        e.preventDefault()
        selectedCommandIndex = selectedCommandIndex > 0 ? selectedCommandIndex - 1 : 0
      } else if (e.key === 'Enter') {
        e.preventDefault()
        if (filteredCommands[selectedCommandIndex]) {
          input = filteredCommands[selectedCommandIndex].name
          showSlashMenu = false
        }
      } else if (e.key === 'Escape') {
        e.preventDefault()
        showSlashMenu = false
      }
    } else if (e.key === 'Enter') {
      // Check send mode
      if (sendMode === 'enter' && !e.shiftKey && !e.altKey && !e.ctrlKey && !e.metaKey) {
        // Enter mode: plain Enter sends, Shift+Enter for newline
        e.preventDefault()
        handleSend()
      } else if (sendMode === 'ctrl-enter' && (e.ctrlKey || e.metaKey)) {
        // Ctrl-Enter mode: Ctrl+Enter or Cmd+Enter sends
        e.preventDefault()
        handleSend()
      }
      // Otherwise, let textarea handle it (newline)
    }
  }

  function selectCommand(cmd: SlashCommand) {
    input = cmd.name
    showSlashMenu = false
    inputEl?.focus()
  }

  async function selectModel(modelId: string) {
    selectedModel = modelId
    showModelDropdown = false

    // Get current session to check agent type
    const currentSession = $sessions.find(s => s.id === sessionId)
    const agentId = currentSession?.agent_id || ''
    const isClaude = agentId === 'claude' || agentId.toLowerCase().includes('claude')
    const isSmartcode = agentId === 'smartcode'
    const isCodex = agentId === 'codex'

    // smartcode/codex：走会话级热切换（set_config），回执经 WS config_changed 确认；
    // 不发 /model 文本（console stdin 不解析 slash 命令），也不动全局 settings.json
    if ((isSmartcode || isCodex) && sessionId && !disabled) {
      try {
        await apiFetch(`/api/sessions/${sessionId}/config`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ model: modelId }),
        })
      } catch (err) {
        console.error('Failed to switch smartcode model:', err)
      }
      return
    }

    // For Claude agent: only update settings.json, don't send /model command
    // For other agents: send /model command (existing behavior)
    if (!isClaude && sessionId && !disabled) {
      const modelCommand = `/model ${modelId}`

      // Append user message to stores
      const userMsg = {
        role: 'user' as const,
        type: 'user' as const,
        content: modelCommand,
        timestamp: Date.now(),
      }
      appendSessionMessage(sessionId, userMsg)
      appendWSMessage(sessionId, userMsg)

      // Send via WebSocket
      sendWSMessage(
        {
          type: 'send_message',
          session_id: sessionId,
          content: modelCommand,
        },
        projectKey
      )
    }

    // Update settings.json for all agents
    try {
      await apiFetch('/api/config/gpt_api/switch_model', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model: modelId, project_key: projectKey }),
      })
    } catch (err) {
      console.error('Failed to update settings.json:', err)
    }
  }

  // --- 重启按钮：点击默认弹确认 dialog（可勾选"不再提示"）；右键/长按弹配置 ---
  // restartDontAsk 是唯一开关源，dialog 与配置面板的「不再提示」勾选都直接
  // 反映它 —— 两处语义必须一致（true = 点击直接发送，不再弹确认）。
  const RESTART_CMD_KEY = 'milletcode.restart.cmd'
  const RESTART_CONFIRM_KEY = 'milletcode.restart.confirm'
  let restartCmd = $state('用 make dev 重启服务')
  let restartDontAsk = $state(false)
  let showRestartConfirm = $state(false)
  let restartNoAsk = $state(false)
  let showRestartPanel = $state(false)
  let restartCmdInput = $state('')

  try {
    const saved = localStorage.getItem(RESTART_CMD_KEY)
    if (saved) restartCmd = saved
    if (localStorage.getItem(RESTART_CONFIRM_KEY) === '0') restartDontAsk = true
  } catch {
    // localStorage 不可用时保持默认：始终确认
  }

  function persistDontAsk() {
    try {
      if (restartDontAsk) localStorage.setItem(RESTART_CONFIRM_KEY, '0')
      else localStorage.removeItem(RESTART_CONFIRM_KEY)
    } catch {
      // ignore
    }
  }

  function handleRestartClick() {
    if (restartLongPressed) {
      restartLongPressed = false
      return
    }
    if (!sessionId || disabled) return
    if (restartDontAsk) {
      sendRestartCommand()
      return
    }
    restartNoAsk = false
    showRestartConfirm = true
  }

  function confirmRestart() {
    showRestartConfirm = false
    if (restartNoAsk) {
      restartDontAsk = true
      persistDontAsk()
    }
    sendRestartCommand()
  }

  function toggleRestartPanel() {
    const next = !showRestartPanel
    closeAllPanels()
    showRestartPanel = next
    if (next) restartCmdInput = restartCmd
  }

  function handleRestartContextMenu(e: MouseEvent) {
    e.preventDefault()
    if (!showRestartPanel) toggleRestartPanel()
  }

  function saveRestartCmd() {
    const text = restartCmdInput.trim()
    if (text) restartCmd = text
    try {
      localStorage.setItem(RESTART_CMD_KEY, restartCmd)
      persistDontAsk()
    } catch {
      // ignore
    }
    showRestartPanel = false
  }

  let restartPressTimer: ReturnType<typeof setTimeout> | null = null
  let restartLongPressed = false

  function handleRestartTouchStart() {
    restartLongPressed = false
    restartPressTimer = setTimeout(() => {
      restartLongPressed = true
      if (!showRestartPanel) toggleRestartPanel()
    }, 500)
  }

  function handleRestartTouchEnd() {
    if (restartPressTimer) {
      clearTimeout(restartPressTimer)
      restartPressTimer = null
    }
  }

  function sendRestartCommand() {
    if (!sessionId || disabled) return

    const restartMessage = restartCmd

    // Append user message to stores
    const userMsg = {
      role: 'user' as const,
      type: 'user' as const,
      content: restartMessage,
      timestamp: Date.now(),
    }
    appendSessionMessage(sessionId, userMsg)
    appendWSMessage(sessionId, userMsg)

    // Send via WebSocket
    sendWSMessage(
      {
        type: 'send_message',
        session_id: sessionId,
        content: restartMessage,
      },
      projectKey
    )
  }

  function sendCustomCommand(cmd: CustomCommand) {
    if (!sessionId || disabled) return

    // Close menus
    showOpsMenu = false
    showGitMenu = false

    // Append user message to stores
    const userMsg = {
      role: 'user' as const,
      type: 'user' as const,
      content: cmd.command,
      timestamp: Date.now(),
    }
    appendSessionMessage(sessionId, userMsg)
    appendWSMessage(sessionId, userMsg)

    // Send via WebSocket
    sendWSMessage(
      {
        type: 'send_message',
        session_id: sessionId,
        content: cmd.command,
      },
      projectKey
    )
  }

  function toggleOpsMenu() {
    const next = !showOpsMenu
    closeAllPanels()
    showOpsMenu = next
  }

  function toggleGitMenu() {
    const next = !showGitMenu
    closeAllPanels()
    showGitMenu = next
  }

  // --- Env panel: environments (dev/prod/...) → key-value vars per env ---
  interface EnvVar {
    key: string
    value: string
  }
  let showEnvPanel = $state(false)
  let envs = $state<string[]>([])
  let selectedEnv = $state('')
  let envVars = $state<EnvVar[]>([])
  let envPanelLoading = $state(false)
  let addingEnv = $state(false)
  let newEnvName = $state('')
  let newEnvKey = $state('')
  let newEnvValue = $state('')
  let savingVar = $state(false)

  function toggleEnvPanel() {
    const next = !showEnvPanel
    closeAllPanels()
    showEnvPanel = next
    if (next) {
      selectedEnv = ''
      envs = []
      envVars = []
      loadEnvs()
    }
  }

  async function loadEnvs() {
    if (!projectId) return
    envPanelLoading = true
    try {
      const res = await apiFetch(`/api/projects/${projectId}/envs`)
      if (res.ok) {
        const data = await res.json()
        envs = data.envs || []
        selectedEnv = envs.includes('dev') ? 'dev' : (envs[0] || 'dev')
        await loadEnvVars()
      }
    } catch (err) {
      console.error('Env panel error:', err)
    } finally {
      envPanelLoading = false
    }
  }

  async function loadEnvVars() {
    if (!projectId || !selectedEnv) return
    const res = await apiFetch(`/api/projects/${projectId}/envs/${encodeURIComponent(selectedEnv)}`)
    if (res.ok) {
      const data = await res.json()
      envVars = data.vars || []
    }
  }

  function selectEnv(env: string) {
    if (selectedEnv === env) return
    selectedEnv = env
    envVars = []
    loadEnvVars()
  }

  function addEnv() {
    const name = newEnvName.trim()
    if (!name || name.includes('/')) return
    if (!envs.includes(name)) envs = [...envs, name]
    newEnvName = ''
    addingEnv = false
    selectEnv(name)
  }

  async function addEnvVar() {
    const key = newEnvKey.trim()
    const value = newEnvValue.trim()
    if (!key || key.includes('/') || !value || !projectId || !selectedEnv) return
    savingVar = true
    try {
      const res = await apiFetch(`/api/projects/${projectId}/envs/${encodeURIComponent(selectedEnv)}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ key, value }),
      })
      if (res.ok) {
        newEnvKey = ''
        newEnvValue = ''
        if (!envs.includes(selectedEnv)) envs = [...envs, selectedEnv]
        await loadEnvVars()
      }
    } catch (err) {
      console.error('Add env var error:', err)
    } finally {
      savingVar = false
    }
  }

  async function removeEnvVar(key: string) {
    if (!projectId || !selectedEnv) return
    try {
      const res = await apiFetch(
        `/api/projects/${projectId}/envs/${encodeURIComponent(selectedEnv)}/${encodeURIComponent(key)}`,
        { method: 'DELETE' }
      )
      if (res.ok) await loadEnvVars()
    } catch (err) {
      console.error('Remove env var error:', err)
    }
  }

  async function sendEnvToAgent() {
    if (!sessionId || disabled || envVars.length === 0) return
    envLoading = true
    try {
      const envList = envVars.map((v) => `- ${v.key}: ${v.value}`).join('\n')
      const prompt =
        `请记忆如下环境变量（环境: ${selectedEnv}），后续交流中当我指定 key 或说明时，均表示以下配置，方便你开发。` +
        `为了保持一致性，我建议用 config.toml/config.yaml/.env 等符合当前技术栈的最佳实践来保存相关环境变量配置。\n\n环境变量如下：\n${envList}`

      const userMsg = {
        role: 'user' as const,
        type: 'user' as const,
        content: prompt,
        timestamp: Date.now(),
      }
      appendSessionMessage(sessionId, userMsg)
      appendWSMessage(sessionId, userMsg)
      sendWSMessage(
        {
          type: 'send_message',
          session_id: sessionId,
          content: prompt,
        },
        projectKey
      )
      showEnvPanel = false
    } finally {
      envLoading = false
    }
  }
</script>

<svelte:window onclick={handleWindowClick} />

<div class="mt-4 flex flex-col gap-2">
  <!-- Toolbar: Model selector + future tools -->
  <div class="flex items-center gap-2">
    {#if models.length > 0 && syncModels}
      {#if selectedModel}
        <div class="relative" data-popover>
          <button
            onclick={toggleModelDropdown}
            class="flex items-center gap-2 px-3 py-1.5 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-300 hover:bg-gray-700 transition-colors"
          >
            <span class="text-xs text-gray-500">Model:</span>
            <span class="font-medium">{selectedModel}</span>
            <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7" />
            </svg>
          </button>

          {#if showModelDropdown}
            <div class="absolute bottom-full left-0 mb-1 w-80 max-h-60 overflow-y-auto bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50">
              {#each models as model (model.id)}
                <button
                  onclick={() => selectModel(model.id)}
                  class="w-full px-4 py-2 text-left hover:bg-gray-700 transition-colors border-b border-gray-700 last:border-b-0"
                >
                  <div class="text-sm font-medium text-white">{model.id}</div>
                  {#if model.owned_by}
                    <div class="text-xs text-gray-500">{model.owned_by}</div>
                  {/if}
                </button>
              {/each}
            </div>
          {/if}
        </div>
      {:else}
        <div class="px-3 py-1.5 bg-yellow-900/30 border border-yellow-700 rounded-lg text-sm text-yellow-200">
          请先配置 GPT API
        </div>
      {/if}
    {/if}

    <!-- Hidden file input -->
    <input
      type="file"
      bind:this={fileInput}
      onchange={handleFileSelect}
      class="hidden"
      multiple
    />
    <button
      onclick={() => fileInput?.click()}
      disabled={isUploading || disabled}
      class="p-1.5 bg-gray-800 border border-gray-700 rounded-lg text-gray-400 hover:text-gray-200 hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
      title="上传文件"
    >
      {#if isUploading}
        <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
          <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
          <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
        </svg>
      {:else}
        <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
          <path stroke-linecap="round" stroke-linejoin="round" d="M15.172 7l-6.586 6.586a2 2 0 102.828 2.828l6.414-6.586a4 4 0 00-5.656-5.656l-6.415 6.585a6 6 0 108.486 8.486L20.5 13" />
        </svg>
      {/if}
    </button>
    <!-- Restart: click = confirm dialog; right-click / long-press = config -->
    <div class="relative" data-popover>
      <button
        onclick={handleRestartClick}
        oncontextmenu={handleRestartContextMenu}
        ontouchstart={handleRestartTouchStart}
        ontouchend={handleRestartTouchEnd}
        ontouchmove={handleRestartTouchEnd}
        disabled={disabled}
        class="p-1.5 bg-gray-800 border border-gray-700 rounded-lg text-gray-400 hover:text-gray-200 hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        title="重启服务（右键配置）"
      >
        <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
          <path stroke-linecap="round" stroke-linejoin="round" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
        </svg>
      </button>
      {#if showRestartPanel}
        <div class="absolute bottom-full left-0 mb-1 w-96 bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50 p-3">
          <div class="text-xs text-gray-500 mb-2">重启指令（发送到当前会话）</div>
          <input
            bind:value={restartCmdInput}
            placeholder="用 make dev 重启服务"
            onkeydown={(e) => e.key === 'Enter' && saveRestartCmd()}
            class="w-full px-2 py-1.5 bg-gray-900 border border-gray-600 rounded text-xs text-white focus:outline-none focus:border-blue-500"
          />
          <label class="flex items-center gap-2 mt-2 text-xs text-gray-400 cursor-pointer">
            <input type="checkbox" bind:checked={restartDontAsk} class="accent-blue-600" />
            点击时不再提示
          </label>
          <div class="flex items-center justify-between mt-2">
            <span></span>
            <button
              onclick={saveRestartCmd}
              class="px-2 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white"
            >
              保存
            </button>
          </div>
          <div class="text-[10px] text-gray-600 mt-2">右键 / 长按打开此配置</div>
        </div>
      {/if}
    </div>

    <!-- Port forward -->
    <button
      onclick={() => (showPortForward = true)}
      disabled={disabled || !sessionKey}
      class="p-1.5 bg-gray-800 border border-gray-700 rounded-lg text-gray-400 hover:text-gray-200 hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
      title={$_('portForward.title')}
    >
      <ArrowLeftRight class="w-4 h-4" />
    </button>

    <!-- Project link -->
    <div class="relative" data-popover>
      <button
        onclick={handleLinkClick}
        oncontextmenu={handleLinkContextMenu}
        ontouchstart={handleLinkTouchStart}
        ontouchend={handleLinkTouchEnd}
        ontouchmove={handleLinkTouchEnd}
        disabled={disabled || !projectId}
        class="p-1.5 bg-gray-800 border rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed {linkUrl
          ? 'border-blue-600/60 text-blue-400 hover:text-blue-300 hover:bg-blue-900/30'
          : 'border-gray-700 text-gray-400 hover:text-gray-200 hover:bg-gray-700'}"
        title={linkUrl ? $_('sessions.linkOpen') : $_('sessions.linkConfigure')}
      >
        <LinkIcon class="w-4 h-4" />
      </button>
      {#if showLinkPanel}
        <div class="absolute bottom-full left-0 mb-1 w-96 bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50 p-3">
          <div class="text-xs text-gray-500 mb-2">{$_('sessions.linkPanelTitle')}</div>
          <input
            bind:value={linkInput}
            placeholder="https://"
            onkeydown={(e) => e.key === 'Enter' && saveProjectLink()}
            class="w-full px-2 py-1.5 bg-gray-900 border border-gray-600 rounded text-xs text-white focus:outline-none focus:border-blue-500"
          />
          <div class="flex items-center justify-between mt-2">
            {#if linkUrl}
              <button
                onclick={clearLinkInput}
                disabled={linkSaving}
                class="px-2 py-1 bg-gray-700 hover:bg-red-600 rounded text-xs text-gray-300 disabled:opacity-50"
              >
                {$_('sessions.linkClear')}
              </button>
            {:else}
              <span></span>
            {/if}
            <button
              onclick={saveProjectLink}
              disabled={linkSaving}
              class="px-2 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white disabled:opacity-50"
            >
              {$_('common.save')}
            </button>
          </div>
          <div class="text-[10px] text-gray-600 mt-2">{$_('sessions.linkEditHint')}</div>
        </div>
      {/if}
    </div>

    <!-- Idle notify toggle -->
    <button
      onclick={toggleIdleNotify}
      disabled={disabled}
      class="p-1.5 bg-gray-800 border rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed {idleNotifyEnabled
        ? 'border-blue-600/60 text-blue-400 hover:text-blue-300 hover:bg-blue-900/30'
        : 'border-gray-700 text-gray-400 hover:text-gray-200 hover:bg-gray-700'}"
      title={idleNotifyEnabled ? $_('sessions.idleNotifyOn') : $_('sessions.idleNotifyOff')}
    >
      <Bell class="w-4 h-4" />
    </button>

    <!-- Spacer -->
    <div class="flex-1"></div>

    <!-- Env panel -->
    <div class="relative" data-popover>
      <button
        onclick={toggleEnvPanel}
        disabled={disabled || !projectId}
        class="px-3 py-1.5 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-300 hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        title={$_('envPanel.title')}
      >
        {#if envLoading}
          <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"></path>
          </svg>
        {:else}
          Env
        {/if}
      </button>
      {#if showEnvPanel}
        <div class="absolute bottom-full right-0 mb-1 w-96 bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50">
          <!-- Level 1: environments -->
          <div class="px-3 py-2 border-b border-gray-700">
            <div class="flex items-center justify-between mb-1.5">
              <span class="text-xs text-gray-500">{$_('envPanel.envSection')}</span>
              {#if addingEnv}
                <div class="flex items-center gap-1">
                  <input
                    bind:value={newEnvName}
                    onkeydown={(e) => e.key === 'Enter' && addEnv()}
                    placeholder={$_('envPanel.envNamePlaceholder')}
                    class="w-24 px-2 py-0.5 bg-gray-900 border border-gray-600 rounded text-xs text-white focus:outline-none focus:border-blue-500"
                  />
                  <button
                    onclick={addEnv}
                    class="px-2 py-0.5 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white"
                  >
                    {$_('common.confirm')}
                  </button>
                  <button
                    onclick={() => (addingEnv = false)}
                    class="px-1 text-gray-500 hover:text-gray-300 text-xs"
                  >
                    ×
                  </button>
                </div>
              {:else}
                <button
                  onclick={() => (addingEnv = true)}
                  class="text-xs text-blue-400 hover:text-blue-300"
                >
                  + {$_('envPanel.addEnv')}
                </button>
              {/if}
            </div>
            <div class="flex flex-wrap gap-1.5">
              {#each envs as env (env)}
                <button
                  onclick={() => selectEnv(env)}
                  class={selectedEnv === env
                    ? 'px-2.5 py-0.5 rounded-full text-xs bg-blue-600 text-white'
                    : 'px-2.5 py-0.5 rounded-full text-xs bg-gray-700 text-gray-300 hover:bg-gray-600'}
                >
                  {env}
                </button>
              {/each}
              {#if envs.length === 0}
                <span class="text-xs text-gray-600">{$_('envPanel.noEnvs')}</span>
              {/if}
            </div>
          </div>

          <!-- Level 2: vars of selected env -->
          <div class="max-h-44 overflow-y-auto px-3 py-2">
            {#if envPanelLoading}
              <div class="text-xs text-gray-500 text-center py-3">...</div>
            {:else if envVars.length === 0}
              <div class="text-xs text-gray-500 text-center py-3">{$_('envPanel.varsEmpty')}</div>
            {:else}
              {#each envVars as v (v.key)}
                <div class="flex items-center gap-2 py-1 border-b border-gray-700/50 last:border-b-0">
                  <span class="text-xs font-mono font-medium text-blue-400 shrink-0">{v.key}</span>
                  <span class="text-xs text-gray-500 shrink-0">=</span>
                  <span class="text-xs text-gray-300 truncate" title={v.value}>{v.value}</span>
                  <button
                    onclick={() => removeEnvVar(v.key)}
                    class="ml-auto shrink-0 text-gray-500 hover:text-red-400 text-sm leading-none"
                    title={$_('common.delete')}
                  >
                    ×
                  </button>
                </div>
              {/each}
            {/if}
          </div>

          <!-- Add var row -->
          <div class="px-3 py-2 border-t border-gray-700 flex items-center gap-1.5">
            <input
              bind:value={newEnvKey}
              placeholder={$_('envPanel.keyPlaceholder')}
              class="w-28 px-2 py-1 bg-gray-900 border border-gray-600 rounded text-xs font-mono text-white placeholder-gray-600 focus:outline-none focus:border-blue-500"
            />
            <input
              bind:value={newEnvValue}
              onkeydown={(e) => e.key === 'Enter' && addEnvVar()}
              placeholder={$_('envPanel.valuePlaceholder')}
              class="flex-1 px-2 py-1 bg-gray-900 border border-gray-600 rounded text-xs text-white placeholder-gray-600 focus:outline-none focus:border-blue-500"
            />
            <button
              onclick={addEnvVar}
              disabled={savingVar || !newEnvKey.trim() || !newEnvValue.trim()}
              class="px-2.5 py-1 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {$_('envPanel.add')}
            </button>
          </div>

          <!-- Footer: send to agent -->
          <div class="px-3 py-2 border-t border-gray-700 flex justify-end">
            <button
              onclick={sendEnvToAgent}
              disabled={disabled || envLoading || envVars.length === 0}
              class="px-3 py-1 bg-green-600 hover:bg-green-700 rounded text-xs text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {$_('envPanel.sendToAgent')}
            </button>
          </div>
        </div>
      {/if}
    </div>

    <!-- 操作 menu -->
    <div class="relative" data-popover>
      <button
        onclick={toggleOpsMenu}
        disabled={disabled}
        class="px-3 py-1.5 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-300 hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
      >
        操作
      </button>
      {#if showOpsMenu}
        <div class="absolute bottom-full right-0 mb-1 w-64 max-h-60 overflow-y-auto bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50">
          {#each customCommands.filter(c => c.type === 'default') as cmd (cmd.id)}
            <button
              onclick={() => sendCustomCommand(cmd)}
              class="w-full px-4 py-2 text-left hover:bg-gray-700 transition-colors border-b border-gray-700 last:border-b-0"
            >
              <div class="text-sm font-medium text-white">{cmd.title}</div>
              <div class="text-xs text-gray-500 truncate">{cmd.command}</div>
            </button>
          {:else}
            <div class="px-4 py-3 text-xs text-gray-500 text-center">暂无操作指令</div>
          {/each}
        </div>
      {/if}
    </div>

    <!-- git menu -->
    <div class="relative" data-popover>
      <button
        onclick={toggleGitMenu}
        disabled={disabled}
        class="p-1.5 bg-gray-800 border border-gray-700 rounded-lg hover:bg-gray-700 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        title="Git 指令"
      >
        <img src={gitIcon} alt="Git" class="w-4 h-4" />
      </button>
      {#if showGitMenu}
        <div class="absolute bottom-full right-0 mb-1 w-64 max-h-60 overflow-y-auto bg-gray-800 border border-gray-700 rounded-lg shadow-lg z-50">
          {#each customCommands.filter(c => c.type === 'git') as cmd (cmd.id)}
            <button
              onclick={() => sendCustomCommand(cmd)}
              class="w-full px-4 py-2 text-left hover:bg-gray-700 transition-colors border-b border-gray-700 last:border-b-0"
            >
              <div class="text-sm font-medium text-white">{cmd.title}</div>
              <div class="text-xs text-gray-500 truncate">{cmd.command}</div>
            </button>
          {:else}
            <div class="px-4 py-3 text-xs text-gray-500 text-center">暂无 Git 指令</div>
          {/each}
        </div>
      {/if}
    </div>
  </div>

  <!-- Input area -->
  <div class="flex gap-2 items-start">
    <div class="flex-1 flex flex-col gap-2">
      <!-- Uploaded files preview -->
      {#if uploadedFiles.length > 0}
        <div class="flex flex-wrap gap-2">
          {#each uploadedFiles as file, i}
            <div class="flex items-center gap-2 px-3 py-1.5 bg-gray-700 rounded-lg text-xs">
              <span class="text-gray-300 truncate max-w-[200px]">{file.filename}</span>
              <span class="text-gray-500">({formatFileSize(file.size)})</span>
              <button
                onclick={() => removeFile(i)}
                class="text-gray-400 hover:text-red-400 transition-colors"
              >
                ×
              </button>
            </div>
          {/each}
        </div>
      {/if}

      <div class="relative flex flex-col bg-gray-800 border border-gray-700 rounded-2xl focus-within:border-gray-600">
        <textarea
          bind:this={inputEl}
          bind:value={input}
          {disabled}
          onkeydown={handleKeyDown}
          oninput={handleInput}
          placeholder={$_('sessions.typeMessage')}
          rows="2"
          class="w-full bg-transparent rounded-t-2xl px-4 pt-3 pb-2 text-sm text-white placeholder-gray-500 focus:outline-none disabled:opacity-50 disabled:cursor-not-allowed resize-none overflow-y-auto"
          style="min-height: 56px; max-height: calc(50vh - 44px);"
        />

        <!-- Bottom action bar -->
        <div class="flex items-center justify-between px-2 pb-2 pt-1 border-t border-gray-700/50">
          <div class="flex items-center gap-2">
            <button
              onclick={toggleSendMode}
              class="px-2 py-1 text-xs text-gray-500 hover:text-gray-300 transition-colors"
              title="切换发送快捷键"
            >
              {sendMode === 'enter' ? 'Enter ↵' : 'Ctrl+Enter'}
            </button>
          </div>
          {#if busy && !disabled}
            <div class="flex items-center gap-2">
              <button
                onclick={handleAppend}
                disabled={isUploading || (!input.trim() && uploadedFiles.length === 0)}
                class="px-4 py-1.5 bg-green-600 hover:bg-green-700 rounded-lg text-xs font-medium text-white transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
                title={$_('sessions.appendQuestionHint')}
              >
                {$_('sessions.appendQuestion')}
              </button>
              <button
                onclick={handleInterrupt}
                class="flex items-center gap-1.5 px-4 py-1.5 bg-red-600 hover:bg-red-500 rounded-lg text-xs font-medium text-white transition-colors"
                title="中断当前对话，输入内容会复制回来以便修改"
              >
                <svg class="w-3 h-3" fill="currentColor" viewBox="0 0 24 24">
                  <rect x="6" y="6" width="12" height="12" rx="2" />
                </svg>
                终止
              </button>
            </div>
          {:else}
            <button
              onclick={() => handleSend()}
              disabled={disabled || isUploading || (!input.trim() && uploadedFiles.length === 0)}
              class="px-4 py-1.5 bg-blue-600 hover:bg-blue-700 rounded-lg text-xs font-medium text-white transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
            >
              {$_('common.send')}
            </button>
          {/if}
        </div>

        <!-- Slash command menu -->
        {#if showSlashMenu}
          <div class="absolute bottom-full left-0 right-0 mb-2 bg-gray-800 border border-gray-700 rounded-lg shadow-lg max-h-60 overflow-y-auto z-50">
            {#each filteredCommands as cmd, i}
              <button
                onclick={() => selectCommand(cmd)}
                class="w-full px-4 py-2 text-left hover:bg-gray-700 transition-colors border-b border-gray-700 last:border-b-0"
                class:bg-blue-600={i === selectedCommandIndex}
              >
                <div class="text-sm font-mono text-blue-400">{cmd.name}</div>
                <div class="text-xs text-gray-500">{cmd.description}</div>
              </button>
            {/each}
          </div>
        {/if}
      </div>
    </div>
  </div>
</div>

{#if showPortForward && sessionKey}
  <PortForward sessionKey={sessionKey} onclose={() => (showPortForward = false)} />
{/if}

{#if showRestartConfirm}
  <div class="fixed inset-0 z-[60] flex items-center justify-center">
    <div
      class="absolute inset-0 bg-black/60"
      onclick={() => (showRestartConfirm = false)}
      aria-hidden="true"
    ></div>
    <div class="relative bg-gray-800 border border-gray-700 rounded-lg shadow-2xl p-4 w-80 max-w-[90vw]">
      <div class="text-sm font-medium text-gray-100 mb-1">确认重启服务？</div>
      <div class="text-xs text-gray-500 font-mono break-all mb-3">{restartCmd}</div>
      <label class="flex items-center gap-2 text-xs text-gray-400 cursor-pointer mb-3">
        <input type="checkbox" bind:checked={restartNoAsk} class="accent-blue-600" />
        不再提示
      </label>
      <div class="flex justify-end gap-2">
        <button
          onclick={() => (showRestartConfirm = false)}
          class="px-3 py-1.5 bg-gray-700 hover:bg-gray-600 rounded text-xs text-gray-300 transition-colors"
        >
          取消
        </button>
        <button
          onclick={confirmRestart}
          class="px-3 py-1.5 bg-blue-600 hover:bg-blue-700 rounded text-xs text-white transition-colors"
        >
          确认重启
        </button>
      </div>
    </div>
  </div>
{/if}
