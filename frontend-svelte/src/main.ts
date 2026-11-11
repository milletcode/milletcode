import './app.css'
import './lib/i18n'
import './lib/api' // 导入全局 401 拦截器
import { isLoading } from 'svelte-i18n'
import App from './App.svelte'
import { mount } from 'svelte'

// Wait for i18n to finish loading before mounting the app
function waitForI18n(): Promise<void> {
  return new Promise((resolve) => {
    const unsub = isLoading.subscribe((loading) => {
      if (!loading) {
        unsub()
        resolve()
      }
    })
  })
}

waitForI18n().then(() => {
  mount(App, {
    target: document.getElementById('app')!,
  })
})
