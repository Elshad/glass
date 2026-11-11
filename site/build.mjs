import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
// Generates the docs in every format from docs.md (the single source), and copies the
// latest release into public/. Run before `wrangler deploy` (npm run deploy does).

import { copyFileSync, cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { marked } from 'marked';

// Headings get ids (`## Connecting` → #connecting), so passages can be linked and quoted.
marked.use({
  renderer: {
    heading({ tokens, depth, text }) {
      const id = text.toLowerCase().replace(/<[^>]+>|`/g, '').replace(/[^\w\s-]/g, '').trim().replace(/\s+/g, '-');
      return `<h${depth} id="${id}">${this.parser.parseInline(tokens)}</h${depth}>\n`;
    },
    // Tables: the row label is a header cell, a leading Yes / No / None becomes a coloured pill (the word stays
    // in the text), Glass's own row or column is highlighted, and the table scrolls inside its own frame.
    table(t) {
      const inline = (c) => this.parser.parseInline(c.tokens);
      const isUs = (c) => /^Glass\b/.test(c.text.replace(/[*_`]/g, ''));
      const usCol = t.header.findIndex((c, i) => i > 0 && isUs(c));
      const value = (c) => {
        const html = inline(c);
        const m = c.text.replace(/[*_`]/g, '').match(/^(Yes|No|None)\b/);
        if (!m) return html;
        const rest = html.replace(new RegExp(`^(<[^>]+>)*${m[1]}`), '').replace(/^[\s;,:]+/, '').trim();
        return `<span class="pill ${m[1] === 'No' ? 'no' : 'yes'}">${m[1]}</span>${rest ? ` <span class="note">${rest}</span>` : ''}`;
      };
      const head = t.header.map((c, i) => `<th scope="col"${i === usCol ? ' class="us"' : ''}>${inline(c)}</th>`).join('');
      const rows = t.rows.map((r) => {
        const us = isUs(r[0]);
        const cells = r.map((c, i) => (i === 0 ? `<th scope="row">${inline(c)}</th>` : `<td${i === usCol ? ' class="us"' : ''}>${value(c)}</td>`)).join('');
        return `<tr${us ? ' class="us"' : ''}>${cells}</tr>`;
      }).join('\n');
      const wide = t.header.length > 5 ? ' wide' : t.header.length > 3 ? ' mid' : '';
      return `<div class="table-wrap${wide}" role="region" aria-label="Comparison table" tabindex="0"><table><thead><tr>${head}</tr></thead><tbody>\n${rows}\n</tbody></table></div>\n`;
    },
  },
});

const SITE = 'https://glassdesk.dev';
const docs = readFileSync('docs.md', 'utf8');

// Who makes Glass, on every page: a stable @id, the logo and the repo, so "Glass" (a common
// word) resolves to this remote desktop.
const ORG = {
  '@type': 'Organization',
  '@id': `${SITE}/#organization`,
  name: 'Glass',
  alternateName: ['Glass remote desktop', 'glassdesk.dev'],
  url: `${SITE}/`,
  description: 'Glass makes a free, open-source, low-latency remote desktop for Mac and Linux that you use from any browser.',
  logo: { '@type': 'ImageObject', '@id': `${SITE}/#logo`, url: `${SITE}/icon-512.png`, width: 512, height: 512, caption: 'Glass' },
  image: { '@id': `${SITE}/#logo` },
  founder: { '@id': `${SITE}/#author` },
  sameAs: ['https://github.com/Ntarasiuk/glass'],
};
const AUTHOR = { '@type': 'Person', '@id': `${SITE}/#author`, name: 'Nathan Tarasiuk', url: 'https://github.com/Ntarasiuk', sameAs: ['https://github.com/Ntarasiuk'] };

// Guides and comparisons: guides/*.md, each with a small front matter block.
const guides = readdirSync('guides')
  .filter((f) => f.endsWith('.md'))
  .map((f) => {
    const src = `guides/${f}`;
    const [, front, markdown] = readFileSync(src, 'utf8').match(/^---\n([\s\S]*?)\n---\n([\s\S]*)$/) ?? [];
    if (!front) throw new Error(`${src}: missing front matter`);
    const meta = Object.fromEntries(front.split('\n').map((l) => l.match(/^(\w+):\s*(.*)$/).slice(1)));
    for (const k of ['path', 'title', 'description', 'label']) if (!meta[k]) throw new Error(`${src}: no ${k}`);
    if (meta.description.length > 160) throw new Error(`${src}: description over 160 characters`);
    return { ...meta, src, markdown, order: Number(meta.order ?? 99) };
  })
  .sort((a, b) => a.order - b.order);

const human = (iso) => new Date(`${iso}T12:00:00Z`).toLocaleDateString('en-US', { year: 'numeric', month: 'long', day: 'numeric', timeZone: 'UTC' });
const esc = (t) => String(t).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const ld = (graph) => `<script type="application/ld+json">${JSON.stringify({ '@context': 'https://schema.org', '@graph': graph }).replace(/</g, '\\u003c')}</script>`;

/** The footer every page shares: what Glass is, when the page changed, and links between pages. */
function footer(modified, { wrap = true } = {}) {
  const compare = guides.map((g) => `<a href="${g.path}">${esc(g.label)}</a>`).join('');
  return `<footer class="site${wrap ? ' wrap' : ''}">
    <span>Glass is a free, open-source remote desktop for Mac and Linux (MIT). Updated <time datetime="${modified}">${human(modified)}</time>.</span>
    <nav aria-label="Footer"><a href="/features">Features</a><a href="/docs">Documentation</a><a href="/changelog">Changelog</a><a href="/guides">Guides</a><a href="/help">Quick support</a><a href="/docs#code-signing-policy">Code signing policy</a><a href="/features#feedback">Feedback</a><a href="https://github.com/Ntarasiuk/glass">Star on GitHub</a><a href="/docs.md">docs.md</a><a href="/llms.txt">llms.txt</a><a href="/install.sh">install.sh</a></nav>
    <nav aria-label="Guides and comparisons">${compare}</nav>
  </footer>`;
}

/** Fills a hand-written page's placeholders: dates, the shared entities and the footer. */
function finish(html, modified) {
  const out = html
    .replaceAll('{{modified}}', modified)
    .replaceAll('"{{organization}}"', JSON.stringify(ORG))
    .replaceAll('"{{author}}"', JSON.stringify(AUTHOR))
    .replaceAll('{{footer}}', footer(modified));
  const left = out.match(/\{\{\w+\}\}/);
  if (left) throw new Error(`unfilled placeholder ${left[0]}`);
  return out;
}

// Hosts check this to offer updates.
const version = readFileSync('../Cargo.toml', 'utf8').match(/^version = "(.+)"/m)[1];
// A pre-release (0.11.0-windows.1) is never offered to installed computers: the site only
// ever carries a real release.
if (version.includes('-')) throw new Error(`Cargo.toml is at pre-release ${version}: the site only serves releases (build it from main)`);
writeFileSync('public/version.json', JSON.stringify({ version }) + '\n');

// The agent skill, so `curl … /skill.md` installs it.
copyFileSync('../skills/glass/SKILL.md', 'public/skill.md');

// Full docs for LLMs and anyone who wants raw markdown.
writeFileSync('public/llms-full.txt', docs);
writeFileSync('public/docs.md', docs);

// llms.txt: the short index format from https://llmstxt.org.
const summary = docs.match(/^> (.+)$/m)[1];
writeFileSync(
  'public/llms.txt',
  `# Glass

> ${summary}

Install on the computer to be controlled: \`curl -fsSL glassdesk.dev | sh\` (macOS 13+ on Apple Silicon or Intel; Linux x86_64 or ARM64 on Wayland, beta). On a Mac the installer adds Glass.app and opens it for a guided setup (Tailscale, the background service, permissions); in a terminal or over SSH it runs \`glass setup\` instead. From outside the tailnet, \`glass link --anywhere\` makes a one-time link that connects any browser directly over WebRTC (end-to-end encrypted, no open ports). At ${SITE}/computers, a passkey account (no email, no password) lists your computers, shows which are online and opens any of them; \`glass account add\` puts a computer there, and the account never lets anyone in by itself. Viewers need only a browser (Safari 26.4+, Chrome, Edge, Firefox). Free and open source (MIT).

Features:
- Picture and speed: about 20 ms, up to 120 fps on fast screens, Display P3 colour, Sharp text (native size, 4:2:2, lossless 4:4:4 once still), every display (live previews, or side by side), the viewing device as a second display that extends the Mac's desktop, fit-this-device resolution, adaptive bitrate.
- Control: touch, trackpad, mouse and keyboard (⌘ shortcuts on iPad, per-machine keyboard profiles, a shortcut bar), trackpad mode on phones (a laptop-style pointer the zoomed view follows; the toolbar folds into menus), Apple Pencil pressure and tilt as Mac tablet input, pointer lock for games, game controllers (Linux), Wake-on-LAN, restart Glass or the computer, unlock from the lock screen (a closed laptop on power too, with \`glass config lid-awake on\`).
- Between devices: your microphone on the computer (Mac: Glass Microphone, our own audio driver, \`glass mic install\`; Linux: PipeWire/PulseAudio), system audio (the browser's media controls and lock screen show and control what the computer is playing), clipboard with rich text and images both ways, files and folders both ways, an owner-only file browser (browse, preview, upload into any folder, rename, Trash), share to the computer from other apps.
- On every device: just a browser; installable app (badge, per-machine shortcuts, share target, picture in picture, window controls overlay, opens offline); notifications even with Glass closed (Web Push, end-to-end encrypted; answer connection requests and agent questions from the notification).
- Helping someone: quick support (\`curl -fsSL glassdesk.dev/help | sh\` on their computer, or \`irm glassdesk.dev/help | iex\` on Windows, shows a code you type at glassdesk.dev/help; nothing stays installed), one-time links and links with limits (view-only, no files, no clipboard, time limits, ask first, passkey required), chat, drawing on the screen, a privacy screen, \`glass link --anywhere\` over WebRTC.
- AI agents: an MCP server (\`glass mcp\`) that lets agents use any of your computers running Glass while you watch, pause, stop and approve them; added to Claude Code, Claude Desktop or Cursor in one click. On a Mac, \`snapshot\` reads an app's window as text (Accessibility) with a ref on each element, and \`press\` and \`set_value\` act on those refs without moving the pointer. Agents from anywhere: Glass is an MCP server over HTTP with OAuth (MCP authorization spec; works as a Vercel Connect connector) at https://<computer>/mcp, published with Tailscale Funnel by \`glass agents public on\`; one-tap approval by number matching, scoped grants enforced on the host, rotating refresh tokens, a hash-chained audit log, \`glass agents\` to list and revoke.
- Apps and admin: a native Mac app (Glass.app) for setup and everything the CLI does, an owner console at \`/admin/\`, single-window streaming, a terminal, forwarded web ports, session recording.
- Teams and security: single sign-on (OpenID Connect), LDAP, SCIM, access rules, device posture checks, a self-hostable relay.

## Docs

- [Full documentation](${SITE}/llms-full.txt): everything in one file: install, setup, features, keyboard behaviour on iPad, CLI reference, architecture, security, troubleshooting, FAQ
- [Documentation (HTML)](${SITE}/docs): the same, as a web page
- [Changelog](${SITE}/changelog.md): what changed in each release
- [Install script](${SITE}/install.sh): what \`curl -fsSL glassdesk.dev | sh\` runs
- [Your computers](${SITE}/computers): sign in with a passkey to see which of your computers are online and open them; \`glass account add\` on a computer adds it
- [Agent skill](${SITE}/skill.md): how an AI agent sets Glass up on any machine in the user's Tailscale tailnet over SSH (\`glass setup --yes --json\`)
- [Source code](https://github.com/Ntarasiuk/glass): Rust host, browser client, and this website (MIT)

## Guides and comparisons

${guides.map((g) => `- [${g.title}](${SITE}${g.path}): ${g.description}`).join('\n')}

## Optional

- [Feedback API](${SITE}/docs.md#feedback): \`POST ${SITE}/api/feedback\` with JSON \`{"message": "…"}\`
`,
);

// The home page's markdown twin, for agents asking / for text/markdown (src/index.js).
writeFileSync('public/index.md', readFileSync('public/llms.txt', 'utf8'));

// FAQ answers as schema.org Questions (AI answers quote them; no Google rich result since 2026-05).
const faqSection = docs.split(/^## FAQ\n/m)[1]?.split(/^## /m)[0] ?? '';
const faq = [...faqSection.matchAll(/\*\*(.+?\?)\*\* (.+?)(?= \*\*|$)/gm)].map(([, q, a]) => ({
  '@type': 'Question',
  name: q,
  acceptedAnswer: { '@type': 'Answer', text: a.replace(/`/g, '').trim() },
}));
if (faq.length < 5) throw new Error(`docs FAQ: parsed only ${faq.length} questions`);

// HTML docs page.
const title = 'Glass documentation: install, setup and remote access';
const docsDescription =
  'How to install and set up Glass, connect from an iPad or any browser, reach your computer from anywhere, and fix problems. CLI reference and FAQ.';
const docsModified = lastModified('docs.md');
const docsLd = {
  '@context': 'https://schema.org',
  '@graph': [
    ORG,
    AUTHOR,
    {
      '@type': 'TechArticle',
      '@id': `${SITE}/docs#article`,
      url: `${SITE}/docs`,
      headline: title,
      description: docsDescription,
      about: { '@id': `${SITE}/#software` },
      author: { '@id': `${SITE}/#author` },
      publisher: { '@id': `${SITE}/#organization` },
      isPartOf: { '@id': `${SITE}/#website` },
      dateModified: docsModified,
      inLanguage: 'en',
    },
    { '@type': 'FAQPage', '@id': `${SITE}/docs#faq`, url: `${SITE}/docs#faq`, mainEntity: faq },
  ],
};
const ogImageAlt = 'Glass: your computer, in any browser. About 20 ms from your Mac or Linux screen to an iPad, phone or laptop.';
const body = marked.parse(docs.replace(/^# Glass\n/, '# Glass documentation\n'));
mkdirSync('public/docs', { recursive: true });
writeFileSync(
  'public/docs/index.html',
  `<!doctype html>
<html lang="en">
<head>
  <!-- Google tag (gtag.js) -->
  <script async src="https://www.googletagmanager.com/gtag/js?id=G-LCYDPMQMLD"></script>
  <script>
    window.dataLayer = window.dataLayer || [];
    function gtag(){dataLayer.push(arguments);}
    gtag('js', new Date());

    gtag('config', 'G-LCYDPMQMLD');
  </script>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>${title}</title>
  <meta name="description" content="${docsDescription}">
  <meta name="robots" content="index, follow, max-image-preview:large, max-snippet:-1">
  <link rel="canonical" href="${SITE}/docs">
  <meta property="og:type" content="article">
  <meta property="article:modified_time" content="${docsModified}">
  <meta property="og:site_name" content="Glass">
  <meta property="og:url" content="${SITE}/docs">
  <meta property="og:title" content="${title}">
  <meta property="og:description" content="${docsDescription}">
  <meta property="og:image" content="${SITE}/og.png">
  <meta property="og:image:width" content="1200">
  <meta property="og:image:height" content="630">
  <meta property="og:image:alt" content="${ogImageAlt}">
  <meta property="og:video" content="${SITE}/og.mp4">
  <meta property="og:video:secure_url" content="${SITE}/og.mp4">
  <meta property="og:video:type" content="video/mp4">
  <meta property="og:video:width" content="1200">
  <meta property="og:video:height" content="630">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="twitter:image" content="${SITE}/og.png">
  <meta name="twitter:image:alt" content="${ogImageAlt}">
  <script type="application/ld+json">${JSON.stringify(docsLd).replace(/</g, '\\u003c')}</script>
  <link rel="alternate" type="text/markdown" href="/docs.md">
  <link rel="icon" href="/icon.svg" type="image/svg+xml">
  <link rel="preload" href="/fonts/bricolage-grotesque-latin.woff2" as="font" type="font/woff2" crossorigin>
  <link rel="preload" href="/fonts/hanken-grotesk-latin.woff2" as="font" type="font/woff2" crossorigin>
  <link rel="stylesheet" href="/glassy/glassy.css">
  <link rel="stylesheet" href="/style.css">
  <script type="speculationrules">{"prefetch":[{"where":{"href_matches":["/","/features","/docs"]},"eagerness":"moderate"}]}</script>
</head>
<body>
  <div class="wrap">
    <header class="site lg-glass-surface" data-lg-preset="header">
      <a class="brand" href="/"><img src="/icon.svg" alt="Glass logo" width="26" height="26">Glass</a>
      <nav class="site" aria-label="Main"><a href="/">Home</a><a href="/features">Features</a><a href="/guides">Guides</a><a href="/docs.md">Markdown</a></nav>
    </header>
    <main class="doc">
<p class="updated">Updated <time datetime="${docsModified}">${human(docsModified)}</time></p>
${body}
    </main>
    ${footer(docsModified, { wrap: false })}
  </div>
  <script src="/site.js" type="module"></script>
</body>
</html>
`,
);

// Home page: the release its SoftwareApplication describes, and the stylesheets inlined so
// first paint needs only the HTML (about 10 KB compressed, one round trip).
const home = finish(readFileSync('index.html', 'utf8'), lastModified('index.html'))
  .replace(/"softwareVersion": "[^"]*"/, `"softwareVersion": "${version}"`)
  .replace('<link rel="stylesheet" href="/glassy/glassy.css">', () => `<style>\n${readFileSync('public/glassy/glassy.css', 'utf8')}</style>`)
  .replace('<link rel="stylesheet" href="/style.css">', () => `<style>\n${readFileSync('public/style.css', 'utf8')}</style>`);
if (!home.includes('<style>')) throw new Error('index.html: stylesheet link not found');
writeFileSync('public/index.html', home);

// Features, quick support and your computers: hand-written pages with the shared placeholders.
for (const [name, dir] of [['features', 'public/features'], ['help', 'public/help'], ['computers', 'public/computers']]) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(`${dir}/index.html`, finish(readFileSync(`pages/${name}.html`, 'utf8'), lastModified(`pages/${name}.html`)));
}

// Guides and comparisons, from guides/*.md: an article each, and an index at /guides.
for (const g of guides) {
  const modified = lastModified(g.src);
  const published = firstCommitted(g.src) ?? modified;
  const faqItems = [...(g.markdown.split(/^## (?:FAQ|Questions)\n/m)[1] ?? '').matchAll(/^### (.+\?)\n\n([\s\S]+?)(?=\n### |\n## |$)/gm)].map(([, q, a]) => ({
    '@type': 'Question',
    name: q,
    acceptedAnswer: { '@type': 'Answer', text: a.replace(/\[([^\]]+)\]\([^)]+\)/g, '$1').replace(/[`*]/g, '').trim() },
  }));
  const graph = [
    ORG,
    AUTHOR,
    {
      '@type': 'Article',
      '@id': `${SITE}${g.path}#article`,
      url: `${SITE}${g.path}`,
      mainEntityOfPage: `${SITE}${g.path}`,
      headline: g.title,
      description: g.description,
      about: { '@id': `${SITE}/#software` },
      author: { '@id': `${SITE}/#author` },
      publisher: { '@id': `${SITE}/#organization` },
      isPartOf: { '@id': `${SITE}/#website` },
      image: `${SITE}/og.png`,
      datePublished: published,
      dateModified: modified,
      inLanguage: 'en',
    },
    {
      '@type': 'BreadcrumbList',
      itemListElement: [
        { '@type': 'ListItem', position: 1, name: 'Glass', item: `${SITE}/` },
        { '@type': 'ListItem', position: 2, name: 'Guides', item: `${SITE}/guides` },
        { '@type': 'ListItem', position: 3, name: g.label, item: `${SITE}${g.path}` },
      ],
    },
    ...(faqItems.length ? [{ '@type': 'FAQPage', '@id': `${SITE}${g.path}#faq`, mainEntity: faqItems }] : []),
  ];
  const parsed = marked.parse(g.markdown);
  // The header holds the title and the short answer side by side; the rest is the article.
  const h1 = parsed.match(/<h1[^>]*>[\s\S]*?<\/h1>\n?/)?.[0] ?? '';
  const afterTitle = parsed.replace(h1, '');
  const intro = afterTitle.match(/^\s*<p>([\s\S]*?)<\/p>\n?/);
  const shortAnswer = intro?.[1].startsWith('<strong>Short answer:</strong>');
  const rest = shortAnswer ? afterTitle.replace(intro[0], '') : afterTitle;
  const answer = shortAnswer
    ? `<div class="g-answer"><span class="g-kicker">Short answer</span><p>${intro[1].replace('<strong>Short answer:</strong>', '').trim().replace(/^((?:<[^>]+>)*)([a-z])/, (_, tags, ch) => tags + ch.toUpperCase())}</p></div>`
    : '';
  const toc = [...rest.matchAll(/<h2 id="([^"]+)">([\s\S]*?)<\/h2>/g)]
    .map(([, id, label]) => `<a href="#${id}">${label.replace(/<[^>]+>/g, '')}</a>`).join('');
  const related = guides.filter((o) => o !== g).map((o) => `<li><a href="${o.path}"><strong>${esc(o.label)}</strong><span>${esc(o.title)}</span></a></li>`).join('');
  writePage(g.path, {
    title: g.title,
    description: g.description,
    modified,
    type: 'article',
    mainClass: 'doc guide',
    graph,
    body: `<header class="g-hero">
<div class="g-title"><nav class="crumbs" aria-label="Breadcrumb"><a href="/">Glass</a> › <a href="/guides">Guides</a></nav>
<p class="updated">Updated <time datetime="${modified}">${human(modified)}</time> · by <a href="https://github.com/Ntarasiuk">Nathan Tarasiuk</a>, who makes Glass</p>
${h1}
<div class="command lg-glass-surface g-install" data-lg-preset="control" data-copy="curl -fsSL glassdesk.dev | sh"><code><span class="prompt" aria-hidden="true">$</span> curl -fsSL glassdesk.dev | sh</code><button type="button" class="copy">Copy</button></div>
<p class="hint g-hint">Run it on the Mac or Linux computer you want to reach. Free and <a href="https://github.com/Ntarasiuk/glass">open source</a>.</p></div>
${answer}
</header>
${toc ? `<nav class="toc" aria-label="On this page"><span>On this page</span>${toc}</nav>` : ''}
<div class="g-body">
${rest}
</div>
<figure class="shot"><img src="/og.png" width="1200" height="630" loading="lazy" alt="Glass: your computer, in any browser. About 20 ms from your Mac or Linux screen to an iPad, phone or laptop. Install with curl -fsSL glassdesk.dev | sh."><figcaption>Glass installs with one line on the Mac or Linux computer; you open it from any browser.</figcaption></figure>
<aside class="about"><h2>How this page was checked</h2><p>Written by <a href="https://github.com/Ntarasiuk">Nathan Tarasiuk</a>, who builds Glass. Every claim about another product links to that product's own page or documentation, checked on ${human(modified)}; Glass's own numbers come from its built-in latency meter on a home network. Products change: if something here is out of date, <a href="/features#feedback">tell us</a> and it gets fixed.</p></aside>
<aside class="related"><h2>More guides</h2><ul>${related}</ul></aside>`,
  });
}
const guidesModified = guides.map((g) => lastModified(g.src)).sort().at(-1) ?? lastModified('build.mjs');
writePage('/guides', {
  title: 'Remote desktop guides and comparisons · Glass',
  description: 'Honest guides to controlling a Mac or Linux computer remotely, and how Glass compares with Jump Desktop, Parsec, RustDesk and Apple Sidecar.',
  modified: guidesModified,
  type: 'website',
  mainClass: 'doc guide guide-index',
  graph: [
    ORG,
    {
      '@type': 'CollectionPage',
      '@id': `${SITE}/guides#page`,
      url: `${SITE}/guides`,
      name: 'Remote desktop guides and comparisons',
      isPartOf: { '@id': `${SITE}/#website` },
      publisher: { '@id': `${SITE}/#organization` },
      dateModified: guidesModified,
      hasPart: guides.map((g) => ({ '@id': `${SITE}${g.path}#article` })),
    },
  ],
  body: `<h1>Guides and comparisons</h1>
<p class="lede">Glass is a free, open-source remote desktop for Mac and Linux, used from any browser. These pages answer the questions people ask before choosing one, including when another tool fits better.</p>
<ul class="guide-list">${guides.map((g) => `<li><a href="${g.path}"><strong>${esc(g.title)}</strong><span>${esc(g.description)}</span></a></li>`).join('')}</ul>`,
});

/** A page in the docs' layout: head, header, body, shared footer. */
function writePage(path, { title, description, modified, type, graph, body: content, mainClass = 'doc' }) {
  const url = `${SITE}${path}`;
  mkdirSync(`public${path}`, { recursive: true });
  writeFileSync(
    `public${path}/index.html`,
    `<!doctype html>
<html lang="en">
<head>
  <!-- Google tag (gtag.js) -->
  <script async src="https://www.googletagmanager.com/gtag/js?id=G-LCYDPMQMLD"></script>
  <script>
    window.dataLayer = window.dataLayer || [];
    function gtag(){dataLayer.push(arguments);}
    gtag('js', new Date());

    gtag('config', 'G-LCYDPMQMLD');
  </script>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>${esc(title)}</title>
  <meta name="description" content="${esc(description)}">
  <meta name="robots" content="index, follow, max-image-preview:large, max-snippet:-1">
  <link rel="canonical" href="${url}">
  <meta property="og:type" content="${type}">
  <meta property="article:modified_time" content="${modified}">
  <meta property="og:site_name" content="Glass">
  <meta property="og:url" content="${url}">
  <meta property="og:title" content="${esc(title)}">
  <meta property="og:description" content="${esc(description)}">
  <meta property="og:image" content="${SITE}/og.png">
  <meta property="og:image:width" content="1200">
  <meta property="og:image:height" content="630">
  <meta property="og:image:alt" content="${ogImageAlt}">
  <meta name="twitter:card" content="summary_large_image">
  <meta name="twitter:image" content="${SITE}/og.png">
  ${ld(graph)}
  <link rel="icon" href="/icon.svg" type="image/svg+xml">
  <link rel="preload" href="/fonts/bricolage-grotesque-latin.woff2" as="font" type="font/woff2" crossorigin>
  <link rel="preload" href="/fonts/hanken-grotesk-latin.woff2" as="font" type="font/woff2" crossorigin>
  <link rel="stylesheet" href="/glassy/glassy.css">
  <link rel="stylesheet" href="/style.css">
</head>
<body>
  <div class="wrap">
    <header class="site lg-glass-surface" data-lg-preset="header">
      <a class="brand" href="/"><img src="/icon.svg" alt="Glass logo" width="26" height="26">Glass</a>
      <nav class="site" aria-label="Main"><a href="/">Home</a><a href="/features">Features</a><a href="/docs">Docs</a><a href="https://github.com/Ntarasiuk/glass">GitHub</a></nav>
    </header>
    <main class="${mainClass}">
${content}
<div class="command lg-glass-surface cta" data-lg-preset="control" data-copy="curl -fsSL glassdesk.dev | sh"><code><span class="prompt" aria-hidden="true">$</span> curl -fsSL glassdesk.dev | sh</code><button type="button" class="copy">Copy</button></div>
<p class="hint">Try Glass: run this on the Mac or Linux computer you want to reach. Free and <a href="https://github.com/Ntarasiuk/glass">open source</a>.</p>
    </main>
    ${footer(modified, { wrap: false })}
  </div>
  <script src="/site.js" type="module"></script>
</body>
</html>
`,
  );
}

// Changelog: ../CHANGELOG.md, as a page and as markdown for agents.
const changelog = readFileSync('../CHANGELOG.md', 'utf8');
const changelogModified = lastModified('../CHANGELOG.md');
writeFileSync('public/changelog.md', changelog);
writePage('/changelog', {
  title: 'Glass changelog: what changed in each release',
  description: 'What changed in each release of Glass, the free, open-source remote desktop for Mac and Linux.',
  modified: changelogModified,
  type: 'article',
  graph: [{ '@type': 'WebPage', '@id': `${SITE}/changelog`, url: `${SITE}/changelog`, name: 'Glass changelog', dateModified: changelogModified, inLanguage: 'en' }],
  body: `<p class="updated">Updated <time datetime="${changelogModified}">${human(changelogModified)}</time> · <a href="/changelog.md">Markdown</a></p>\n${marked.parse(changelog)}`,
});

// Sitemap: the HTML pages, each dated by its source's last commit.
const pages = [
  ['/', 'index.html'],
  ['/features', 'pages/features.html'],
  ['/help', 'pages/help.html'],
  ['/docs', 'docs.md'],
  ['/changelog', '../CHANGELOG.md'],
  ...guides.map((g) => [g.path, g.src]),
];
writeFileSync(
  'public/sitemap.xml',
  `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${pages.map(([path, src]) => `  <url><loc>${SITE}${path}</loc><lastmod>${lastModified(src)}</lastmod></url>`).join('\n')}
  <url><loc>${SITE}/guides</loc><lastmod>${guidesModified}</lastmod></url>
</urlset>
`,
);

// The web client, for remote access from anywhere (glassdesk.dev/app/#r=…).
rmSync('public/app', { recursive: true, force: true });
cpSync('../web', 'public/app', { recursive: true });

// Releases: ../dist (scripts/package.sh, or the signed CI build fetched from GitHub when dist
// lacks this version). "latest" is always this Cargo.toml version, so the installer can't
// serve an older build than version.json announces (hosts would offer the update forever).
mkdirSync('public/releases', { recursive: true });
const dist = '../dist';
const PLATFORMS = ['darwin-universal', 'linux-x86_64', 'linux-aarch64'];
// Windows ships zips (the installer's), and MSIs for managed installs; optional until
// releases include them.
const WINDOWS = ['windows-x86_64', 'windows-arm64'];
const archives = (p) => (p.startsWith('windows-') ? ['zip', 'msi'] : ['tar.gz']);
const missing = () => PLATFORMS.some((p) => !existsSync(`${dist}/glass-${version}-${p}.tar.gz`));
if (missing()) {
  try {
    execFileSync('gh', ['release', 'download', `v${version}`, '-R', 'Ntarasiuk/glass', '-D', dist, '-p', `glass-${version}-*`, '--clobber'], { stdio: 'inherit' });
  } catch {}
}
// A deploy (npm run deploy) must ship this version; tests and local builds can do without.
const why = `no ${version} release in ${dist}: run scripts/package.sh, or wait for the v${version} release workflow`;
if (missing() && process.env.GLASS_DEPLOY) throw new Error(why);
if (missing()) console.warn(`warning: ${why}`);
// Cloudflare serves assets up to 25 MiB; bigger tarballs stay on the GitHub release, and the
// Worker redirects to them (src/index.js). Their checksums are still served from here.
const ASSET_LIMIT = 25 * 1024 * 1024;
const small = (f) => statSync(f).size < ASSET_LIMIT;
if (existsSync(dist)) {
  for (const f of readdirSync(dist, { withFileTypes: true }).filter((e) => e.isFile() && !e.name.startsWith('glass-latest-')).map((e) => e.name)) {
    if (small(`${dist}/${f}`)) copyFileSync(`${dist}/${f}`, `public/releases/${f}`);
    else rmSync(`public/releases/${f}`, { force: true });
  }
}
// The installer verifies each platform's "latest" tarball against this checksum.
for (const p of [...PLATFORMS, ...WINDOWS]) {
  for (const ext of archives(p)) {
    const release = `${dist}/glass-${version}-${p}.${ext}`;
    const f = `glass-latest-${p}.${ext}`;
    if (!existsSync(release)) continue;
    if (small(release)) copyFileSync(release, `public/releases/${f}`);
    else rmSync(`public/releases/${f}`, { force: true });
    // Hash the file itself: with several versions around, any lookup by name can pick a stale one.
    const sum = createHash('sha256').update(readFileSync(release)).digest('hex');
    writeFileSync(`public/releases/${f}.sha256`, `${sum}  ${f}\n`);
  }
}

console.log('built: index.html, features/, help/, computers/, guides/, vs/, changelog/, changelog.md, skill.md, llms.txt, llms-full.txt, docs.md, docs/index.html, index.md, sitemap.xml, app/, releases/');

/** YYYY-MM-DD of the first commit adding a file, if it's committed. */
function firstCommitted(file) {
  try {
    return execFileSync('git', ['log', '--diff-filter=A', '--follow', '--format=%cs', '--', file], { encoding: 'utf8' }).trim().split('\n').at(-1) || null;
  } catch {
    return null;
  }
}

/** YYYY-MM-DD of the last commit touching a file (today when uncommitted). */
function lastModified(file) {
  try {
    const date = execFileSync('git', ['log', '-1', '--format=%cs', '--', file], { encoding: 'utf8' }).trim();
    const dirty = execFileSync('git', ['status', '--porcelain', '--', file], { encoding: 'utf8' }).trim();
    if (date && !dirty) return date;
  } catch {}
  return new Date().toISOString().slice(0, 10);
}
