<script lang="ts">
  import SessionDetail from '$lib/components/SessionDetail.svelte'
  import { pageTitle } from '$lib/pageTitle'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate }: Props = $props()

  // e.g. "agent-demo@smartcode | MilletCode® 多Agent调度平台" — project first,
  // it is what identifies the session; discovery path as qualifier.
  $effect(() => {
    pageTitle.set(params.org && params.project ? `${params.project}@${params.org}` : null)
    return () => pageTitle.set(null)
  })
</script>

<!-- {#key}: force a clean remount when switching between sessions of the same
     route — otherwise the in-place switcher navigates here and stale internal
     state can render an empty thread. -->
{#key `${params.org}/${params.project}/${params.agent}/${params.name}`}
  <SessionDetail {params} {navigate} />
{/key}
