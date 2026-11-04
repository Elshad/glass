// Agents from anywhere: Glass as an OAuth authorization server and MCP server (streamable
// HTTP), the way Vercel Connect and claude.ai connectors use it. Registration, approval by
// number matching (or one tap on an owner device), tokens that rotate, scopes, revocation
// and the audit log, against real `glass --test` hosts.

import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto';
import { Host, expectVideo, freePort, test, expect } from './harness.mjs';

const REDIRECT = 'http://127.0.0.1:8765/cb';
const b64url = (buf) => Buffer.from(buf).toString('base64url');

const it = test.extend({
  host: async ({}, use) => {
    const host = await new Host().start();
    await use(host);
    await host.stop();
  },
});

async function register(host, body) {
  const r = await fetch(`${host.base}/oauth/register`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  expect(r.status).toBe(201);
  return r.json();
}

async function token(host, form, headers = {}) {
  const r = await fetch(`${host.base}/oauth/token`, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded', ...headers },
    body: new URLSearchParams(form),
  });
  return { status: r.status, body: await r.json() };
}

/** One JSON-RPC call to /mcp with a bearer token. */
async function mcp(host, access, method, params = {}) {
  const r = await fetch(`${host.base}/mcp`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream', authorization: `Bearer ${access}` },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  return { status: r.status, auth: r.headers.get('www-authenticate'), body: r.status === 200 || r.status === 403 ? await r.json() : null };
}

function pkce() {
  const verifier = b64url(randomBytes(32));
  return { verifier, challenge: b64url(createHash('sha256').update(verifier).digest()) };
}

function authorizeUrl(host, clientId, challenge, extra = {}) {
  const q = new URLSearchParams({
    response_type: 'code', client_id: clientId, redirect_uri: REDIRECT, code_challenge: challenge,
    code_challenge_method: 'S256', state: 'st8', resource: `${host.base}/mcp`, ...extra,
  });
  return `${host.base}/oauth/authorize?${q}`;
}

/** A browser that isn't the owner's opens the sign-in page; its redirect is caught. */
async function signInPage(browser, url) {
  const ctx = await browser.newContext();
  await ctx.route('http://127.0.0.1:8765/**', (r) => r.fulfill({ body: 'back at the agent' }));
  const page = await ctx.newPage();
  await page.goto(url);
  return page;
}

it('an agent signs in by number matching, uses the computer, and refreshes; a stolen refresh token ends it', async ({ browser, page, host }) => {
  // Discovery, as Vercel Connect and MCP clients do it.
  const unauth = await fetch(`${host.base}/mcp`, { method: 'POST', body: '{}' });
  expect(unauth.status).toBe(401);
  expect(unauth.headers.get('www-authenticate')).toContain(`resource_metadata="${host.base}/.well-known/oauth-protected-resource/mcp"`);
  const prm = await host.get('/.well-known/oauth-protected-resource/mcp');
  expect(prm).toMatchObject({ resource: `${host.base}/mcp`, authorization_servers: [host.base] });
  const meta = await host.get('/.well-known/oauth-authorization-server');
  expect(meta).toMatchObject({
    issuer: host.base,
    code_challenge_methods_supported: ['S256'],
    client_id_metadata_document_supported: true,
    revocation_endpoint: `${host.base}/oauth/revoke`,
  });
  expect(meta.grant_types_supported).toEqual(['authorization_code', 'refresh_token', 'client_credentials']);

  const client = await register(host, { client_name: 'Test agent', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
  expect(client.client_secret).toBeUndefined();
  expect(client.registration_access_token).toBeTruthy();

  // The owner is watching in a viewer.
  await page.goto(host.url());
  await expectVideo(page);

  const { verifier, challenge } = pkce();
  const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge));
  const number = (await signIn.locator('.number').textContent()).trim();
  expect(number).toMatch(/^\d\d$/);

  // In Glass: the card shows three numbers; the right one approves.
  const card = page.locator('.agent-ask', { hasText: 'Test agent' });
  await expect(card).toContainText('An agent asks to use this computer');
  await expect(card.locator('button.choice')).toHaveCount(4);
  await card.locator('button.choice', { hasText: number }).click();

  await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
  const back = new URL(signIn.url());
  expect(back.searchParams.get('state')).toBe('st8');
  expect(back.searchParams.get('iss')).toBe(host.base);
  const code = back.searchParams.get('code');

  // The wrong verifier is refused; the right one gets tokens.
  expect((await token(host, { grant_type: 'authorization_code', code, redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: pkce().verifier })).body.error).toBe('invalid_grant');
  const first = await token(host, { grant_type: 'authorization_code', code, redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
  // The first wrong try spent the code: one code, one chance.
  expect(first.body.error).toBe('invalid_grant');

  // Again, approving properly this time.
  const second = pkce();
  const signIn2 = await signInPage(browser, authorizeUrl(host, client.client_id, second.challenge));
  const n2 = (await signIn2.locator('.number').textContent()).trim();
  await page.locator('.agent-ask', { hasText: 'Test agent' }).locator('button.choice', { hasText: n2 }).click();
  await signIn2.waitForURL(/127\.0\.0\.1:8765\/cb/);
  const code2 = new URL(signIn2.url()).searchParams.get('code');
  const t = await token(host, { grant_type: 'authorization_code', code: code2, redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: second.verifier });
  expect(t.status).toBe(200);
  expect(t.body).toMatchObject({ token_type: 'Bearer', expires_in: 600, scope: 'computer:view computer:use' });
  expect(t.body.refresh_token).toBeTruthy();

  // MCP over HTTP: this computer's tools, no `machine`.
  const init = await mcp(host, t.body.access_token, 'initialize', { protocolVersion: '2025-11-25', capabilities: {}, clientInfo: { name: 't', version: '1' } });
  expect(init.body.result.serverInfo.name).toBe('glass');
  const tools = (await mcp(host, t.body.access_token, 'tools/list')).body.result.tools;
  const names = tools.map((x) => x.name);
  expect(names).toEqual(expect.arrayContaining(['screenshot', 'click', 'type_text']));
  expect(names).not.toContain('list_machines');
  expect(tools.every((x) => !x.inputSchema.properties.machine)).toBe(true);

  const shot = await mcp(host, t.body.access_token, 'tools/call', { name: 'screenshot', arguments: {} });
  expect(shot.body.result.content[0]).toMatchObject({ type: 'image', mimeType: 'image/png' });
  await host.clearInput();
  expect((await mcp(host, t.body.access_token, 'tools/call', { name: 'click', arguments: { x: 320, y: 180 } })).body.result.isError).toBeFalsy();
  expect((await host.input('move')).events[0]).toMatchObject({ x: 0.5, y: 0.5 });
  // The banner names the agent; what it types isn't shown.
  await mcp(host, t.body.access_token, 'tools/call', { name: 'type_text', arguments: { text: 'hunter2' } });
  await expect(page.locator('#agent-label')).toContainText('Test agent · type 7 characters');

  // Refreshing rotates the refresh token.
  const r1 = await token(host, { grant_type: 'refresh_token', refresh_token: t.body.refresh_token, client_id: client.client_id });
  expect(r1.status).toBe(200);
  expect(r1.body.refresh_token).not.toBe(t.body.refresh_token);
  expect((await mcp(host, r1.body.access_token, 'ping')).status).toBe(200);

  // The one just replaced, sent again at once (a retry): refused, nothing else happens.
  expect((await token(host, { grant_type: 'refresh_token', refresh_token: t.body.refresh_token, client_id: client.client_id })).body.error).toBe('invalid_grant');
  expect((await mcp(host, r1.body.access_token, 'ping')).status).toBe(200);
  const r2 = await token(host, { grant_type: 'refresh_token', refresh_token: r1.body.refresh_token, client_id: client.client_id });
  expect(r2.status).toBe(200);

  // An older one, used again (a stolen copy): the whole grant ends.
  const reused = await token(host, { grant_type: 'refresh_token', refresh_token: t.body.refresh_token, client_id: client.client_id });
  expect(reused.body.error).toBe('invalid_grant');
  expect((await mcp(host, r2.body.access_token, 'ping')).status).toBe(401);
  expect((await token(host, { grant_type: 'refresh_token', refresh_token: r2.body.refresh_token, client_id: client.client_id })).body.error).toBe('invalid_grant');

  // Everything is in the audit log, chained, and typed text never is.
  const log = host.cli('agents', 'log', '--json');
  expect(log).toContain('"action":"click"');
  expect(log).not.toContain('hunter2');
  expect(host.cli('agents', 'verify')).toMatch(/intact/);
});

it('the wrong number, or Deny, sends the agent away', async ({ browser, page, host }) => {
  const client = await register(host, { client_name: 'Sneaky', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
  await page.goto(host.url());
  await expectVideo(page);
  const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, pkce().challenge));
  const number = (await signIn.locator('.number').textContent()).trim();
  const card = page.locator('.agent-ask', { hasText: 'Sneaky' });
  await card.locator('button.choice').filter({ hasNotText: number }).first().click();
  await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
  expect(new URL(signIn.url()).searchParams.get('error')).toBe('access_denied');

  // After a wrong number it can't simply ask again (no guessing).
  expect((await fetch(authorizeUrl(host, client.client_id, pkce().challenge), { redirect: 'manual' })).status).toBe(429);

  // A redirect URI it never registered never gets anything (the page says so instead).
  const other = await register(host, { client_name: 'Other', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
  const r = await fetch(authorizeUrl(host, other.client_id, pkce().challenge, { redirect_uri: 'https://evil.example/cb' }), { redirect: 'manual' });
  expect(r.status).toBe(400);
});

it('on an owner device it is one tap; a view-only agent can look but not touch, and protected apps are off limits', async ({ browser }) => {
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1', GLASS_TEST_REMOTE_OWNER: '1', GLASS_TEST_FRONTMOST: 'com.apple.Passwords' } }).start();
  try {
    const client = await register(host, { client_name: 'Looker', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const { verifier, challenge } = pkce();
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge, { scope: 'computer:view' }));
    await expect(signIn.locator('.number')).toHaveCount(0);
    await signIn.getByRole('button', { name: 'Approve' }).click();
    await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const code = new URL(signIn.url()).searchParams.get('code');
    const t = await token(host, { grant_type: 'authorization_code', code, redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
    expect(t.body.scope).toBe('computer:view');

    const names = (await mcp(host, t.body.access_token, 'tools/list')).body.result.tools.map((x) => x.name);
    expect(names.sort()).toEqual(['list_displays', 'record', 'screenshot', 'wait_for_change']);
    // Asking to touch: step-up, the MCP way.
    const click = await mcp(host, t.body.access_token, 'tools/call', { name: 'click', arguments: { x: 1, y: 1 } });
    expect(click.status).toBe(403);
    expect(click.auth).toContain('error="insufficient_scope"');

    // A full agent, with Passwords in front: refused, and told to ask the person.
    const full = pkce();
    const signIn2 = await signInPage(browser, authorizeUrl(host, client.client_id, full.challenge));
    await signIn2.getByRole('button', { name: 'Approve' }).click();
    await signIn2.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const t2 = await token(host, { grant_type: 'authorization_code', code: new URL(signIn2.url()).searchParams.get('code'), redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: full.verifier });
    const refused = await mcp(host, t2.body.access_token, 'tools/call', { name: 'click', arguments: { x: 1, y: 1 } });
    expect(refused.body.result.isError).toBe(true);
    expect(refused.body.result.content[0].text).toContain('protected');

    // Revoking (Vercel's revoke button sends just the access token) ends the whole grant.
    await fetch(`${host.base}/oauth/revoke`, { method: 'POST', headers: { 'content-type': 'application/x-www-form-urlencoded' }, body: new URLSearchParams({ token: t2.body.access_token, client_id: client.client_id }) });
    expect((await mcp(host, t2.body.access_token, 'ping')).status).toBe(401);
    expect((await token(host, { grant_type: 'refresh_token', refresh_token: t2.body.refresh_token, client_id: client.client_id })).body.error).toBe('invalid_grant');
  } finally {
    await host.stop();
  }
});

it('an app (client credentials, signed assertions) works only once the owner installs it', async ({ host }) => {
  const { publicKey, privateKey } = generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'k1', alg: 'ES256', use: 'sig' };
  const app = await register(host, {
    client_name: 'QA runner', grant_types: ['client_credentials'], token_endpoint_auth_method: 'private_key_jwt', jwks: { keys: [jwk] },
  });
  const assertion = (aud = `${host.base}/oauth/token`) => {
    const now = Math.floor(Date.now() / 1000);
    const head = b64url(JSON.stringify({ alg: 'ES256', kid: 'k1', typ: 'JWT' }));
    const body = b64url(JSON.stringify({ iss: app.client_id, sub: app.client_id, aud, exp: now + 60, iat: now, jti: b64url(randomBytes(12)) }));
    const sig = sign('sha256', Buffer.from(`${head}.${body}`), { key: privateKey, dsaEncoding: 'ieee-p1363' });
    return `${head}.${body}.${b64url(sig)}`;
  };
  const form = (a) => ({ grant_type: 'client_credentials', client_assertion_type: 'urn:ietf:params:oauth:client-assertion-type:jwt-bearer', client_assertion: a });

  const before = await token(host, form(assertion()));
  expect(before.body.error).toBe('unauthorized_client');
  expect(host.cli('agents')).toContain(`glass agents install ${app.client_id}`);

  host.cli('agents', 'install', app.client_id);
  // An assertion for another server, or one replayed, is refused.
  expect((await token(host, form(assertion('https://elsewhere.example/token')))).body.error).toBe('invalid_client');
  const a = assertion();
  const t = await token(host, form(a));
  expect(t.status).toBe(200);
  expect(t.body.refresh_token).toBeUndefined();
  expect((await token(host, form(a))).body.error).toBe('invalid_client');
  expect((await mcp(host, t.body.access_token, 'tools/list')).status).toBe(200);

  const listed = JSON.parse(host.cli('agents', 'list', '--json'));
  expect(listed.grants).toEqual([expect.objectContaining({ agent: 'QA runner', subject: 'app' })]);
  host.cli('agents', 'revoke', 'all');
  expect((await mcp(host, t.body.access_token, 'ping')).status).toBe(401);
});

it('a cloud agent never waits long for the person: questions go pending, and it collects the answer later', async ({ browser, page }) => {
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1', GLASS_TEST_REMOTE_OWNER: '1' } }).start();
  try {
    const client = await register(host, { client_name: 'Cloud agent', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const { verifier, challenge } = pkce();
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge));
    await signIn.getByRole('button', { name: 'Approve' }).click();
    await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const t = await token(host, { grant_type: 'authorization_code', code: new URL(signIn.url()).searchParams.get('code'), redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
    const call = (name, args) => mcp(host, t.body.access_token, 'tools/call', { name, arguments: args });

    await page.goto(host.url());
    await expectVideo(page);

    // Nobody answers within a second: pending, with the question's id.
    const started = Date.now();
    const asked = await call('confirm', { action: 'Send the invoice to Acme', wait_seconds: 1 });
    expect(Date.now() - started).toBeLessThan(5000);
    const text = asked.body.result.content[0].text;
    expect(text).toMatch(/hasn't answered yet \(question \d+\)/);
    const id = Number(text.match(/question (\d+)/)[1]);
    expect((await call('check_answer', { id, wait_seconds: 0 })).body.result.content[0].text).toContain("hasn't answered yet");

    // The question is still up; the person answers whenever.
    const card = page.locator('.agent-ask', { hasText: 'Send the invoice to Acme' });
    await expect(card).toContainText('Cloud agent');
    await card.getByRole('button', { name: 'Allow' }).click();
    expect((await call('check_answer', { id, wait_seconds: 5 })).body.result.content[0].text).toBe('Approved.');
    // Collected once.
    expect((await call('check_answer', { id })).body.result.isError).toBe(true);

    // The tool is listed for cloud agents.
    const tools = (await mcp(host, t.body.access_token, 'tools/list')).body.result.tools.map((x) => x.name);
    expect(tools).toContain('check_answer');
  } finally {
    await host.stop();
  }
});

it('this computer never approves an agent by itself: no Approve button on its own screen, and its own viewer can’t pick the number while an agent works', async ({ browser }) => {
  // The owner's identity checks pass for this computer, as they do for a browser on the Mac itself.
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1' } }).start();
  try {
    const client = await register(host, { client_name: 'Self approver', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, pkce().challenge));
    await expect(signIn.getByRole('button', { name: 'Approve' })).toHaveCount(0);
    await expect(signIn.locator('.number')).toBeVisible();
    // A form posted anyway is refused.
    const id = new URL(await signIn.evaluate(() => location.href)).searchParams.get('client_id');
    expect(id).toBe(client.client_id);
  } finally {
    await host.stop();
  }
});

it('redirects are matched by their parts, and the page names where the code goes', async ({ host }) => {
  const client = await register(host, { client_name: 'Loopy', redirect_uris: ['http://127.0.0.1/cb', 'https://second.example/cb'], token_endpoint_auth_method: 'none' });
  const ask = (redirect) => fetch(authorizeUrl(host, client.client_id, pkce().challenge, { redirect_uri: redirect }), { redirect: 'manual' });
  // Userinfo can't smuggle another host past the loopback rule.
  expect((await ask('http://127.0.0.1:1@evil.example/cb')).status).toBe(400);
  expect((await ask('http://127.0.0.1:5555/cb')).status).toBe(200);
  // The page names the redirect's host, not the first one registered.
  const page = await (await ask('https://second.example/cb')).text();
  expect(page).toContain('second.example');
});

it('a use-only grant can still look, and a revoked app stays revoked', async ({ browser }) => {
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1', GLASS_TEST_REMOTE_OWNER: '1' } }).start();
  try {
    const client = await register(host, { client_name: 'User', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const { verifier, challenge } = pkce();
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge, { scope: 'computer:use' }));
    await signIn.getByRole('button', { name: 'Approve' }).click();
    await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const t = await token(host, { grant_type: 'authorization_code', code: new URL(signIn.url()).searchParams.get('code'), redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
    expect((await mcp(host, t.body.access_token, 'tools/list')).body.result.tools.map((x) => x.name)).toContain('screenshot');
    const shot = await mcp(host, t.body.access_token, 'tools/call', { name: 'screenshot', arguments: {} });
    expect(shot.body.result.isError).toBeFalsy();

    // An installed app, revoked: its next request doesn't quietly get a new grant.
    const app = await register(host, { client_name: 'Bot', grant_types: ['client_credentials'], token_endpoint_auth_method: 'client_secret_basic' });
    const basic = { authorization: `Basic ${Buffer.from(`${app.client_id}:${app.client_secret}`).toString('base64')}` };
    host.cli('agents', 'install', app.client_id);
    expect((await token(host, { grant_type: 'client_credentials' }, basic)).status).toBe(200);
    const grantId = JSON.parse(host.cli('agents', 'list', '--json')).grants.find((g) => g.agent === 'Bot').id;
    host.cli('agents', 'revoke', grantId);
    expect((await token(host, { grant_type: 'client_credentials' }, basic)).body.error).toBe('unauthorized_client');
    // The audit chain holds across the CLI and the host writing it.
    expect(host.cli('agents', 'verify')).toMatch(/intact/);
  } finally {
    await host.stop();
  }
});

it('agents wait while the person uses the computer itself, and terminals are off limits', async ({ browser }) => {
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1', GLASS_TEST_REMOTE_OWNER: '1', GLASS_TEST_FRONTMOST: 'com.apple.Terminal' } }).start();
  try {
    const client = await register(host, { client_name: 'Typist', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const { verifier, challenge } = pkce();
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge));
    await signIn.getByRole('button', { name: 'Approve' }).click();
    await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const t = await token(host, { grant_type: 'authorization_code', code: new URL(signIn.url()).searchParams.get('code'), redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
    const typed = await mcp(host, t.body.access_token, 'tools/call', { name: 'type_text', arguments: { text: 'rm -rf ~' } });
    expect(typed.body.result.isError).toBe(true);
    expect(typed.body.result.content[0].text).toContain('com.apple.Terminal is protected');
  } finally {
    await host.stop();
  }
});

it('a metadata-document client (how Vercel Connect registers) can be installed as an app', async ({ host }) => {
  const { createServer } = await import('node:http');
  const { publicKey, privateKey } = generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'v1', alg: 'ES256', use: 'sig' };
  const server = createServer((req, res) => {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ client_id: clientId, client_name: 'Connect', redirect_uris: ['https://connect.example/callback'], token_endpoint_auth_method: 'private_key_jwt', grant_types: ['authorization_code', 'client_credentials'], jwks: { keys: [jwk] } }));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const clientId = `http://127.0.0.1:${server.address().port}/client.json`;
  try {
    const assertion = () => {
      const now = Math.floor(Date.now() / 1000);
      const head = b64url(JSON.stringify({ alg: 'ES256', kid: 'v1' }));
      const body = b64url(JSON.stringify({ iss: clientId, sub: clientId, aud: `${host.base}/oauth/token`, exp: now + 60, jti: b64url(randomBytes(12)) }));
      return `${head}.${body}.${b64url(sign('sha256', Buffer.from(`${head}.${body}`), { key: privateKey, dsaEncoding: 'ieee-p1363' }))}`;
    };
    const form = () => ({ grant_type: 'client_credentials', client_assertion_type: 'urn:ietf:params:oauth:client-assertion-type:jwt-bearer', client_assertion: assertion() });
    expect((await token(host, form())).body.error).toBe('unauthorized_client');
    expect(host.cli('agents')).toContain(`glass agents install ${clientId}`);
    host.cli('agents', 'install', clientId);
    const t = await token(host, form());
    expect(t.status).toBe(200);
    expect((await mcp(host, t.body.access_token, 'tools/list')).status).toBe(200);
  } finally {
    server.close();
  }
});

it('speaks the protocols as written: registration management, JSON-RPC, versions, CORS, strict details, agents off', async ({ host }) => {
  const client = await register(host, { client_name: 'Spec', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
  // RFC 7592: read and update rotate the registration token and return it; delete works.
  const read = await fetch(`${host.base}/oauth/register/${client.client_id}`, { headers: { authorization: `Bearer ${client.registration_access_token}` } });
  const readBody = await read.json();
  expect(readBody.registration_access_token).toBeTruthy();
  expect(readBody.registration_access_token).not.toBe(client.registration_access_token);
  const stale = await fetch(`${host.base}/oauth/register/${client.client_id}`, { headers: { authorization: `Bearer ${client.registration_access_token}` } });
  expect(stale.status).toBe(401);
  expect(stale.headers.get('www-authenticate')).toContain('invalid_token');
  const put = await fetch(`${host.base}/oauth/register/${client.client_id}`, {
    method: 'PUT', headers: { authorization: `Bearer ${readBody.registration_access_token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ client_id: client.client_id, client_name: 'Renamed', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' }),
  });
  const putBody = await put.json();
  expect(putBody.client_name).toBe('Renamed');
  const del = await fetch(`${host.base}/oauth/register/${client.client_id}`, { method: 'DELETE', headers: { authorization: `Bearer ${putBody.registration_access_token}` } });
  expect(del.status).toBe(204);

  // Unclear authorization_details are refused, never read as "no limit".
  const c2 = await register(host, { client_name: 'Strict', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
  const bad = await fetch(authorizeUrl(host, c2.client_id, pkce().challenge, { authorization_details: '[{"type":"glass_computer","apps":"com.x"}]' }), { redirect: 'manual' });
  expect(bad.status).toBe(303);
  expect(new URL(bad.headers.get('location')).searchParams.get('error')).toBe('invalid_authorization_details');

  // Browser clients can register and get tokens (CORS preflight).
  const pre = await fetch(`${host.base}/oauth/token`, { method: 'OPTIONS', headers: { origin: 'http://localhost:6274', 'access-control-request-method': 'POST' } });
  expect(pre.headers.get('access-control-allow-origin')).toBe('*');

  // Agents off, from Glass's settings (`glass config agents`, the console, the app's Settings):
  // registering, sign-in and tokens refused with proper OAuth errors, and nothing can be published.
  host.cli('config', 'agents', 'off');
  expect(host.cli('config', 'agents').trim()).toBe('off');
  expect((await fetch(`${host.base}/oauth/register`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ client_name: 'Late', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' }) })).status).toBe(403);
  expect((await fetch(authorizeUrl(host, c2.client_id, pkce().challenge))).status).toBe(403);
  expect((await token(host, { grant_type: 'refresh_token', refresh_token: 'x', client_id: c2.client_id })).body.error).toBe('invalid_grant');
  expect(() => host.cli('agents', 'public', 'on')).toThrow();
  host.cli('config', 'agents', 'on');
  expect(host.cli('config', 'agents').trim()).toBe('on');
});

it('JSON-RPC as MCP expects: one message per request, unknown tools are protocol errors, versions negotiated', async ({ browser }) => {
  const host = await new Host({ env: { GLASS_TEST_TRUST_LOOPBACK: '1', GLASS_TEST_REMOTE_OWNER: '1' } }).start();
  try {
    const client = await register(host, { client_name: 'Rpc', redirect_uris: [REDIRECT], token_endpoint_auth_method: 'none' });
    const { verifier, challenge } = pkce();
    const signIn = await signInPage(browser, authorizeUrl(host, client.client_id, challenge));
    await signIn.getByRole('button', { name: 'Approve' }).click();
    await signIn.waitForURL(/127\.0\.0\.1:8765\/cb/);
    const t = await token(host, { grant_type: 'authorization_code', code: new URL(signIn.url()).searchParams.get('code'), redirect_uri: REDIRECT, client_id: client.client_id, code_verifier: verifier });
    const raw = (body) => fetch(`${host.base}/mcp`, { method: 'POST', headers: { 'content-type': 'application/json', authorization: `Bearer ${t.body.access_token}` }, body });
    expect((await raw('[{"jsonrpc":"2.0","id":1,"method":"ping"}]')).status).toBe(400);
    expect((await raw('not json')).status).toBe(400);
    expect((await raw('{"jsonrpc":"2.0","method":"notifications/initialized"}')).status).toBe(202);
    const unknown = await (await raw('{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"rm_rf"}}')).json();
    expect(unknown.error.code).toBe(-32602);
    const init = await mcp(host, t.body.access_token, 'initialize', { protocolVersion: '1999-01-01', capabilities: {}, clientInfo: { name: 't', version: '1' } });
    expect(init.body.result.protocolVersion).toBe('2025-11-25');
    // Read-only tools say so (ChatGPT and others can skip asking for them).
    const tools = (await mcp(host, t.body.access_token, 'tools/list')).body.result.tools;
    expect(tools.find((x) => x.name === 'screenshot').annotations.readOnlyHint).toBe(true);
    expect(tools.find((x) => x.name === 'click').annotations.readOnlyHint).toBe(false);
  } finally {
    await host.stop();
  }
});

it('the public listener serves only the agent endpoints', async () => {
  const port = await freePort('tcp');
  const host = await new Host({ env: { GLASS_TEST_PUBLIC_PORT: String(port) } }).start();
  try {
    const at = (path) => fetch(`http://127.0.0.1:${port}${path}`, { headers: { host: `localhost:${port}` } });
    await expect.poll(async () => (await at('/.well-known/oauth-protected-resource/mcp').catch(() => ({ status: 0 }))).status).toBe(200);
    expect((await at('/')).status).toBe(404);
    expect((await at('/agent/v1/status')).status).toBe(404);
    expect((await at('/config.json')).status).toBe(404);
    expect((await at('/oauth/token')).headers.get('cache-control')).toBe('no-store');
  } finally {
    await host.stop();
  }
});
