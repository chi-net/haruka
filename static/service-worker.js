const STATIC_CACHE = 'haruka-static-2026-09-01-v4';
const STATIC_PATHS = new Set([
  '/static/app.css',
  '/static/vendor/htmx.min.js',
  '/static/vendor/chart.umd.min.js',
  '/static/receipt-scanner.js',
  '/static/ocr/tesseract.min.js',
  '/static/ocr/worker.min.js'
]);

self.addEventListener('install', event => {
  event.waitUntil(caches.open(STATIC_CACHE).then(cache => cache.addAll([...STATIC_PATHS])));
  self.skipWaiting();
});

self.addEventListener('activate', event => {
  event.waitUntil(
    caches.keys().then(keys => Promise.all(keys.filter(key => key.startsWith('haruka-static-') && key !== STATIC_CACHE).map(key => caches.delete(key))))
  );
  self.clients.claim();
});

function offlineJson() {
  return new Response(JSON.stringify({ ok: false, status: 503, error: '当前处于离线状态，账务操作没有提交。' }), {
    status: 503,
    headers: { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' }
  });
}

self.addEventListener('fetch', event => {
  const request = event.request;
  const url = new URL(request.url);
  const acceptsJson = (request.headers.get('Accept') || '').includes('application/json');
  const isHtmx = (request.headers.get('HX-Request') || '').toLowerCase() === 'true';

  // AJAX 与所有写操作只走网络。离线时返回统一 JSON 错误，绝不排队重放账务请求。
  if (request.method !== 'GET' || acceptsJson || isHtmx) {
    event.respondWith(fetch(request).catch(offlineJson));
    return;
  }

  if (url.origin === self.location.origin && STATIC_PATHS.has(url.pathname)) {
    event.respondWith(
      caches.match(request).then(cached => cached || fetch(request).then(response => {
        if (response.ok) caches.open(STATIC_CACHE).then(cache => cache.put(request, response.clone()));
        return response;
      }))
    );
    return;
  }

});
