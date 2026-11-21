// Browser smoke test for the web control app. Starts a real paneld on
// 127.0.0.1:18090 (config: smoke.toml), drives /app with Chromium, and checks
// the effects through /api/state. Needs no root, hardware or network access.
//
//   PANELD_BIN=target/debug/paneld node tests/web/smoke.mjs
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { chromium } from 'playwright';

const here = path.dirname(fileURLToPath(import.meta.url));
const bin = process.env.PANELD_BIN || 'target/debug/paneld';
const base = 'http://127.0.0.1:18090';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const getState = async () => (await fetch(base + '/api/state')).json();

async function waitFor(what, fn, timeoutMs = 10000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (_) { /* not ready yet */ }
    if (Date.now() > end) throw new Error('timed out waiting for ' + what);
    await sleep(100);
  }
}

const daemon = spawn(bin, ['run', '--config', path.join(here, 'smoke.toml')], {
  stdio: ['ignore', 'inherit', 'inherit'],
});
let exited = false;
daemon.on('exit', () => { exited = true; });

let browser;
let failed = false;
try {
  await waitFor('paneld to answer', async () => {
    if (exited) throw new Error('paneld exited early');
    return (await fetch(base + '/api/state')).ok;
  });

  browser = await chromium.launch();
  const page = await browser.newPage();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push('console: ' + m.text()); });
  page.on('pageerror', (e) => problems.push('pageerror: ' + e.message));

  await page.goto(base + '/app');
  await page.getByRole('heading', { name: 'Status' }).waitFor();

  // Scenes rendered: Auto, a, b.
  const picker = page.getByRole('group', { name: 'Scene picker' });
  await waitFor('scene buttons', async () => (await picker.getByRole('button').count()) === 3);

  // Pin scene b, then release with Auto.
  await picker.getByRole('button', { name: 'b', exact: true }).click();
  await waitFor('pinned b', async () => (await getState()).pinned === 'b');
  await picker.getByRole('button', { name: 'Auto' }).click();
  await waitFor('pin released', async () => (await getState()).pinned === null);

  // Notification.
  await page.getByRole('textbox', { name: 'Text', exact: true }).fill('smoke test');
  await page.getByRole('button', { name: 'Send' }).click();
  await waitFor('notification queued', async () =>
    (await getState()).notifications.some((n) => n.summary === 'smoke test'));
  await page.getByRole('button', { name: 'Clear all' }).click();
  await waitFor('notifications cleared', async () => (await getState()).notifications.length === 0);

  // Power off and on.
  const power = page.getByLabel('Panel on');
  await power.uncheck();
  await waitFor('power off', async () => (await getState()).on === false);
  await power.check();
  await waitFor('power on', async () => (await getState()).on === true);

  // Push a value, then remove it. Failures show as an error toast.
  await page.getByLabel('Key').fill('push:smoke');
  await page.getByLabel('Value', { exact: true }).fill('42');
  await page.getByRole('button', { name: 'Push', exact: true }).click();
  await page.getByText('Pushed push:smoke').waitFor();
  await page.getByRole('button', { name: 'Remove' }).click();
  await page.getByText('Removed push:smoke').waitFor();

  // The preview image loaded (when the backend reports one).
  await waitFor('preview image', () =>
    page.locator('#preview').evaluate((i) => i.complete && i.naturalWidth > 0));

  if (problems.length) throw new Error('browser problems:\n' + problems.join('\n'));
  console.log('web smoke test passed');
} catch (e) {
  failed = true;
  console.error('web smoke test FAILED:', e.message);
} finally {
  if (browser) await browser.close().catch(() => {});
  if (!exited) daemon.kill('SIGTERM');
  await sleep(200);
  if (!exited) daemon.kill('SIGKILL');
}
process.exit(failed ? 1 : 0);
