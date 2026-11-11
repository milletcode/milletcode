<script lang="ts">
  import { _ } from 'svelte-i18n'
  import TerminalPane from '$lib/components/TerminalPane.svelte'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate: _navigate }: Props = $props()

  const projectId = $derived(
    params.org && params.project
      ? `${decodeURIComponent(params.org)}/${decodeURIComponent(params.project)}`
      : '',
  )
  const projectName = $derived(decodeURIComponent(params.project || projectId.split('/').pop() || ''))
</script>

{#if projectId}
  <TerminalPane project={{ id: projectId, name: projectName }} onclose={() => window.close()} />
{:else}
  <div class="h-full flex items-center justify-center text-gray-500 text-sm">
    {$_('terminal.missingProject')}
  </div>
{/if}
