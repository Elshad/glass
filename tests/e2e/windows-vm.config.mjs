import { defineConfig } from '@playwright/test';

// The Windows host's basics, run against a real Windows computer (a VM) over SSH, not a
// `--test` host: see windows-vm/README.md. Not part of the normal run or CI.
//   npx playwright test -c windows-vm.config.mjs
export default defineConfig({
  testDir: './windows-vm',
  testMatch: '*.vm.mjs',
  timeout: 120_000,
  // One computer, one desktop: the tests take turns.
  workers: 1,
  fullyParallel: false,
  reporter: 'list',
  use: {
    channel: 'chrome',
    headless: true,
    viewport: { width: 1280, height: 900 },
    permissions: ['clipboard-read', 'clipboard-write', 'microphone'],
    // A fake microphone (a tone) for the viewer's mic.
    launchOptions: { args: ['--autoplay-policy=no-user-gesture-required', '--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'] },
    trace: 'retain-on-failure',
  },
});
