<script lang="ts">
  import Skills from '$lib/components/SkillsPage.svelte'

  let { navigate }: { navigate: (path: string) => void } = $props()
</script>

<Skills {navigate} />
