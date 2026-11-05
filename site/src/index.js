// glassdesk.dev: landing page, one-line installer, release downloads (counted in D1 and
// sent to Google Analytics), the feedback API (emailed to the maintainer), the introducer
// for remote access (WebRTC signaling, one Durable Object per host), and passkey accounts
// for /computers (src/accounts.js). Static files come from ./public via the ASSETS binding;
// the web client for remote access is served from /app/.

import { accounts } from './accounts.js';

import { hostsOnline } from './introducer.js';

export { Introducer } from './introducer.js';

const FEEDBACK_MAX = 4000;
const FEEDBACK_PER_HOUR = 5;
// Quick support: how long a code waits, how many a computer may make, and how many wrong
// guesses a browser gets, per hour.
const SUPPORT_TTL_MS = 3600_000;
const SUPPORT_CODES_PER_HOUR = 20;
const SUPPORT_MISSES_PER_HOUR = 20;
// Command-line fetchers get the install script at the bare domain (`curl glassdesk.dev | sh`).
const CLI_AGENT = /^(curl|wget|fetch|httpie|python-requests|go-http-client)\b/i;
// Invoke-RestMethod / Invoke-WebRequest: `irm glassdesk.dev | iex` gets the Windows installer.
const POWERSHELL = /\b(Windows)?PowerShell\/\d/i;

// Agent discovery on the home page: Link relations, and a markdown twin for Accept: text/markdown.
const HOME_LINKS = [
  '<https://glassdesk.dev/>; rel="canonical"',
  '</index.md>; rel="alternate"; type="text/markdown"',
  '</sitemap.xml>; rel="sitemap"; type="application/xml"',
  '</llms.txt>; rel="describedby"; type="text/plain"',
  '</docs>; rel="help"',
].join(', ');
const HSTS = 'max-age=31536000; includeSubDomains';

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const ua = request.headers.get('user-agent') ?? '';

    // HTTPS only (`curl -fsSL glassdesk.dev` follows the redirect).
    // CF-Visitor carries the client's scheme at the edge (local dev has none).
    if (/"scheme":"http"/.test(request.headers.get('cf-visitor') ?? '')) {
      url.protocol = 'https:';
      return Response.redirect(url.toString(), 301);
    }

    if (url.pathname === '/api/feedback') {
      if (request.method === 'OPTIONS') return cors(new Response(null, { status: 204 }));
      if (request.method === 'POST') return cors(await feedback(request, env, ctx));
      return cors(json({ error: 'method not allowed' }, 405));
    }

    const account = await accounts(request, env, url);
    if (account) return account;

    if (url.pathname === '/api/stats') return stats(request, env);
    // Your own visitor id, so `node stats.mjs --mark-me` can leave this machine out of the numbers.
    if (url.pathname === '/api/visitor') return json({ visitor: await visitorId(request, env) });

    if (url.pathname === '/api/support') {
      if (request.method === 'POST') return supportRegister(request, env, url);
      if (request.method === 'DELETE') return supportCancel(request, env);
      return json({ error: 'method not allowed' }, 405);
    }
    if (url.pathname === '/api/support/claim' && request.method === 'POST') return supportClaim(request, env);

    // /signal/online?ids=<id>,…: which of these computers are online (glassdesk.dev/computers).
    if (url.pathname === '/signal/online') {
      const ids = [...new Set((url.searchParams.get('ids') ?? '').split(','))].filter((id) => /^[A-Za-z0-9_-]{22}$/.test(id)).slice(0, 32);
      return json({ online: await hostsOnline(env, ids) });
    }

    // /signal/host/<id> (the host) and /signal/connect/<id> (a browser).
    const signal = url.pathname.match(/^\/signal\/(host|connect)\/([A-Za-z0-9_-]{22})$/);
    if (signal) {
      const [, kind, id] = signal;
      const target = new URL(`https://introducer/?role=${kind === 'host' ? 'host' : 'client'}&id=${id}`);
      const headers = new Headers(request.headers);
      headers.set('X-Client-IP', request.headers.get('CF-Connecting-IP') ?? '');
      return env.INTRODUCER.get(env.INTRODUCER.idFromName(id)).fetch(new Request(target, { headers }));
    }

    // An agent asking for markdown gets the page's markdown twin, even from curl; `curl … | sh` never asks.
    const wantsMarkdown = /\btext\/markdown\b/i.test(request.headers.get('accept') ?? '');
    // Quick support on Windows: `irm glassdesk.dev/help | iex`.
    if (url.pathname === '/help.ps1' || (url.pathname === '/help' && POWERSHELL.test(ua) && !wantsMarkdown)) {
      ctx.waitUntil(recordDownload(env, request, 'script', 'help.ps1', 'quick_support'));
      const res = await env.ASSETS.fetch(new URL('/help.ps1', url));
      return new Response(res.body, {
        headers: { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store', 'strict-transport-security': HSTS },
      });
    }
    // Quick support: `curl glassdesk.dev/help | sh` runs Glass for one session, installing nothing.
    if (url.pathname === '/help.sh' || (url.pathname === '/help' && CLI_AGENT.test(ua) && !wantsMarkdown)) {
      ctx.waitUntil(recordDownload(env, request, 'script', 'help.sh', 'quick_support'));
      const res = await env.ASSETS.fetch(new URL('/help.sh', url));
      return new Response(res.body, {
        headers: { 'content-type': 'text/x-shellscript; charset=utf-8', 'cache-control': 'no-store', 'strict-transport-security': HSTS },
      });
    }
    if (url.pathname === '/install.ps1' || (url.pathname === '/' && POWERSHELL.test(ua) && !wantsMarkdown)) {
      ctx.waitUntil(recordDownload(env, request, 'script', 'install.ps1', 'install_script'));
      const res = await env.ASSETS.fetch(new URL('/install.ps1', url));
      return new Response(res.body, {
        headers: { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store', 'strict-transport-security': HSTS },
      });
    }
    const wantsScript = url.pathname === '/install.sh' || (url.pathname === '/' && CLI_AGENT.test(ua) && !wantsMarkdown);
    if (wantsScript) {
      ctx.waitUntil(recordDownload(env, request, 'script', 'install.sh', 'install_script'));
      const res = await env.ASSETS.fetch(new URL('/install.sh', url));
      return new Response(res.body, {
        headers: { 'content-type': 'text/x-shellscript; charset=utf-8', 'cache-control': 'no-store', 'strict-transport-security': HSTS },
      });
    }

    // The web client for remote access lives at /app/, with the slash, so its relative URLs
    // resolve under /app/. drop-trailing-slash would redirect it to /app and break them.
    if (url.pathname === '/app') {
      url.pathname = '/app/';
      return Response.redirect(url.toString(), 301);
    }
    if (url.pathname === '/app/') return env.ASSETS.fetch(new Request(new URL('/app', url), request));

    if (url.pathname === '/' && (request.method === 'GET' || request.method === 'HEAD')) {
      const res = await env.ASSETS.fetch(wantsMarkdown ? new Request(new URL('/index.md', url), request) : request);
      const out = new Response(res.body, res);
      if (wantsMarkdown) out.headers.set('content-type', 'text/markdown; charset=utf-8');
      out.headers.append('vary', 'Accept');
      out.headers.set('link', HOME_LINKS);
      out.headers.set('strict-transport-security', HSTS);
      return out;
    }

    if (url.pathname.endsWith('.mp4')) return ranged(await env.ASSETS.fetch(request), request);

    if (url.pathname.startsWith('/releases/') && /\.(tar\.gz|zip|msi)$/.test(url.pathname)) {
      const name = url.pathname.slice('/releases/'.length);
      ctx.waitUntil(recordDownload(env, request, 'release', name, 'download'));
      const res = await env.ASSETS.fetch(request);
      if (res.status !== 404) return res;
      // Too big to be an asset (over 25 MiB): the same file from the GitHub release.
      return releaseRedirect(env, url, name);
    }
    return env.ASSETS.fetch(request);
  },
};

/** A tarball, zip or MSI kept only on GitHub: `glass-latest-*` means the version in version.json. */
export async function releaseRedirect(env, url, name) {
  let file = name;
  if (file.startsWith('glass-latest-')) {
    const { version } = await (await env.ASSETS.fetch(new URL('/version.json', url))).json();
    file = file.replace('glass-latest-', `glass-${version}-`);
  }
  const version = file.match(/^glass-(\d+\.\d+\.\d+)-[\w.-]+\.(tar\.gz|zip|msi)$/)?.[1];
  if (!version) return new Response('Not found', { status: 404 });
  return Response.redirect(`https://github.com/Ntarasiuk/glass/releases/download/v${version}/${file}`, 302);
}

/** Answers `Range: bytes=…` from a whole (small) asset: static assets ignore Range. */
export async function ranged(res, request) {
  const range = request.headers.get('range');
  const headers = new Headers(res.headers);
  headers.set('accept-ranges', 'bytes');
  if (!res.ok || !range) return new Response(res.body, { status: res.status, headers });
  const body = new Uint8Array(await res.arrayBuffer());
  const size = body.length;
  const m = /^bytes=(\d*)-(\d*)$/.exec(range.trim());
  let start = m && m[1] !== '' ? Number(m[1]) : NaN;
  let end = m && m[2] !== '' ? Number(m[2]) : size - 1;
  if (m && m[1] === '' && m[2] !== '') {
    start = Math.max(0, size - Number(m[2])); // the last N bytes
    end = size - 1;
  }
  if (!m || !(start >= 0) || start >= size || end < start) {
    headers.set('content-range', `bytes */${size}`);
    return new Response(null, { status: 416, headers });
  }
  end = Math.min(end, size - 1);
  headers.set('content-range', `bytes ${start}-${end}/${size}`);
  headers.set('content-length', String(end - start + 1));
  return new Response(request.method === 'HEAD' ? null : body.slice(start, end + 1), { status: 206, headers });
}

async function feedback(request, env, ctx) {
  let body;
  try {
    body = await request.json();
  } catch {
    return json({ error: 'expected JSON' }, 400);
  }
  const message = String(body.message ?? '').trim();
  if (!message) return json({ error: 'message is required' }, 400);
  if (message.length > FEEDBACK_MAX) return json({ error: `message is over ${FEEDBACK_MAX} characters` }, 400);
  const email = clip(body.email, 200);
  if (email && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) return json({ error: 'that email looks off' }, 400);

  const visitor = await visitorId(request, env);
  const hourAgo = Date.now() - 3600_000;
  const recent = await env.DB.prepare('SELECT COUNT(*) AS n FROM feedback WHERE visitor = ? AND ts > ?')
    .bind(visitor, hourAgo)
    .first('n');
  if (recent >= FEEDBACK_PER_HOUR) return json({ error: 'thanks! that is plenty for now, try again later' }, 429);

  const row = {
    message,
    email,
    source: clip(body.source, 20) ?? 'web',
    version: clip(body.version, 40),
    os: clip(body.os, 80),
    country: request.cf?.country ?? null,
  };
  await env.DB.prepare(
    'INSERT INTO feedback (ts, message, email, source, version, os, country, visitor) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
  )
    .bind(Date.now(), row.message, row.email, row.source, row.version, row.os, row.country, visitor)
    .run();
  ctx.waitUntil(Promise.all([emailFeedback(env, row), track(env, request, visitor, 'feedback', { source: row.source })]));
  return json({ ok: true });
}

/** Each piece of feedback, emailed to FEEDBACK_TO (a secret, so the address isn't in the repo). */
async function emailFeedback(env, f) {
  if (!env.EMAIL || !env.FEEDBACK_TO) return;
  const about = [f.source, f.version && `Glass ${f.version}`, f.os, f.country].filter(Boolean).join(' · ');
  const text = `${f.message}\n\n${about}${f.email ? `\nFrom: ${f.email}` : ''}`;
  const esc = (v) => String(v).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
  try {
    await env.EMAIL.send({
      to: env.FEEDBACK_TO,
      from: { email: 'feedback@glassdesk.dev', name: 'Glass feedback' },
      ...(f.email ? { replyTo: f.email } : {}),
      subject: `Glass feedback: ${f.message.replace(/\s+/g, ' ').slice(0, 60)}`,
      text,
      html: `<p style="white-space:pre-wrap">${esc(f.message)}</p><p style="color:#666">${esc(about)}${f.email ? `<br>From: ${esc(f.email)}` : ''}</p>`,
    });
  } catch (e) {
    console.error('emailFeedback failed', e.code, e.message);
  }
}

// ---- Quick support ----

/** `glass support` registers its one-time link and gets a code for the helper to type. */
async function supportRegister(request, env, url) {
  let body;
  try {
    body = await request.json();
  } catch {
    return json({ error: 'expected JSON' }, 400);
  }
  // Only links to this site's own viewer, so a code can never send anyone elsewhere.
  // (Local dev, over plain HTTP, also takes localhost: wrangler dev reports the route's host.)
  const link = String(body.url ?? '');
  const m = link.match(/^(https?:\/\/[^/]+)\/app\/#r=[A-Za-z0-9_-]{40,120}&l=[0-9a-f]{32}$/);
  const local = url.protocol === 'http:' && /^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(m?.[1] ?? '');
  if (!m || (m[1] !== url.origin && !local)) return json({ error: 'not a Glass link' }, 400);
  const now = Date.now();
  const visitor = await visitorId(request, env);
  const recent = await env.DB.prepare('SELECT COUNT(*) AS n FROM support_codes WHERE visitor = ? AND created > ?')
    .bind(visitor, now - 3600_000)
    .first('n');
  if (recent >= SUPPORT_CODES_PER_HOUR) return json({ error: 'too many codes; try again later' }, 429);
  await env.DB.prepare('DELETE FROM support_codes WHERE expires < ?').bind(now).run();
  const cancel = randomToken();
  for (let i = 0; i < 5; i++) {
    const code = randomCode();
    const res = await env.DB.prepare(
      'INSERT OR IGNORE INTO support_codes (code_hash, url, cancel_hash, created, expires, visitor) VALUES (?, ?, ?, ?, ?, ?)',
    )
      .bind(await secretHash(env, `code:${code}`), link, await secretHash(env, `cancel:${cancel}`), now, now + SUPPORT_TTL_MS, visitor)
      .run();
    if (res.meta.changes) return json({ code, cancel, expires: Math.floor((now + SUPPORT_TTL_MS) / 1000) });
  }
  return json({ error: 'no free code; try again' }, 503);
}

/** The helper typed a code at /help: its link, once. */
async function supportClaim(request, env) {
  let body;
  try {
    body = await request.json();
  } catch {
    return json({ error: 'expected JSON' }, 400);
  }
  const now = Date.now();
  const visitor = await visitorId(request, env);
  const misses = await env.DB.prepare('SELECT COUNT(*) AS n FROM support_misses WHERE visitor = ? AND ts > ?')
    .bind(visitor, now - 3600_000)
    .first('n');
  if (misses >= SUPPORT_MISSES_PER_HOUR) return json({ error: 'Too many wrong codes. Try again in an hour.' }, 429);
  const code = String(body.code ?? '').replace(/\D/g, '');
  const hash = code.length === 9 ? await secretHash(env, `code:${code}`) : '';
  // Taken in one step: a code works once, even if two browsers race for it.
  const row = hash
    ? await env.DB.prepare('DELETE FROM support_codes WHERE code_hash = ? AND expires > ? RETURNING url').bind(hash, now).first()
    : null;
  if (!row) {
    await env.DB.prepare('INSERT INTO support_misses (visitor, ts) VALUES (?, ?)').bind(visitor, now).run();
    return json({ error: "That code didn't work. Check it with them, or ask them for a new one." }, 404);
  }
  return json({ url: row.url });
}

/** `glass support` stopped: its code goes at once (header x-cancel: the token it got). */
async function supportCancel(request, env) {
  const cancel = request.headers.get('x-cancel') ?? '';
  if (!cancel) return json({ error: 'missing x-cancel' }, 400);
  await env.DB.prepare('DELETE FROM support_codes WHERE cancel_hash = ?').bind(await secretHash(env, `cancel:${cancel}`)).run();
  return json({ ok: true });
}

/** Nine random digits (leading zeros allowed), without modulo bias. */
function randomCode() {
  const buf = new Uint32Array(1);
  for (;;) {
    crypto.getRandomValues(buf);
    // Below 4e9, every residue mod 1e9 is equally likely.
    if (buf[0] < 4_000_000_000) return String(buf[0] % 1_000_000_000).padStart(9, '0');
  }
}

function randomToken() {
  const b = new Uint8Array(24);
  crypto.getRandomValues(b);
  return btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** Salted SHA-256: a leaked table doesn't give the codes back (there are only 10⁹). */
async function secretHash(env, value) {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(`${env.VISITOR_SALT ?? ''}:support:${value}`));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

async function recordDownload(env, request, kind, file, event) {
  try {
    const visitor = await visitorId(request, env);
    // The installer sends the version it replaces ('none' for a new install); older
    // installers and direct downloads send nothing.
    const said = request.headers.get('x-glass-previous');
    const previous = said && said !== 'none' && /^[\w.+-]{1,40}$/.test(said) ? said : null;
    const via = said === 'none' ? 'install' : previous ? 'update' : null;
    await env.DB.prepare(
      'INSERT INTO downloads (ts, kind, file, country, user_agent, visitor, via, previous) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
    )
      .bind(Date.now(), kind, file, request.cf?.country ?? null, clip(request.headers.get('user-agent'), 200), visitor, via, previous)
      .run();
    const os = /darwin/.test(file) ? 'macos' : /linux/.test(file) ? 'linux' : undefined;
    await track(env, request, visitor, via === 'update' ? 'update' : event, { file, os, via: via ?? 'unknown', previous });
  } catch (e) {
    console.error('recordDownload failed', e);
  }
}

/**
 * The same event in Google Analytics (Measurement Protocol), next to the site's page views.
 * Downloads come from curl, which never runs the page's tag. Our own machines are left out.
 */
async function track(env, request, visitor, name, params) {
  if (!env.GA_MEASUREMENT_ID || !env.GA_API_SECRET) return;
  try {
    if (await env.DB.prepare('SELECT 1 FROM internal_visitors WHERE visitor = ?').bind(visitor).first()) return;
    const clean = Object.fromEntries(Object.entries(params).filter(([, v]) => v !== undefined && v !== null));
    const country = request.cf?.country;
    const res = await fetch(
      `https://www.google-analytics.com/mp/collect?measurement_id=${env.GA_MEASUREMENT_ID}&api_secret=${env.GA_API_SECRET}`,
      {
        method: 'POST',
        body: JSON.stringify({
          client_id: visitor,
          ...(country ? { user_location: { country_id: country } } : {}),
          // A session per visitor per half hour, so the events show up in GA's reports.
          events: [{ name, params: { ...clean, session_id: String(Math.floor(Date.now() / 1800_000)), engagement_time_msec: 1 } }],
        }),
      },
    );
    if (!res.ok) console.error('track failed', res.status);
  } catch (e) {
    console.error('track failed', e);
  }
}

/** Totals for the last 30 days, without our own machines. Requires `Authorization: Bearer <STATS_KEY>`. */
async function stats(request, env) {
  const auth = request.headers.get('authorization') ?? '';
  if (!env.STATS_KEY || auth !== `Bearer ${env.STATS_KEY}`) return json({ error: 'unauthorized' }, 401);
  const since = Date.now() - 30 * 86400_000;
  const [downloads, feedbackRows] = await Promise.all([
    env.DB.prepare(
      `SELECT kind, via, COUNT(*) AS total, COUNT(DISTINCT visitor) AS unique_visitors FROM downloads
       WHERE ts > ? AND visitor NOT IN (SELECT visitor FROM internal_visitors) GROUP BY kind, via`,
    )
      .bind(since)
      .all(),
    env.DB.prepare('SELECT ts, source, version, message, email FROM feedback ORDER BY ts DESC LIMIT 50').all(),
  ]);
  return json({ downloads: downloads.results, feedback: feedbackRows.results });
}

/**
 * Salted SHA-256 of the client IP: lets us count unique visitors without storing IPs. IPv6
 * counts by its /64 (one household or office), since devices change the rest every day.
 */
export async function visitorId(request, env) {
  const ip = network(request.headers.get('cf-connecting-ip') ?? '');
  const data = new TextEncoder().encode(`${env.VISITOR_SALT ?? ''}:${ip}`);
  const digest = await crypto.subtle.digest('SHA-256', data);
  return [...new Uint8Array(digest).slice(0, 12)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** An IPv6 address's /64 ("2001:0db8:0001:0002::/64"); IPv4 as it is. */
export function network(ip) {
  if (!ip.includes(':')) return ip;
  const [head, tail] = ip.split('::');
  const h = head ? head.split(':') : [];
  const t = tail ? tail.split(':') : [];
  const groups = tail === undefined ? h : [...h, ...Array(Math.max(0, 8 - h.length - t.length)).fill('0'), ...t];
  return `${groups.slice(0, 4).map((g) => g.toLowerCase().padStart(4, '0')).join(':')}::/64`;
}

function clip(value, max) {
  if (value === undefined || value === null) return null;
  const s = String(value).trim();
  return s ? s.slice(0, max) : null;
}

function json(data, status = 200) {
  return new Response(JSON.stringify(data), { status, headers: { 'content-type': 'application/json' } });
}

// The in-app feedback button posts from the user's own Glass host origin.
function cors(response) {
  const res = new Response(response.body, response);
  res.headers.set('access-control-allow-origin', '*');
  res.headers.set('access-control-allow-methods', 'POST, OPTIONS');
  res.headers.set('access-control-allow-headers', 'content-type');
  return res;
}
