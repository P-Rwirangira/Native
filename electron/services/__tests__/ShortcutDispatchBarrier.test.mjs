import { test } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const modulePath = path.resolve(
  __dirname,
  '../../../dist-electron/electron/services/shortcutDispatchBarrier.js'
);

async function load() {
  return import(pathToFileURL(modulePath).href);
}

test('suppresses the second delivery of one physical shortcut across sources', async () => {
  const { ShortcutDispatchBarrier } = await load();
  let now = 1_000;
  const barrier = new ShortcutDispatchBarrier(100, () => now);

  assert.equal(barrier.accept('general:toggle-visibility', 'native-hook'), true);
  now += 8;
  assert.equal(barrier.accept('general:toggle-visibility', 'register-hotkey'), false);
});

test('preserves same-source key repeat', async () => {
  const { ShortcutDispatchBarrier } = await load();
  let now = 1_000;
  const barrier = new ShortcutDispatchBarrier(100, () => now);

  assert.equal(barrier.accept('chat:scrollDown', 'register-hotkey'), true);
  now += 25;
  assert.equal(barrier.accept('chat:scrollDown', 'register-hotkey'), true);
});

test('accepts a different source after the duplicate window', async () => {
  const { ShortcutDispatchBarrier } = await load();
  let now = 1_000;
  const barrier = new ShortcutDispatchBarrier(100, () => now);

  assert.equal(barrier.accept('general:toggle-visibility', 'native-hook'), true);
  now += 100;
  assert.equal(barrier.accept('general:toggle-visibility', 'register-hotkey'), true);
});

test('tracks actions independently and recovers from a clock epoch reset', async () => {
  const { ShortcutDispatchBarrier } = await load();
  let now = 1_000;
  const barrier = new ShortcutDispatchBarrier(100, () => now);

  assert.equal(barrier.accept('general:toggle-visibility', 'native-hook'), true);
  now += 5;
  assert.equal(barrier.accept('general:toggle-mouse-passthrough', 'register-hotkey'), true);
  now = 10;
  assert.equal(barrier.accept('general:toggle-visibility', 'register-hotkey'), true);
});

test('clear starts a fresh dispatch epoch', async () => {
  const { ShortcutDispatchBarrier } = await load();
  let now = 1_000;
  const barrier = new ShortcutDispatchBarrier(100, () => now);

  assert.equal(barrier.accept('general:toggle-visibility', 'native-hook'), true);
  barrier.clear();
  now += 1;
  assert.equal(barrier.accept('general:toggle-visibility', 'register-hotkey'), true);
});
