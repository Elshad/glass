import { execFileSync, spawn } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
const [root, bin, linkName, runs='3', waitS='25'] = process.argv.slice(2);
process.env.GLASS_BIN = bin;
const { Host, LINKS, shape } = await import(join(root, 'bench/run.mjs'));
const http = await import('node:http');
const osa = (s) => execFileSync('osascript', ['-e', s]).toString().trim();
for (let r = 0; r < Number(runs); r++) {
  const host = new Host({ GLASS_TEST_BENCH: 'anim', GLASS_TEST_SIZE: '1280x720' });
  await host.start(60);
  const link = LINKS[linkName];
  const shaper = await shape({ shaper: 'user' }, link, host.wt);
  // A small proxy: Safari can't be given a page route, so rewrite config.json's wtPort here.
  const srv = http.createServer(async (q, a) => {
    const chunks = []; for await (const c of q) chunks.push(c);
    const res = await fetch(host.base + q.url, { method: q.method, headers: { 'content-type': q.headers['content-type'] ?? 'text/plain' }, body: chunks.length ? Buffer.concat(chunks) : undefined });
    let body = Buffer.from(await res.arrayBuffer());
    const headers = Object.fromEntries(res.headers); delete headers['content-encoding']; delete headers['content-length'];
    if (q.url.startsWith('/config.json') && shaper.port) body = Buffer.from(JSON.stringify({ ...JSON.parse(body), wtPort: shaper.port }));
    if ((headers['content-type'] ?? '').includes('text/html')) {
      body = Buffer.from(body.toString().replace('<head>', `<head><script>Object.defineProperty(document,'visibilityState',{get:()=>'visible'});Object.defineProperty(document,'hidden',{get:()=>false});</script>`));
    }
    a.writeHead(res.status, headers); a.end(body);
  });
  await new Promise((d) => srv.listen(0, '127.0.0.1', d));
  const url = `http://localhost:${srv.address().port}/#t=${host.token}`;
  osa(`tell application "Safari" to make new document with properties {URL:"${url}"}`);
  await new Promise((d) => setTimeout(d, Number(waitS) * 1000));
  try { osa(`tell application "Safari" to close (every window whose URL of current tab contains "localhost:${srv.address().port}")`); } catch (e) { console.log('close failed', String(e).slice(0, 100)); }
  console.log(execFileSync('sh',['-c',`grep -E 'first keyframe|first viewport|encoding widths|Viewport' ${host.dir}/logs/host.log | cut -c12-130 | head -8`]).toString());
  const log = join(host.dir, 'logs', 'client.log');
  const lines = existsSync(log) ? readFileSync(log, 'utf8').split('\n') : [];
  const ev = [];
  for (const l of lines) { const m = l.match(/\{.*\}/); if (!m) continue; try { const d = JSON.parse(m[0]); ev.push(d); } catch {} }
  const t0 = ev.find((e) => e.msg === 'connecting')?.t;
  const at = (s) => { const e = ev.find((x) => x.msg?.startsWith(s)); return e ? e.t - t0 : null; };
  const st = ev.filter((e) => e.msg === 'stats').map((e) => `${e.data.fps}/${e.data.mbps.toFixed(2)}/${Math.round(e.data.rtt)}`);
  const ua = ev.find((e) => e.msg === 'client start')?.data?.ua?.match(/Version\/[\d.]+.*Safari/)?.[0];
  const counts = await shaper.stop().catch(() => undefined);
  console.log(JSON.stringify({ link: linkName, run: r, ua, ready: at('transport ready'), welcome: at('welcome'), decoder: at('decoder configured'), wait: ev.filter((e) => e.msg === 'waiting for first picture').map((e) => JSON.stringify(e.data)).slice(0, 12), stats: st.slice(0, 5), shaper: counts }));
  srv.close();
  await host.stop();
}
process.exit(0);
