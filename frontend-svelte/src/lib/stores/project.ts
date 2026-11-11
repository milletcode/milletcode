import { writable } from 'svelte/store'
import type { Project } from '../../types/project'
import { apiFetch } from '../api'

export const projects = writable<Project[]>([])
export const activeProject = writable<string | null>(null)

export function setProjects(p: Project[]) {
  projects.set(p)
}

export function setActiveProject(id: string | null) {
  activeProject.set(id)
}

export async function fetchProjects() {
  try {
    const res = await apiFetch('/api/projects')
    const data = await res.json()
    projects.set(data)
  } catch {
    // ignore
  }
}

export async function refreshProjects() {
  try {
    const res = await apiFetch('/api/projects/refresh', { method: 'POST' })
    const data = await res.json()
    projects.set(data)
  } catch {
    // ignore
  }
}

export function getProjectById(id: string): Project | undefined {
  let result: Project | undefined
  projects.subscribe((p) => {
    result = p.find((proj) => proj.id === id)
  })()
  return result
}
