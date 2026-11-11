<script lang="ts">
  import { _ } from 'svelte-i18n'

  interface Props {
    sessionId: string
    onStart?: () => void
  }

  let { sessionId, onStart }: Props = $props()

  let starting = $state(false)

  async function handleStart() {
    starting = true
    try {
      // Call restore API to restart the agent
      const res = await fetch(`/api/sessions/${sessionId}/restore`, { method: 'POST' })
      if (res.ok) {
        onStart?.()
      }
    } catch {}
    // Reset starting state after a delay
    setTimeout(() => (starting = false), 3000)
  }
</script>

<div class="flex justify-center my-6">
  <div class="w-full max-w-xl">
    <div class="bg-blue-950/60 border border-blue-600/40 rounded-xl p-5">
      <div class="flex items-center gap-2 mb-3">
        <span class="text-xs text-blue-300 bg-blue-800/50 px-2.5 py-1 rounded-md font-semibold uppercase tracking-wide">
          {$_('sessions.agent')}
        </span>
      </div>
      <p class="text-base text-blue-50 font-medium mb-4">{$_('sessions.agentStopped')}</p>
      <button
        onclick={handleStart}
        disabled={starting}
        class="w-full px-4 py-3 rounded-lg transition-colors bg-blue-600 hover:bg-blue-700 disabled:bg-gray-600 text-white font-medium flex items-center justify-center gap-2"
      >
        {#if starting}
          <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
          </svg>
          {$_('common.loading')}
        {:else}
          <svg class="w-4 h-4" fill="currentColor" viewBox="0 0 20 20">
            <path fill-rule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zM9.555 7.168A1 1 0 008 8v4a1 1 0 001.555.832l3-2a1 1 0 000-1.664l-3-2z" clip-rule="evenodd" />
          </svg>
          {$_('sessions.startAgent')}
        {/if}
      </button>
    </div>
  </div>
</div>
