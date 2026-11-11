<script lang="ts">
  import { X, AlertCircle, AlertTriangle, Info } from 'lucide-svelte'
  import { notifications, removeNotification } from '$lib/stores/notification'

  let visibleNotifications = $derived($notifications.slice(-5))

  function getIcon(level: string) {
    switch (level) {
      case 'error':
        return 'error'
      case 'warning':
        return 'warning'
      default:
        return 'info'
    }
  }

  function getColor(level: string): string {
    switch (level) {
      case 'error':
        return 'bg-red-500/10 border-red-500/50 text-red-200'
      case 'warning':
        return 'bg-yellow-500/10 border-yellow-500/50 text-yellow-200'
      default:
        return 'bg-blue-500/10 border-blue-500/50 text-blue-200'
    }
  }

  function dismiss(id: string) {
    removeNotification(id)
  }

  // Auto-dismiss after 5 seconds
  $effect(() => {
    const timers: ReturnType<typeof setTimeout>[] = []
    for (const notif of visibleNotifications) {
      const timer = setTimeout(() => {
        removeNotification(notif.id)
      }, 5000)
      timers.push(timer)
    }
    return () => {
      for (const timer of timers) {
        clearTimeout(timer)
      }
    }
  })
</script>

{#if visibleNotifications.length > 0}
  <div class="fixed top-4 right-4 z-50 flex flex-col gap-2 max-w-md">
    {#each visibleNotifications as notif (notif.id)}
      <div class="flex items-start gap-3 p-4 rounded-lg border {getColor(notif.level)} animate-slide-down">
        {#if getIcon(notif.level) === 'error'}
          <AlertCircle class="w-5 h-5 flex-shrink-0 mt-0.5" />
        {:else if getIcon(notif.level) === 'warning'}
          <AlertTriangle class="w-5 h-5 flex-shrink-0 mt-0.5" />
        {:else}
          <Info class="w-5 h-5 flex-shrink-0 mt-0.5" />
        {/if}
        <div class="flex-1 text-sm">{notif.message}</div>
        <button onclick={() => dismiss(notif.id)} class="flex-shrink-0 hover:opacity-70">
          <X class="w-4 h-4" />
        </button>
      </div>
    {/each}
  </div>
{/if}
