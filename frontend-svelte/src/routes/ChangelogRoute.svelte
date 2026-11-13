<script lang="ts">
  import { onMount } from 'svelte'
  import { _, locale } from 'svelte-i18n'
  import { Lock, LockOpen, BookOpen, Bot, Copy, Github, Headphones, Heart } from 'lucide-svelte'
  import { apiFetch } from '$lib/api'
  import { renderMarkdown } from '$lib/markdown'

  const PW_KEY = 'milletcode.changelog.pw'

  type Gate = 'checking' | 'locked' | 'open'

  let gate = $state<Gate>('checking')
  let pwInput = $state('')
  let error = $state('')
  let busy = $state(false)
  let content = $state('')
  let contentEn = $state('')
  let version = $state('')

  // markdown 里的 [[beta]] / [[alpha]] 标记渲染成扁平徽章。markdown-it html:false 会转义
  // 原始 HTML，所以只能渲染后再注入 —— 内容来自自家后端，可信。
  const BETA_BADGE =
    '<span style="display:inline-flex;align-items:center;gap:3px;vertical-align:middle;margin-right:6px;padding:1px 7px;border-radius:9999px;font-size:10px;font-weight:500;line-height:1.5;letter-spacing:.02em;color:#fcd34d;background:rgba(245,158,11,.15);border:1px solid rgba(245,158,11,.3);">' +
    '<svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 2v7.527a2 2 0 0 1-.211.896L4.72 20.55a1 1 0 0 0 .9 1.45h12.76a1 1 0 0 0 .9-1.45l-5.069-10.127A2 2 0 0 1 14 9.527V2"/><path d="M8.5 2h7"/><path d="M7 16h10"/></svg>' +
    'Beta</span>'
  const ALPHA_BADGE =
    '<span style="display:inline-flex;align-items:center;gap:3px;vertical-align:middle;margin-right:6px;padding:1px 7px;border-radius:9999px;font-size:10px;font-weight:500;line-height:1.5;letter-spacing:.02em;color:#f87171;background:rgba(239,68,68,.15);border:1px solid rgba(239,68,68,.3);">' +
    '<svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"/></svg>' +
    'Alpha</span>'

  // 双语正文：zh 恒有；en 缺失（旧后端）时回落 zh
  const displayContent = $derived.by(() => {
    if (($locale || '').startsWith('en')) return contentEn || content
    return content
  })

  const contentHtml = $derived.by(() => {
    if (!displayContent) return ''
    return renderMarkdown(displayContent)
      .replaceAll('[[beta]]', BETA_BADGE)
      .replaceAll('[[alpha]]', ALPHA_BADGE)
  })

  function storedPassword(): string {
    try {
      return localStorage.getItem(PW_KEY) || ''
    } catch {
      return ''
    }
  }

  async function unlock(password: string, remember: boolean) {
    if (!password || busy) return
    busy = true
    error = ''
    try {
      const res = await apiFetch('/api/changelog', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      })
      if (res.ok) {
        const data = await res.json()
        if (remember) {
          try {
            localStorage.setItem(PW_KEY, password)
          } catch {
            // localStorage 不可用就只在本次会话内展示
          }
        }
        content = data.content || ''
        contentEn = data.content_en || ''
        gate = 'open'
      } else if (res.status === 403) {
        error = $_('changelog.wrongPassword')
        gate = 'locked'
      } else {
        error = `${$_('changelog.loadFailed')} (HTTP ${res.status})`
        gate = 'locked'
      }
    } catch (e: any) {
      error = e?.message || String(e)
      gate = 'locked'
    } finally {
      busy = false
    }
  }

  function submit() {
    unlock(pwInput.trim(), true)
  }

  function lock() {
    try {
      localStorage.removeItem(PW_KEY)
    } catch {
      // ignore
    }
    pwInput = ''
    content = ''
    contentEn = ''
    error = ''
    gate = 'locked'
  }

  onMount(() => {
    fetch('/version.json', { cache: 'no-store' })
      .then((r) => r.json())
      .then((d) => {
        version = d.version || ''
      })
      .catch(() => {})

    const saved = storedPassword()
    if (saved) {
      unlock(saved, true)
    } else {
      gate = 'locked'
    }
  })
</script>

<div class="max-w-3xl mx-auto">
  {#if gate === 'checking'}
    <div class="flex items-center justify-center h-64 text-gray-500">
      <Lock class="w-5 h-5 animate-pulse" />
    </div>
  {:else if gate === 'locked'}
    <!-- 密码门：幽默声明 + 输密码 -->
    <div class="max-w-md mx-auto mt-16">
      <div class="bg-gray-900 border border-gray-700/60 rounded-2xl shadow-2xl p-6">
        <div class="flex items-center gap-3 mb-1">
          <div class="w-10 h-10 rounded-xl bg-blue-600/15 border border-blue-600/30 flex items-center justify-center shrink-0">
            <BookOpen class="w-5 h-5 text-blue-400" />
          </div>
          <div>
            <h1 class="text-lg font-bold text-white">{$_('changelog.gateTitle')}</h1>
            <p class="text-xs text-gray-500">{$_('changelog.gateSubtitle')}</p>
          </div>
        </div>

        <!-- 声明 -->
        <div class="mt-4 space-y-2">
          <div class="flex gap-2 bg-gray-800/60 border border-gray-700/50 rounded-lg px-3 py-2.5">
            <Bot class="w-4 h-4 text-blue-400 shrink-0 mt-0.5" />
            <p class="text-xs leading-relaxed text-gray-400">{$_('changelog.noteAi')}</p>
          </div>
          <div class="flex gap-2 bg-gray-800/60 border border-gray-700/50 rounded-lg px-3 py-2.5">
            <Copy class="w-4 h-4 text-amber-400 shrink-0 mt-0.5" />
            <p class="text-xs leading-relaxed text-gray-400">{$_('changelog.noteOriginal')}</p>
          </div>
          <div class="flex gap-2 bg-gray-800/60 border border-gray-700/50 rounded-lg px-3 py-2.5">
            <Github class="w-4 h-4 text-green-400 shrink-0 mt-0.5" />
            <p class="text-xs leading-relaxed text-gray-400">
              {$_('changelog.noteOpenPrefix')}
              <a
                href="https://github.com/milletcode/milletcode"
                target="_blank"
                rel="noopener"
                class="text-blue-400 hover:underline break-all">https://github.com/milletcode/milletcode</a>{$_('changelog.noteOpenSuffix')}
            </p>
          </div>
          <div class="flex gap-2 bg-gray-800/60 border border-gray-700/50 rounded-lg px-3 py-2.5">
            <Headphones class="w-4 h-4 text-sky-400 shrink-0 mt-0.5" />
            <p class="text-xs leading-relaxed text-gray-400">{$_('changelog.noteSupport')}</p>
          </div>
          <div class="flex gap-2 bg-gray-800/60 border border-gray-700/50 rounded-lg px-3 py-2.5">
            <Heart class="w-4 h-4 text-red-400 shrink-0 mt-0.5" />
            <p class="text-xs leading-relaxed text-gray-400">{$_('changelog.noteCustomers')}</p>
          </div>
        </div>

        <form onsubmit={(e) => { e.preventDefault(); submit() }} class="mt-5">
          <div class="flex gap-2">
            <input
              type="password"
              bind:value={pwInput}
              placeholder={$_('changelog.passwordPlaceholder')}
              autocomplete="off"
              class="flex-1 px-3 py-2 bg-gray-800 border border-gray-600 rounded-lg text-sm text-white placeholder-gray-600 focus:outline-none focus:border-blue-500"
            />
            <button
              type="submit"
              disabled={busy || !pwInput.trim()}
              class="px-4 py-2 bg-blue-600 hover:bg-blue-700 rounded-lg text-sm font-medium text-white transition-colors disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-1.5"
            >
              <LockOpen class="w-4 h-4" />
              {$_('changelog.unlock')}
            </button>
          </div>
          {#if error}
            <p class="mt-2 text-xs text-red-400">{error}</p>
          {/if}
        </form>
      </div>
    </div>
  {:else}
    <!-- 已解锁：更新日志正文 -->
    <div class="flex items-center justify-between mb-4">
      <div class="flex items-center gap-3">
        <div class="w-10 h-10 rounded-xl bg-blue-600/15 border border-blue-600/30 flex items-center justify-center shrink-0">
          <BookOpen class="w-5 h-5 text-blue-400" />
        </div>
        <div>
          <h1 class="text-lg font-bold text-white">{$_('changelog.title')}</h1>
          {#if version}
            <p class="text-xs text-gray-500 font-mono">MilletCode® {version}</p>
          {/if}
        </div>
      </div>
      <button
        onclick={lock}
        class="px-3 py-1.5 bg-gray-800 border border-gray-700 rounded-lg text-xs text-gray-400 hover:text-gray-200 hover:bg-gray-700 transition-colors flex items-center gap-1.5"
        title={$_('changelog.lockTitle')}
      >
        <Lock class="w-3.5 h-3.5" />
        {$_('changelog.lockAction')}
      </button>
    </div>
    <div class="bg-gray-900 border border-gray-700/60 rounded-2xl shadow-2xl p-6">
      <div class="markdown-content">{@html contentHtml}</div>
    </div>
  {/if}
</div>
