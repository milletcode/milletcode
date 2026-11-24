// Attention service worker: exists solely to show notifications on browsers
// that don't support the page-context Notification constructor (Android
// Chrome and friends) and to route notification clicks back into the SPA.
// It registers NO fetch handler and caches nothing — dev-server and dist
// traffic pass through untouched.
self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const path = (event.notification.data && event.notification.data.path) || '/'
  event.waitUntil(
    (async () => {
      const clientList = await self.clients.matchAll({ type: 'window', includeUncontrolled: true })
      for (const client of clientList) {
        await client.focus()
        client.postMessage({ type: 'navigate', path })
        return
      }
      await self.clients.openWindow(path)
    })()
  )
})
