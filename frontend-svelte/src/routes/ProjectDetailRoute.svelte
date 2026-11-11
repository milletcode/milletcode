<script lang="ts">
  import ProjectDetail from '$lib/components/ProjectDetail.svelte'

  interface Props {
    params: Record<string, string>
    navigate: (path: string) => void
  }

  let { params, navigate }: Props = $props()
</script>

<ProjectDetail {params} {navigate} />
