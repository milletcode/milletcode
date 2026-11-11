import { writable } from 'svelte/store'

export interface Notification {
  id: string
  level: 'info' | 'warning' | 'error'
  message: string
  ts: number
}

export const notifications = writable<Notification[]>([])

export function addNotification(n: Notification) {
  notifications.update((current) => [...current, n])
}

export function removeNotification(id: string) {
  notifications.update((current) => current.filter((n) => n.id !== id))
}

export function clearAll() {
  notifications.set([])
}
