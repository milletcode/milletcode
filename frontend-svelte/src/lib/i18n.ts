import { register, init, getLocaleFromNavigator } from 'svelte-i18n'

register('zh-CN', () => import('../locales/zh-CN.json'))
register('en', () => import('../locales/en.json'))

init({
  fallbackLocale: 'zh-CN',
  initialLocale: localStorage.getItem('lang') || getLocaleFromNavigator() || 'zh-CN',
})
