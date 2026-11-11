/**
 * Global fetch interceptor for OAuth 401 handling.
 *
 * When the backend returns 401, the frontend proxy (Vite dev / Nginx prod)
 * should route /oauth2/* to the OAuth2 Proxy.
 * This module patches window.fetch to detect 401 and redirect to login.
 */

const originalFetch = window.fetch.bind(window)

// Cache for auth settings
let authSettings: {
  mode?: string
  proxy_domain?: string | null
} | null = null

/**
 * Fetch auth settings from backend
 */
async function getAuthSettings() {
  if (authSettings) return authSettings

  try {
    const response = await originalFetch('/api/settings/auth')
    if (response.ok) {
      authSettings = await response.json()
      return authSettings
    }
  } catch {
    // ignore
  }
  return null
}

/**
 * Redirect to OAuth2 Proxy login page.
 * Uses oauth_proxy_url from backend config, with protocol-relative URL.
 *
 * Two guards keep flaky networks from nuking the page:
 * - cooldown: at most one redirect attempt per REDIRECT_COOLDOWN_MS (old builds
 *   wrote a permanent flag that never expired, masking later real logouts)
 * - probe: re-check a protected endpoint first; only a confirmed second 401
 *   redirects. Transient 401s (proxy hiccup, cookie refresh race) stay put.
 */
const REDIRECT_FLAG = 'oauth_redirecting'
const REDIRECT_COOLDOWN_MS = 30_000

async function redirectToLogin() {
  const lastAttempt = Number(sessionStorage.getItem(REDIRECT_FLAG))
  if (Number.isFinite(lastAttempt) && Date.now() - lastAttempt < REDIRECT_COOLDOWN_MS) return
  sessionStorage.setItem(REDIRECT_FLAG, String(Date.now()))

  try {
    const probe = await originalFetch('/api/config/__auth_probe', { cache: 'no-store' })
    if (probe.status !== 401) return
  } catch {
    // Probe itself failed on the network — not evidence of a logged-out session.
    return
  }

  const settings = await getAuthSettings()
  const currentUrl = window.location.href

  // plain 模式：同源自建登录页，next 只带站内路径。
  if (settings?.mode === 'plain') {
    window.location.href = `/login?next=${encodeURIComponent(
      window.location.pathname + window.location.search,
    )}`
    return
  }

  if (settings?.proxy_domain) {
    // Backend-provided oauth2 proxy domain → protocol-relative absolute URL
    const proxyUrl = settings.proxy_domain.replace(/^https?:\/\//, '')
    const loginUrl = `//${proxyUrl}/oauth2/sign_in?rd=${encodeURIComponent(currentUrl)}`
    window.location.href = loginUrl
  } else {
    // Fallback: use relative path (for same-origin proxy setup)
    const loginUrl = `/oauth2/sign_in?rd=${encodeURIComponent(currentUrl)}`
    window.location.href = loginUrl
  }
}

/**
 * Patched fetch that intercepts 401 responses.
 * - oauth2_proxy / plain: redirect to the matching login flow.
 * - mode 未启用（如直连后端 Basic 保护）：不动作，交给浏览器原生
 *   WWW-Authenticate 弹窗。
 */
window.fetch = async function patchedFetch(
  input: RequestInfo | URL,
  init?: RequestInit,
): Promise<Response> {
  const response = await originalFetch(input, init)

  if (response.status === 401) {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    // Only redirect for API requests (not for /oauth2/ itself). /api/terminal/*
    // authenticates with its own per-terminal token: a 401 there means the
    // terminal session expired, not that the user logged out.
    const isApi = url.startsWith('/api/') || url.includes('/api/')
    // Already on the login page (or logging in): don't fight the URL with
    // another redirect — the page's own fetches can 401 before login.
    if (isApi && !url.includes('/api/terminal/') && window.location.pathname !== '/login') {
      const settings = await getAuthSettings()
      if (settings?.mode === 'plain' || settings?.mode === 'oauth2_proxy') {
        redirectToLogin()
      }
      // 其它情况（含直连后端的 Basic 保护）：浏览器处理 WWW-Authenticate。
    }
  }

  return response
}

/**
 * Named export for components that prefer explicit imports.
 * This is the same patched fetch as window.fetch.
 */
export const apiFetch = window.fetch
