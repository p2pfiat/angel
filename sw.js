// Offline cache. Network first: with a connection the app always gets the newest files
// (and refreshes the cache); without one it runs from the cache. Bump CACHE with each release
// so old copies are dropped.
const CACHE = 'sync-v10'
const FILES = ['./', './index.html', './app.js', './manifest.webmanifest']

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

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url)
  if (e.request.method !== 'GET' || url.origin !== self.location.origin) return
  e.respondWith(networkFirst(e.request))
})
