// Runs the getglass Worker locally (wrangler dev, fresh local D1) and checks every route.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const SITE = resolve(import.meta.dirname, '../../site');
const PORT = 8790;
const BASE = `http://localhost:${PORT}`;
const KEY = 'test-stats-key';
let wrangler;

before(async () => {
  const persist = mkdtempSync(join(tmpdir(), 'glass-site-'));
  const run = (args) => execFileSync('npx', ['wrangler', ...args, '--persist-to', persist], { cwd: SITE, stdio: 'pipe' });
  execFileSync('node', ['build.mjs'], { cwd: SITE, stdio: 'pipe' });
  run(['d1', 'migrations', 'apply', 'getglass', '--local']);
  wrangler = spawn(
    'npx',
    ['wrangler', 'dev', '--port', String(PORT), '--persist-to', persist, '--var', `STATS_KEY:${KEY}`, '--var', 'VISITOR_SALT:test'],
    { cwd: SITE, stdio: 'ignore', detached: true },
  );
  for (let i = 0; i < 120; i++) {
    try {
      if ((await fetch(`${BASE}/llms.txt`)).ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error('wrangler dev did not start');
});

after(() => process.kill(-wrangler.pid));

const get = (path, ua) => fetch(`${BASE}${path}`, { headers: ua ? { 'user-agent': ua } : {}, redirect: 'manual' });
const feedback = (body) =>
  fetch(`${BASE}/api/feedback`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
const statsFor = async () => (await fetch(`${BASE}/api/stats`, { headers: { authorization: `Bearer ${KEY}` } })).json();

test('command-line fetchers get the install script at /', async () => {
  for (const ua of ['curl/8.7.1', 'Wget/1.21']) {
    const res = await get('/', ua);
    assert.equal(res.status, 200);
    assert.match(res.headers.get('content-type'), /shellscript/);
    assert.match(await res.text(), /^#!\/bin\/sh/);
  }
});

test('browsers get the landing page with structured data', async () => {
  const html = await (await get('/', 'Mozilla/5.0 (Macintosh) Safari/605.1.15')).text();
  assert.match(html, /<title>Glass/);
  assert.match(html, /"@type": "SoftwareApplication"/);
  assert.match(html, /curl -fsSL glassdesk\.dev \| sh/);
});

test('the remote-access client is served at /app/, where its relative URLs resolve', async () => {
  const res = await get('/app/');
  assert.equal(res.status, 200);
  assert.match(await res.text(), /src="main\.js"/);
  assert.equal((await get('/app/main.js')).status, 200);
  const bare = await get('/app');
  assert.equal(bare.status, 301);
  assert.equal(new URL(bare.headers.get('location')).pathname, '/app/');
});

test('videos answer byte ranges (Safari and link-card players need them)', async () => {
  const whole = await fetch(`${BASE}/og.mp4`);
  assert.equal(whole.status, 200);
  assert.equal(whole.headers.get('accept-ranges'), 'bytes');
  const size = (await whole.arrayBuffer()).byteLength;
  const part = await fetch(`${BASE}/og.mp4`, { headers: { range: 'bytes=0-99' } });
  assert.equal(part.status, 206);
  assert.equal(part.headers.get('content-range'), `bytes 0-99/${size}`);
  assert.equal((await part.arrayBuffer()).byteLength, 100);
  const tail = await fetch(`${BASE}/og.mp4`, { headers: { range: 'bytes=-10' } });
  assert.equal(tail.headers.get('content-range'), `bytes ${size - 10}-${size - 1}/${size}`);
  assert.equal((await fetch(`${BASE}/og.mp4`, { headers: { range: `bytes=${size}-` } })).status, 416);
});

test('docs exist in every format and agree', async () => {
  const md = await (await get('/docs.md')).text();
  const full = await (await get('/llms-full.txt')).text();
  assert.equal(md, full);
  const llms = await (await get('/llms.txt')).text();
  assert.match(llms, /^# Glass\n\n> /);
  assert.match(llms, /llms-full\.txt/);
  // /docs is canonical; the trailing-slash form redirects to it.
  assert.equal((await get('/docs')).status, 200);
  assert.match((await get('/docs/')).headers.get('location') ?? '', /\/docs$/);
  const skill = await (await get('/skill.md')).text();
  assert.match(skill, /^---\nname: glass\ndescription: /);
  assert.match(skill, /glass setup --yes --json/);
  const robots = await (await get('/robots.txt')).text();
  for (const bot of ['GPTBot', 'ClaudeBot', 'PerplexityBot']) assert.match(robots, new RegExp(bot));
  assert.match(robots, /^Content-Signal: /m);
});

test('search engines and agents find every page, its card and its answers', async () => {
  const home = await get('/', 'Mozilla/5.0');
  assert.match(home.headers.get('link') ?? '', /<\/index\.md>; rel="alternate"; type="text\/markdown"/);
  assert.match(home.headers.get('strict-transport-security') ?? '', /max-age=/);
  const html = await home.text();
  assert.match(html, /<meta property="og:image" content="https:\/\/glassdesk\.dev\/og\.png">/);
  assert.match(html, /"softwareVersion": "\d+\.\d+\.\d+"/);
  assert.equal((await get('/og.png')).status, 200);

  const md = await fetch(`${BASE}/`, { headers: { accept: 'text/markdown' } });
  assert.match(md.headers.get('content-type') ?? '', /text\/markdown/);
  assert.match(await md.text(), /^# Glass\n/);
  // An agent using curl still gets markdown when it asks for it; `curl … | sh` gets the script.
  const curlMd = await fetch(`${BASE}/`, { headers: { accept: 'text/markdown', 'user-agent': 'curl/8.7.1' } });
  assert.match(await curlMd.text(), /^# Glass\n/);

  const sitemap = await (await get('/sitemap.xml')).text();
  for (const path of ['/', '/features', '/docs']) assert.match(sitemap, new RegExp(`<loc>https://glassdesk\\.dev${path}</loc><lastmod>\\d{4}-\\d{2}-\\d{2}</lastmod>`));

  const docs = await (await get('/docs')).text();
  const ld = JSON.parse(/<script type="application\/ld\+json">(.*?)<\/script>/s.exec(docs)[1]);
  const faq = ld['@graph'].find((n) => n['@type'] === 'FAQPage');
  assert.ok(faq.mainEntity.length >= 5);
  assert.ok(faq.mainEntity.every((q) => q.name.endsWith('?') && q.acceptedAnswer.text.length > 10));

  const skills = await (await get('/.well-known/agent-skills/index.json')).json();
  assert.equal(skills.skills[0].url, 'https://glassdesk.dev/skill.md');
});

const hasReleases = existsSync(join(SITE, 'public/releases/glass-latest-darwin-universal.tar.gz'));

test('release downloads are served with checksums and counted', { skip: !hasReleases && 'no release built (scripts/package.sh)' }, async () => {
  const before = await statsFor();
  const count = (s, kind) => s.downloads.find((d) => d.kind === kind)?.total ?? 0;
  for (const platform of ['darwin-universal', 'linux-x86_64', 'linux-aarch64']) {
    const res = await get(`/releases/glass-latest-${platform}.tar.gz`);
    assert.equal(res.status, 200, platform);
    const sum = await (await get(`/releases/glass-latest-${platform}.tar.gz.sha256`)).text();
    assert.match(sum, /^[0-9a-f]{64}  glass-latest-/);
  }
  await get('/install.sh');
  await new Promise((r) => setTimeout(r, 300));
  const after = await statsFor();
  assert.equal(count(after, 'release') - count(before, 'release'), 3);
  assert.ok(count(after, 'script') > count(before, 'script'));
});

test('feedback is validated', async () => {
  assert.equal((await feedback({ message: '' })).status, 400);
  assert.equal((await feedback({ message: 'x'.repeat(4001) })).status, 400);
  assert.equal((await feedback({ message: 'hi', email: 'not-an-email' })).status, 400);
  assert.equal((await fetch(`${BASE}/api/feedback`, { method: 'POST', body: '{nope' })).status, 400);
  assert.equal((await fetch(`${BASE}/api/feedback`)).status, 405);
});

test('feedback is stored, CORS-enabled and rate limited', async () => {
  const pre = await fetch(`${BASE}/api/feedback`, { method: 'OPTIONS' });
  assert.equal(pre.status, 204);
  assert.equal(pre.headers.get('access-control-allow-origin'), '*');
  const ok = await feedback({ message: 'love it', email: 'me@example.com', source: 'test', version: '0.1.0' });
  assert.equal(ok.status, 200);
  const latest = (await statsFor()).feedback[0];
  assert.equal(latest.message, 'love it');
  assert.equal(latest.email, 'me@example.com');
  let status;
  for (let i = 0; i < 6; i++) status = (await feedback({ message: `spam ${i}` })).status;
  assert.equal(status, 429);
});

test('stats need the key', async () => {
  assert.equal((await fetch(`${BASE}/api/stats`)).status, 401);
  assert.equal((await fetch(`${BASE}/api/stats`, { headers: { authorization: 'Bearer wrong' } })).status, 401);
});

test('a tarball too big for the site redirects to the GitHub release', async () => {
  const { version } = await (await get('/version.json')).json();
  const latest = await get('/releases/glass-latest-darwin-universal.tar.gz');
  assert.equal(latest.status, 302);
  assert.equal(latest.headers.get('location'), `https://github.com/Ntarasiuk/glass/releases/download/v${version}/glass-${version}-darwin-universal.tar.gz`);
  const pinned = await get('/releases/glass-9.9.9-linux-x86_64.tar.gz');
  assert.equal(pinned.headers.get('location'), 'https://github.com/Ntarasiuk/glass/releases/download/v9.9.9/glass-9.9.9-linux-x86_64.tar.gz');
  assert.equal((await get('/releases/nope.tar.gz')).status, 404);
});

// ---- Quick support ----

const support = (method, path, body, headers = {}) =>
  fetch(`${BASE}${path}`, { method, headers: { 'content-type': 'application/json', ...headers }, body: body && JSON.stringify(body) });
const supportLink = () => `${BASE}/app/#r=${'B'.repeat(87)}&l=${'0123456789abcdef'.repeat(2)}`;

test('quick support: the one-liner at /help for terminals, the page for browsers', async () => {
  const script = await get('/help', 'curl/8.7.1');
  assert.match(script.headers.get('content-type'), /shellscript/);
  const text = await script.text();
  assert.match(text, /Glass quick support/);
  assert.match(text, /bin\/glass" support/);
  assert.doesNotMatch(text, /\.local\/bin|LaunchAgents|PREFIX/, 'installs nothing');
  const page = await (await get('/help', 'Mozilla/5.0 (Macintosh) Safari/605.1.15')).text();
  assert.match(page, /Help someone with their computer/);
  assert.match(page, /\/api\/support\/claim/);
});

test('quick support: a code stands for a Glass link once', async () => {
  assert.equal((await support('POST', '/api/support', { url: 'https://evil.example/app/#r=x&l=y' })).status, 400);
  assert.equal((await support('POST', '/api/support', { url: `${BASE}/app/#r=short&l=1` })).status, 400);
  const made = await (await support('POST', '/api/support', { url: supportLink() })).json();
  assert.match(made.code, /^\d{9}$/);
  assert.ok(made.cancel.length >= 20);
  assert.ok(made.expires * 1000 > Date.now() + 50 * 60_000);
  // Typed with spaces, as read out.
  const spaced = made.code.replace(/(\d{3})(?=\d)/g, '$1 ');
  const claimed = await support('POST', '/api/support/claim', { code: spaced });
  assert.equal(claimed.status, 200);
  assert.equal((await claimed.json()).url, supportLink());
  assert.equal((await support('POST', '/api/support/claim', { code: made.code })).status, 404, 'works once');
});

test('quick support: stopping cancels the code', async () => {
  const made = await (await support('POST', '/api/support', { url: supportLink() })).json();
  assert.equal((await support('DELETE', '/api/support', undefined, { 'x-cancel': 'wrong' })).status, 200);
  assert.equal((await support('DELETE', '/api/support', undefined, { 'x-cancel': made.cancel })).status, 200);
  assert.equal((await support('POST', '/api/support/claim', { code: made.code })).status, 404);
});

test('quick support: guessing codes is slow', async () => {
  let status;
  for (let i = 0; i < 25; i++) status = (await support('POST', '/api/support/claim', { code: '000000000' })).status;
  assert.equal(status, 429);
});
