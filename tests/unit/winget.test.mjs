import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fill, parseSum, msiUrl } from '../../scripts/winget.mjs';

const installer = readFileSync(new URL('../../packaging/winget/manifests/g/Glassdesk/Glass/Glassdesk.Glass.installer.yaml', import.meta.url), 'utf8');
const locale = readFileSync(new URL('../../packaging/winget/manifests/g/Glassdesk/Glass/Glassdesk.Glass.locale.en-US.yaml', import.meta.url), 'utf8');
const sums = { x64: 'a'.repeat(64), arm64: 'b'.repeat(64) };

test('each installer gets its own URL and checksum', () => {
  const out = fill(installer, { version: '1.2.3', sums, date: '2026-11-01' });
  assert.match(out, /^PackageVersion: 1\.2\.3$/m);
  assert.match(out, /^ReleaseDate: 2026-11-01$/m);
  const x64 = out.indexOf('Architecture: x64');
  const arm = out.indexOf('Architecture: arm64');
  assert.ok(x64 > 0 && arm > x64);
  const block = (from, to) => out.slice(from, to);
  assert.ok(block(x64, arm).includes(`InstallerUrl: ${msiUrl('1.2.3', 'x86_64')}`));
  assert.ok(block(x64, arm).includes(`InstallerSha256: ${'A'.repeat(64)}`));
  assert.ok(block(arm).includes(`InstallerUrl: ${msiUrl('1.2.3', 'arm64')}`));
  assert.ok(block(arm).includes(`InstallerSha256: ${'B'.repeat(64)}`));
  assert.equal(msiUrl('1.2.3', 'arm64'), 'https://github.com/Ntarasiuk/glass/releases/download/v1.2.3/glass-1.2.3-windows-arm64.msi');
  // Everything else stays as it was.
  assert.equal(out.split('\n').length, installer.split('\n').length);
  assert.match(out, /^InstallerType: msi$/m);
  assert.match(out, /UpgradeCode: '\{83EBD5C2-954D-4113-A3C1-4797653E5386\}'/);
});

test('release notes point at the release; a missing checksum is an error', () => {
  const out = fill(locale, { version: '1.2.3', sums, date: '2026-11-01' });
  assert.match(out, /^ReleaseNotesUrl: https:\/\/github\.com\/Ntarasiuk\/glass\/releases\/tag\/v1\.2\.3$/m);
  assert.throws(() => fill(installer, { version: '1.2.3', sums: { x64: sums.x64 }, date: '2026-11-01' }), /arm64/);
  assert.equal(parseSum('ABCDEF  glass-1.2.3-windows-x86_64.msi\n'), 'abcdef');
});

test('the MSI keeps the UpgradeCode the manifest names', () => {
  const wxs = readFileSync(new URL('../../packaging/windows/glass.wxs', import.meta.url), 'utf8');
  assert.match(wxs, /UpgradeCode="83EBD5C2-954D-4113-A3C1-4797653E5386"/);
});
