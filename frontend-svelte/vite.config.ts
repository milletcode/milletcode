import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import type { IncomingMessage, ServerResponse } from 'http'
import { fileURLToPath } from 'url'

const __dirname = fileURLToPath(new URL('.', import.meta.url))

// Proxy configure callback: handle multipart uploads properly
function proxyConfigure(proxy: any) {
  proxy.on('proxyReq', (proxyReq: any, req: any) => {
    const contentType = req.headers['content-type'] || ''
    if (contentType.includes('multipart/form-data')) {
      // Remove Content-Length to allow streaming
      proxyReq.removeHeader('Content-Length')
    }
  })
}

// plain 模式认证网关（dev 5180 与生产同构）：SPA 壳不能匿名看。
// /api、/login、/changelog、vite 内部路径放行（/api 由 proxy 打到后端做真校验），
// 其余每个请求拿原始 Cookie 去 backend probe——401 一律 302 到 /login。
// /changelog 与后端 PUBLIC_PATHS 对齐：只有独立密码门，不参与登录体系，
// 由 proxy 转给后端出独立页（匿名下 SPA 加载不出来）。
function plainAuthGate() {
  const BACKEND = 'http://127.0.0.1:3030'
  const PUBLIC_PREFIXES = ['/api/', '/login', '/changelog', '/ws', '/@vite', '/@fs', '/@id', '/node_modules/', '/__vite_hmr']
  return {
    name: 'plain-auth-gate',
    configureServer(server: any) {
      server.middlewares.use((req: IncomingMessage, res: ServerResponse, next: () => void) => {
        const url = (req.url || '').split('?')[0]
        if (PUBLIC_PREFIXES.some((p) => url === p || url.startsWith(p))) return next()
        fetch(`${BACKEND}/api/config/__auth_probe`, {
          headers: { cookie: req.headers.cookie || '' },
        })
          .then((r) => {
            if (r.status === 401) {
              res.statusCode = 302
              res.setHeader('Location', `/login?next=${encodeURIComponent(req.url || '/')}`)
              res.end()
            } else {
              next()
            }
          })
          .catch(() => {
            res.statusCode = 502
            res.end('backend unavailable')
          })
      })
    },
  }
}

export default defineConfig({
  plugins: [plainAuthGate(), svelte()],
  resolve: {
    alias: {
      $lib: '/src/lib',
    },
  },
  server: {
    host: '0.0.0.0',
    port: 5180,
    allowedHosts: true,
    fs: {
      allow: [__dirname, '/tmp/milletcode-uploads'],
    },
    hmr: {
      path: '/__vite_hmr',
      timeout: 60000,
    },
    proxy: {
      '/api': {
        target: 'http://0.0.0.0:3030',
        changeOrigin: true,
        ws: true,
        xfwd: true,
        headers: { 'x-millet-edge': 'vite' },
        configure: proxyConfigure,
      },
      // plain 模式登录页由后端提供（自包含，独立于 SPA bundle）
      '/login': {
        target: 'http://127.0.0.1:3030',
        changeOrigin: true,
      },
      // 更新日志独立页同理：由后端提供（自带密码门，匿名可访问）
      '/changelog': {
        target: 'http://127.0.0.1:3030',
        changeOrigin: true,
      },
      '/ws': {
        target: 'http://127.0.0.1:3030',
        ws: true,
        changeOrigin: true,
        configure: proxyConfigure,
      },
      // oauth2_proxy 模式 dev 代理：指向你自己的 oauth2-proxy 实例（本机默认端口 4180）
      '/oauth2': {
        target: 'http://127.0.0.1:4180',
        changeOrigin: true,
      },
    },
  },
})
