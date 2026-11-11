<script lang="ts">
  import Dashboard from '$lib/components/Dashboard.svelte'

  interface Props {
    navigate: (path: string) => void
  }

  let { navigate }: Props = $props()
</script>

<Dashboard {navigate} />
