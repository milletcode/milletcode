<script lang="ts">
  import Approvals from '$lib/components/Approvals.svelte'

  interface Props {
    navigate: (path: string) => void
  }
  let { navigate }: Props = $props()
</script>

<Approvals {navigate} />
