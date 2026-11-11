import { writable } from 'svelte/store'

/**
 * Page-specific title segment, e.g. "smartcode/agent-demo". null = show only
 * the brand title. Routes set this on mount / clear it on destroy; App.svelte
 * composes the final document.title so locale switches re-render it.
 */
export const pageTitle = writable<string | null>(null)
