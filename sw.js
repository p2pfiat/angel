// Offline cache. Network first: with a connection the app always gets the newest files
// (and refreshes the cache); without one it runs from the cache. Bump CACHE with each release
// so old copies are dropped.
const CACHE = 'sync-v27'
const FILES = [
  './',
  './index.html',
  './home.js',
  './app.html',
  './app.js',
  './manifest.webmanifest',
  './icon-180.png',
  './icon-192.png',
  './icon-512.png',
]

self.addEventListener('install', (e) => {
  e.waitUntil(
    caches
      .open(CACHE)
      // Bypass the HTTP cache: hosts may mark files as long-lived.
      .then((c) => c.addAll(FILES.map((f) => new Request(f, { cache: 'reload' }))))
      .then(() => self.skipWaiting()),
  )
})

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  )
})

/** Fetch fresh (ignoring the HTTP cache) within a few seconds, else use the cached copy. */
async function networkFirst(request) {
  const cache = await caches.open(CACHE)
  try {
    const fresh = await Promise.race([
      fetch(request, { cache: 'no-cache' }),
      new Promise((_, reject) => setTimeout(() => reject(new Error('slow')), 4000)),
    ])
    if (fresh.ok) cache.put(request, fresh.clone())
    return fresh
  } catch {
    const cached = await cache.match(request, { ignoreSearch: true })
    if (cached) return cached
    throw new Error('offline and not cached')
  }
}

// An alarm for an angel (Web Push from a watcher). Shown even when the app is closed.
self.addEventListener('push', (e) => {
  let d = {}
  try {
    d = e.data ? e.data.json() : {}
  } catch { }
  const urgent = d.kind === 'duress' || d.kind === 'missed'
  const tag = d.id || 'alarm'
  // The same alarm can come from two watchers: the second one replaces the first quietly
  // (a push must always show something, so it is shown again, not dropped).
  e.waitUntil(
    self.registration
      .getNotifications({ tag })
      .catch(() => [])
      .then((open) =>
        self.registration.showNotification(d.title || 'Alarm', {
          body: d.body || '',
          tag,
          renotify: !open.length,
          silent: open.length > 0,
          requireInteraction: urgent,
          data: { id: d.id || '' },
        }),
      ),
  )
})

// Tapping it opens the angel screen.
self.addEventListener('notificationclick', (e) => {
  e.notification.close()
  const url = new URL('./app.html#angel', self.registration.scope).href
  e.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((ws) => {
      for (const w of ws) {
        if ('focus' in w) {
          w.postMessage({ open: 'angel' })
          return w.focus()
        }
      }
      return self.clients.openWindow(url)
    }),
  )
})

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url)
  if (e.request.method !== 'GET' || url.origin !== self.location.origin) return
  e.respondWith(networkFirst(e.request))
})
