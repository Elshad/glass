// Service worker: notifications (Web Push) while Glass is closed, their Allow / Deny
// buttons, the home-screen badge, things shared to Glass from other apps, and a copy of
// the client so Glass still opens when this computer can't be reached (asleep, off, or
// off the tailnet) and can say so. The network always comes first: a reachable host
// serves every load, so the copy is never stale while it matters.

const DB = 'glass';
const STORE = 'kv';
const SHARE_CACHE = 'glass-share';
const SHELL_CACHE = 'glass-shell';
// Waiting longer than this for the host means it isn't coming: open from the copy.
const NAVIGATE_TIMEOUT_MS = 4000;

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

// ---- A tiny key-value store (the page writes the action key here) ----

function db() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
async function get(key) {
  const d = await db();
  return new Promise((resolve) => {
    const req = d.transaction(STORE).objectStore(STORE).get(key);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => resolve(undefined);
  });
}

// ---- Notifications ----

/** A window of this viewer the person is looking at right now. */
async function focusedClient() {
  const viewer = new URL(self.registration.scope).pathname;
  const list = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  return list.find((c) => new URL(c.url).pathname === viewer && c.visibilityState === 'visible' && c.focused);
}

self.addEventListener('push', (e) => {
  e.waitUntil((async () => {
    let msg;
    try { msg = e.data?.json(); } catch {}
    const n = msg?.notification;
    if (!n) return;
    if (typeof msg.app_badge === 'number') await self.navigator.setAppBadge?.(msg.app_badge).catch(() => {});
    // Already looking at Glass: the viewer shows it (chat, the agent's question).
    if (await focusedClient()) return;
    const data = { ...(n.data ?? {}), navigate: n.navigate };
    const answer = data.answer;
    const key = answer ? await get('pushKey') : undefined;
    await self.registration.showNotification(n.title, {
      body: n.body,
      tag: n.tag,
      renotify: Boolean(n.tag),
      icon: 'icon-192.png',
      badge: 'badge.png',
      data,
      // Answering from the notification needs this device's key (owners only).
      actions: answer && key ? [{ action: 'yes', title: answer.yes }, { action: 'no', title: answer.no }] : [],
      requireInteraction: data.kind === 'ask',
    });
  })());
});

self.addEventListener('notificationclick', (e) => {
  const { data } = e.notification;
  e.notification.close();
  e.waitUntil((async () => {
    if (e.action === 'yes' || e.action === 'no') {
      const key = await get('pushKey');
      const r = await fetch('push/act', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ key, what: data.answer.what, id: data.answer.id, yes: e.action === 'yes' }),
      }).catch(() => undefined);
      if (r?.ok) return;
      // Couldn't answer from here (offline, or the question is gone): open Glass instead.
    }
    await openViewer(data?.navigate);
  })());
});

/** Brings back an open viewer, or opens one. */
async function openViewer(url) {
  const viewer = new URL(self.registration.scope).pathname;
  const list = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const existing = list.find((c) => new URL(c.url).pathname === viewer);
  if (existing) {
    await existing.focus().catch(() => {});
    return;
  }
  await self.clients.openWindow(url || self.registration.scope);
}

// The browser replaced the subscription: the page subscribes again next time it opens.
self.addEventListener('pushsubscriptionchange', () => {});

// ---- Share target: files, text and links shared to Glass from another app ----

// ---- The client, for when the host can't be reached ----

/** The page's own files (not the host's APIs, consoles or forwarded ports). */
function isShell(url) {
  if (url.origin !== self.location.origin) return false;
  const scope = new URL(self.registration.scope).pathname;
  if (!url.pathname.startsWith(scope)) return false;
  const path = url.pathname.slice(scope.length);
  return /^(|index\.html|[\w.-]+\.(js|png|svg|webmanifest)|config\.json|machines\.json|vendor\/[\w./-]+)$/.test(path);
}

async function networkFirst(request, timeoutMs) {
  const cache = await caches.open(SHELL_CACHE);
  // Navigations with a query (shortcuts, shares) are the same page.
  const key = request.mode === 'navigate' ? new URL('./', self.registration.scope).href : request.url.replace(/[?#].*$/, '');
  // Always ask the server (a cheap 304 when nothing changed): a proxy in front, such as a
  // Cloudflare zone's "Browser Cache TTL", can stretch max-age=0 into hours, and the
  // browser would keep running an old client that long. (A navigation can't take options.)
  const get = () => (request.mode === 'navigate' ? fetch(request) : fetch(request, { cache: 'no-cache' }));
  try {
    const response = await (timeoutMs
      ? Promise.race([get(), new Promise((_, reject) => setTimeout(() => reject(new Error('timeout')), timeoutMs))])
      : get());
    if (response.ok) cache.put(key, response.clone()).catch(() => {});
    return response;
  } catch (e) {
    const cached = await cache.match(key);
    if (cached) return cached;
    throw e;
  }
}

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method === 'GET' && isShell(url)) {
    e.respondWith(networkFirst(e.request, e.request.mode === 'navigate' ? NAVIGATE_TIMEOUT_MS : 0));
    return;
  }
  if (e.request.method !== 'POST' || !url.pathname.endsWith('/share')) return;
  e.respondWith((async () => {
    const form = await e.request.formData();
    const cache = await caches.open(SHARE_CACHE);
    const files = form.getAll('files').filter((f) => f instanceof File && f.size);
    let i = 0;
    for (const f of files) {
      await cache.put(`shared/file/${i++}`, new Response(f, { headers: { 'x-name': encodeURIComponent(f.name), 'content-type': f.type || 'application/octet-stream' } }));
    }
    const text = [form.get('title'), form.get('text'), form.get('url')].filter(Boolean).join('\n').trim();
    if (text) await cache.put('shared/text', new Response(text));
    return Response.redirect(new URL('./?shared=1', self.registration.scope).href, 303);
  })());
});
