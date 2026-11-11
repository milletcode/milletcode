<script lang="ts">
  interface AgentOption {
    id: string
    name: string
    release_channel?: string
  }

  interface Props {
    agents: AgentOption[]
    selected: string
  }

  let { agents, selected = $bindable() }: Props = $props()

  let open = $state(false)
  let highlighted = $state(-1)
  let wrap: HTMLDivElement | undefined = $state()

  const current = $derived(agents.find((a) => a.id === selected))

  function badgeClass(channel?: string): string {
    if (channel === 'beta') return 'bg-amber-500/15 text-amber-300 border border-amber-500/30'
    if (channel === 'alpha') return 'bg-violet-500/15 text-violet-300 border border-violet-500/30'
    return ''
  }

  function select(agent: AgentOption) {
    selected = agent.id
    open = false
    highlighted = -1
  }

  function toggle() {
    if (open) {
      open = false
      highlighted = -1
    } else {
      highlighted = agents.findIndex((a) => a.id === selected)
      open = true
    }
  }

  function handleWindowClick(e: MouseEvent) {
    const t = e.target as Node
    if (wrap && t && !wrap.contains(t)) {
      open = false
      highlighted = -1
    }
  }

  function handleWindowKeydown(e: KeyboardEvent) {
    if (!open) return
    if (e.key === 'Escape') {
      open = false
      highlighted = -1
      return
    }
    // 键盘导航只在焦点落在本组件内时接管，避免抢走 prompt 输入框的 Enter 启动
    const ae = document.activeElement
    if (!wrap || !ae || !wrap.contains(ae)) return
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      highlighted = Math.min(highlighted + 1, agents.length - 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      highlighted = Math.max(highlighted - 1, 0)
    } else if (e.key === 'Enter' && highlighted >= 0) {
      e.preventDefault()
      select(agents[highlighted])
    }
  }
</script>

<svelte:window onclick={handleWindowClick} onkeydown={handleWindowKeydown} />

<div class="relative" bind:this={wrap}>
  <button
    type="button"
    onclick={toggle}
    aria-haspopup="listbox"
    aria-expanded={open}
    class="flex items-center justify-between gap-3 min-w-[10rem] bg-gray-800 border border-gray-700 rounded-lg px-3 py-2 text-sm text-white focus:outline-none focus:border-blue-500 hover:border-gray-600 transition-colors"
  >
    <span class="flex items-center gap-2">
      {current?.name ?? selected}
      {#if current && current.release_channel && current.release_channel !== 'stable'}
        <span class="px-2 py-0.5 rounded text-xs font-medium {badgeClass(current.release_channel)}">
          {current.release_channel}
        </span>
      {/if}
    </span>
    <svg class="w-4 h-4 text-gray-400 shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24" stroke-width="2">
      <path stroke-linecap="round" stroke-linejoin="round" d="M19 9l-7 7-7-7" />
    </svg>
  </button>

  {#if open}
    <div
      role="listbox"
      class="absolute z-50 mt-1 w-full min-w-[12rem] bg-gray-800 border border-gray-700 rounded-lg shadow-lg overflow-hidden py-1"
    >
      {#each agents as agent, i (agent.id)}
        <button
          type="button"
          role="option"
          aria-selected={agent.id === selected}
          onclick={() => select(agent)}
          onmouseenter={() => (highlighted = i)}
          class="w-full flex items-center justify-between gap-3 px-3 py-2 text-sm text-left transition-colors {i === highlighted ? 'bg-blue-600 text-white' : 'text-gray-200'}"
        >
          <span>{agent.name}</span>
          {#if agent.release_channel && agent.release_channel !== 'stable'}
            <span class="px-2 py-0.5 rounded text-xs font-medium {badgeClass(agent.release_channel)}">
              {agent.release_channel}
            </span>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>
