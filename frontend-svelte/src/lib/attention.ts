import { get } from 'svelte/store'
import { writable } from 'svelte/store'
import { _ } from 'svelte-i18n'
import { pageTitle } from '$lib/pageTitle'
import { totalPendingCount, fetchPending } from '$lib/stores/approval'
import { startFaviconAlert, stopFaviconAlert } from '$lib/favicon'

const NOTIFY_KEY = 'milletcode.notify.enabled'

export const browserNotifyEnabled = writable<boolean>(false)

try {
  const saved = localStorage.getItem(NOTIFY_KEY)
  if (saved !== null) browserNotifyEnabled.set(saved === '1')
} catch {
  // localStorage unavailable (private mode etc.) — keep in-memory default
}

browserNotifyEnabled.subscribe((v) => {
  try {
    localStorage.setItem(NOTIFY_KEY, v ? '1' : '0')
  } catch {
    // ignore
  }
})

let navigateFn: ((path: string) => void) | null = null

/** Own document.title + favicon cycling, previously two $effects in App.svelte. */
export function initAttention(navigate: (path: string) => void) {
  navigateFn = navigate

  const renderTitle = () => {
    const brand = `${get(_)('brand.name')} ${get(_)('brand.tagline')}`
    const segment = get(pageTitle)
    document.title = segment ? `${segment} | ${brand}` : brand
  }

  pageTitle.subscribe(renderTitle)
  _.subscribe(renderTitle)

  totalPendingCount.subscribe((n) => {
    if (n > 0) startFaviconAlert()
    else stopFaviconAlert()
  })

  // Reconcile on tab focus: a suspended/disconnected tab may have missed
  // resolution events — without this its favicon would flash until refreshed.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') fetchPending()
  })
}

export type NotifyKind = 'approval' | 'question' | 'complete'

/** One attention-worthy happening, as data: the bridge routes WS frames to
 *  here and nothing else — copy, truncation and navigation live in this module. */
export interface AttentionEvent {
  kind: NotifyKind
  sessionId: string
  agentName?: string | null
  toolName?: string | null
  /** Prompt text / first question — the notification body for pending items. */
  detail?: string | null
}

/** "org/project/agent/session" — the agent+session tail reads best as label. */
function labelFromSessionId(sessionId: string): string {
  const parts = sessionId.split('/')
  return parts.length >= 2 ? parts.slice(-2).join('/') : sessionId
}

function truncate(text: string, max = 80): string {
  return text.length > max ? `${text.slice(0, max)}…` : text
}

function composeTitle(evt: AttentionEvent): string {
  const label = evt.agentName || labelFromSessionId(evt.sessionId)
  return `${label} · ${get(_)(`notify.${evt.kind}Title`)}`
}

function composeBody(evt: AttentionEvent): string {
  if (evt.kind === 'complete') return get(_)('notify.completeBody')
  if (evt.kind === 'approval' && evt.toolName) {
    return truncate(`${evt.toolName}: ${evt.detail || ''}`)
  }
  return truncate(evt.detail || '')
}

function composePath(evt: AttentionEvent): string {
  return evt.kind === 'complete' ? `/sessions/${evt.sessionId}` : '/approvals'
}

/**
 * Fire a browser notification. Stays silent unless the user opted in, the tab
 * is hidden, and the browser granted permission — favicon cycling covers the
 * visible-tab case. Desktop uses the page-context constructor; browsers
 * without one (Android Chrome et al.) fall back to the service worker.
 */
export function notifyAttention(evt: AttentionEvent) {
  if (!get(browserNotifyEnabled)) return
  if (typeof document === 'undefined' || !document.hidden) return
  if (!('Notification' in window) || Notification.permission !== 'granted') return
  const title = composeTitle(evt)
  const options: NotificationOptions = {
    body: composeBody(evt),
    tag: `${evt.kind}-${evt.sessionId}`,
    icon: '/logos/color-1.png',
    data: { path: composePath(evt) },
  }
  const path = composePath(evt)
  try {
    const n = new Notification(title, options)
    n.onclick = () => {
      n.close()
      window.focus()
      navigateFn?.(path)
    }
  } catch {
    // Notification constructor throws on Android-class browsers — the
    // registered service worker shows it instead (click routing lives in sw.js).
    navigator.serviceWorker
      ?.getRegistration()
      .then((reg) => reg?.showNotification(title, options))
      .catch(() => {
        // no registration / insecure context — nothing more to try
      })
  }
}

/** Ask the browser for notification permission; must run in a user gesture. */
export async function requestNotifyPermission(): Promise<'granted' | 'denied' | 'unsupported'> {
  if (!('Notification' in window)) return 'unsupported'
  try {
    const result = await Notification.requestPermission()
    return result === 'granted' ? 'granted' : 'denied'
  } catch {
    return 'denied'
  }
}

/** Current permission state for settings UI display. */
export function notifyPermissionState(): 'unsupported' | 'denied' | 'default' | 'granted' {
  if (!('Notification' in window)) return 'unsupported'
  return Notification.permission as 'denied' | 'default' | 'granted'
}
