<script lang="ts">
  import { renderMarkdown } from '$lib/markdown'
  import { onDestroy } from 'svelte'

  interface Props {
    content: string
    done?: boolean
  }

  let { content, done = false }: Props = $props()

  let html = $state('')
  let rafId: number | null = null

  function render() {
    html = renderMarkdown(content)
  }

  // RAF-throttled: during streaming, at most one render per frame
  function scheduleRender() {
    if (rafId !== null) return
    rafId = requestAnimationFrame(() => {
      rafId = null
      render()
    })
  }

  // On content change
  $effect(() => {
    void content  // track
    if (done) {
      // Synchronous render when done
      if (rafId !== null) {
        cancelAnimationFrame(rafId)
        rafId = null
      }
      render()
    } else {
      scheduleRender()
    }
  })

  onDestroy(() => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId)
      rafId = null
    }
  })
</script>

<div class="markdown-content">
  {@html html}
</div>
