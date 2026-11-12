// Fills in the winget manifests (packaging/winget) for a release: its version, the MSIs'
// URLs on the GitHub release and their SHA-256.
//   node scripts/winget.mjs 0.11.0 [dist] [out]
// Checksums come from dist/glass-<version>-windows-<arch>.msi.sha256 when there, else from
// the release itself. Writes out/manifests/g/Glassdesk/Glass/<version>/ (default
// dist/winget), ready to check with `winget validate` and submit to microsoft/winget-pkgs
// by hand (packaging/winget/README.md). It submits nothing.
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(import.meta.dirname, '..');
const TEMPLATES = join(ROOT, 'packaging/winget/manifests/g/Glassdesk/Glass');
const RELEASES = 'https://github.com/Ntarasiuk/glass/releases';
/** winget's architecture names for ours. */
export const ARCHES = { x64: 'x86_64', arm64: 'arm64' };

export const msiName = (version, arch) => `glass-${version}-windows-${arch}.msi`;
export const msiUrl = (version, arch) => `${RELEASES}/download/v${version}/${msiName(version, arch)}`;

/**
 * One manifest with this release's details. `sums` maps winget architectures (x64, arm64)
 * to SHA-256s; each installer's checksum follows its URL.
 */
export function fill(text, { version, sums, date }) {
  let arch = null;
  return text
    .split('\n')
    .map((line) => {
      const [, indent, key] = line.match(/^(\s*-?\s*)(\w+):/) ?? [];
      if (key === 'Architecture') arch = line.split(':')[1].trim();
      if (key === 'PackageVersion') return `${indent}PackageVersion: ${version}`;
      if (key === 'ReleaseDate') return `${indent}ReleaseDate: ${date}`;
      if (key === 'ReleaseNotesUrl') return `${indent}ReleaseNotesUrl: ${RELEASES}/tag/v${version}`;
      if (key === 'InstallerUrl') return `${indent}InstallerUrl: ${msiUrl(version, ARCHES[arch])}`;
      if (key === 'InstallerSha256') {
        const sum = sums[arch];
        if (!/^[0-9a-f]{64}$/i.test(sum ?? '')) throw new Error(`no SHA-256 for ${arch}`);
        return `${indent}InstallerSha256: ${sum.toUpperCase()}`;
      }
      return line;
    })
    .join('\n');
}

/** The first field of a sha256sum line. */
export const parseSum = (text) => text.trim().split(/\s+/)[0].toLowerCase();

async function sums(version, dist) {
  const out = {};
  for (const [arch, ours] of Object.entries(ARCHES)) {
    const local = join(dist, `${msiName(version, ours)}.sha256`);
    if (existsSync(local)) {
      out[arch] = parseSum(readFileSync(local, 'utf8'));
      continue;
    }
    const res = await fetch(`${msiUrl(version, ours)}.sha256`);
    if (!res.ok) throw new Error(`${msiName(version, ours)}.sha256: ${res.status}`);
    out[arch] = parseSum(await res.text());
  }
  return out;
}

async function main([version, dist = 'dist', out = join(dist, 'winget')]) {
  if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error('usage: node scripts/winget.mjs <version, like 0.11.0> [dist] [out]');
  const details = { version, sums: await sums(version, dist), date: new Date().toISOString().slice(0, 10) };
  const dir = join(out, 'manifests/g/Glassdesk/Glass', version);
  mkdirSync(dir, { recursive: true });
  for (const f of readdirSync(TEMPLATES).filter((f) => f.endsWith('.yaml'))) {
    const text = fill(readFileSync(join(TEMPLATES, f), 'utf8'), details).replace(/^# A template: .*\n/m, '');
    writeFileSync(join(dir, f), text);
  }
  console.log(`winget manifests for ${version}: ${dir}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((e) => {
    console.error(`error: ${e.message}`);
    process.exit(1);
  });
}
