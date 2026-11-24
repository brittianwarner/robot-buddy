// Shared setup for the opt-in live lab scripts: an isolated test server and a
// provisioned test identity only. Never point these at a shared or production server.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

/** Reads the required environment, or fails with the variables to set. */
export function labEnv() {
  const url = process.env.BUDDY_TEST_SERVER_URL;
  const token = process.env.BUDDY_LAB_TOKEN;
  const identity = process.env.BUDDY_TEST_IDENTITY_FILE;
  assert(url && token && identity, 'Set BUDDY_TEST_SERVER_URL, BUDDY_LAB_TOKEN, BUDDY_TEST_IDENTITY_FILE');
  return { url, token, identity };
}

/** POSTs a lab action with the session's origin and CSRF token; returns the response. */
export function labAction({ context, url, csrf }) {
  return (target, command) =>
    context.request.post(url + '/lab/api/action', {
      headers: { origin: url, 'x-buddy-csrf': csrf },
      data: { target, command },
    });
}

/**
 * Logs in to the lab, checks origin/CSRF enforcement and the command allowlist, enrolls the
 * test identity, and checks that a second enroll reconnects the same bot.
 * Returns the browser (close it in `finally`), page, collected page errors and the bot id.
 */
export async function openLab({ viewport = { width: 1440, height: 1000 } } = {}) {
  const { url, token, identity } = labEnv();
  const browser = await chromium.launch({ headless: true });
  try {
    const context = await browser.newContext({ viewport });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    assert.equal((await context.request.get(url + '/lab/api/state')).status(), 401);
    const foreign = await context.request.post(url + '/lab/api/login', {
      headers: { origin: 'https://attacker.invalid' },
      data: { token },
    });
    assert.equal(foreign.status(), 403);
    await page.goto(url + '/lab');
    await page.locator('#operator-token').fill(token);
    await page.locator('#login-form button').click();
    await page.locator('#workspace').waitFor({ state: 'visible' });
    const csrf = await page.evaluate(async () => (await (await fetch('/lab/api/state')).json()).csrf);
    const noCsrf = await context.request.post(url + '/lab/api/action', {
      headers: { origin: url },
      data: { target: 'bot', command: { type: 'revoke' } },
    });
    assert.equal(noCsrf.status(), 403);
    await page.locator('#identity-file').setInputFiles(identity);
    await page.locator('#enroll').click();
    await page.waitForFunction(() => document.querySelector('#bot-id').textContent !== '—');
    const bot = await page.locator('#bot-id').textContent();
    const action = labAction({ context, url, csrf });
    assert.equal((await action('bot', { type: 'revoke' })).status(), 403, 'revoke is not on the lab allowlist');
    await page.locator('#enroll').click();
    await page.waitForTimeout(1000);
    assert.equal(await page.locator('#bot-id').textContent(), bot, 'reconnect keeps the same bot');
    return { browser, context, page, errors, csrf, bot, url, action };
  } catch (error) {
    await browser.close();
    throw error;
  }
}
