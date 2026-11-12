// Tailnet identities: the owner's own devices, people the access rules name, and (with
// tailnet-members) anyone on the tailnet connect without the token. Test mode pretends every
// viewer is GLASS_TEST_TAILNET_LOGIN on Tailscale, on a computer GLASS_TEST_TAILNET_OWNER owns.

import { Host, expectVideo, test, expect } from './harness.mjs';

const OWNER = 'owner@example.com';

async function tailnetHost(login, setup = () => {}) {
  const host = new Host({ env: { GLASS_TEST_TAILNET_LOGIN: login, GLASS_TEST_TAILNET_OWNER: OWNER } });
  setup(host);
  return host.start();
}

test("the owner's own devices connect as the owner", async ({ page }) => {
  const host = await tailnetHost(OWNER);
  try {
    await page.goto(host.url({ token: false }));
    await expectVideo(page);
    await expect(page.locator('#btn-files')).toBeVisible();
  } finally {
    await host.stop();
  }
});

test('someone else on the tailnet is asked for the token', async ({ page }) => {
  const host = await tailnetHost('friend@example.com');
  try {
    await page.goto(host.url({ token: false }));
    await expect(page.locator('#auth')).toBeVisible();
  } finally {
    await host.stop();
  }
});

test('an access rule lets them in as a guest, with its limits', async ({ page }) => {
  const host = await tailnetHost('friend@example.com', (h) => h.cli('acl', 'add', 'friend@example.com', '--view-only'));
  try {
    await page.goto(host.url({ token: false }));
    await expectVideo(page);
    // A guest: no file browser; view only: no keyboard.
    await expect(page.locator('#btn-files')).toBeHidden();
    await expect(page.locator('#btn-keyboard')).toBeHidden();
  } finally {
    await host.stop();
  }
});

test('tailnet-members lets anyone on the tailnet in as a guest', async ({ page }) => {
  const host = await tailnetHost('someone@else.example', (h) => h.cli('config', 'tailnet-members', 'on'));
  try {
    await page.goto(host.url({ token: false }));
    await expectVideo(page);
    await expect(page.locator('#btn-files')).toBeHidden();
  } finally {
    await host.stop();
  }
});
