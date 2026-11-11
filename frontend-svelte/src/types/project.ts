export interface Project {
  id: string
  name: string
  path: string
  hostname: string
  description: string
  last_modified: string
  keyword: string
  frontend_port?: number
  backend_port?: number
  archived: boolean
}
