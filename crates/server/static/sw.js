// Perhe-elämää service worker — app shell caching for offline use.
//
// Strategy:
//   /api/*       → network-only (never cache server responses)
//   navigations  → network-first, fall back to cached index.html when offline
//   assets       → cache-first (filenames include content hashes so they're immutable)
//
// "navigations" means any HTML page load (`request.mode === 'navigate'`): the
// root `/` AND SPA routes like `/calendar` or `/recipes`, which the server also
// answers with index.html. These MUST be network-first — otherwise a reload on
// `/calendar` serves a stale cached HTML that still points at the previous
// build's hashed bundle, and the app appears frozen on an old version until a
// manual hard refresh. Treating them as cache-first "assets" was the bug.
//
// The cache is populated lazily: assets are stored as the browser fetches them,
// so the first visit online seeds the cache and subsequent offline visits are served
// from it. Bump the cache name when this strategy changes to evict stale entries.

const CACHE = 'perkele-v2';

self.addEventListener('install', event => {
  // Pre-cache the shell entry point so the app can open offline immediately.
  event.waitUntil(caches.open(CACHE).then(c => c.add('/')));
  // Take over immediately without waiting for old tabs to close.
  self.skipWaiting();
});

self.addEventListener('activate', event => {
  // Delete caches from previous versions.
  event.waitUntil(
    caches.keys().then(keys =>
      Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k)))
    )
  );
  self.clients.claim();
});

self.addEventListener('fetch', event => {
  const url = new URL(event.request.url);

  // Let API calls and the SW itself bypass the cache entirely.
  if (url.pathname.startsWith('/api/') || url.pathname === '/sw.js') return;

  // Navigations (root + SPA routes): network-first. Cache the fresh HTML for
  // offline, and when offline fall back to this route's cache or, failing that,
  // the cached shell at '/'.
  if (event.request.mode === 'navigate') {
    event.respondWith(
      caches.open(CACHE).then(cache =>
        fetch(event.request)
          .then(response => {
            if (response.ok) cache.put(event.request, response.clone());
            return response;
          })
          .catch(() =>
            cache.match(event.request).then(cached => cached || cache.match('/'))
          )
      )
    );
    return;
  }

  // Static assets: cache-first — filenames are content-hashed and immutable, so
  // a cache hit is always safe; a new build produces new names that miss and are
  // fetched fresh.
  event.respondWith(
    caches.open(CACHE).then(cache =>
      cache.match(event.request).then(cached => {
        const networkFetch = fetch(event.request).then(response => {
          if (response.ok) cache.put(event.request, response.clone());
          return response;
        });
        return cached || networkFetch;
      })
    )
  );
});

// --- Web Push (Phase 5C) ----------------------------------------------------

self.addEventListener('push', event => {
  // Payload: {"title": "...", "body": "..."} from the server's scheduler.
  const data = event.data ? event.data.json() : {};
  event.waitUntil(
    self.registration.showNotification(data.title || 'Perhe-elämää', {
      body: data.body || '',
      icon: '/icon-192.png',
    })
  );
});

// Re-subscribe when the browser rotates/expires the push subscription (iOS
// Safari does this silently). Without this the device quietly stops getting
// pushes until the user re-toggles in Minä.
self.addEventListener('pushsubscriptionchange', event => {
  event.waitUntil(
    fetch('/api/push/vapid')
      .then(r => r.json())
      .then(v => self.registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: v.public_key,
      }))
      .then(sub => {
        const j = sub.toJSON();
        return fetch('/api/push/subscribe', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            endpoint: sub.endpoint,
            p256dh: j.keys.p256dh,
            auth: j.keys.auth,
          }),
        });
      })
      .catch(() => { /* best-effort; a foreground load will reconcile */ })
  );
});

self.addEventListener('notificationclick', event => {
  event.notification.close();
  // Focus an existing app window if there is one; otherwise open the calendar.
  event.waitUntil(
    clients.matchAll({ type: 'window', includeUncontrolled: true }).then(list => {
      for (const c of list) if ('focus' in c) return c.focus();
      return clients.openWindow('/calendar');
    })
  );
});
