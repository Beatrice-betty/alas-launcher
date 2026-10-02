const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/splash-progress.js'), 'utf8');
const ids = [
  'badge', 'badge-text', 'spinner', 'error-dot', 'progress-fill', 'progress-pct',
  'progress-head', 'uv-progress-container', 'uv-progress-fill', 'uv-progress-pct',
  'uv-progress-detail', 'progress-meta', 'splash-actions', 'tip-text', 'title', 'detail',
];

function harness({ reducedMotion = false, readyPayload = null, invokeError = null } = {}) {
  let now = 0;
  let nextFrame = 1;
  let width = 400;
  let domWrites = 0;
  let domQueries = 0;
  let layoutReads = 0;
  const frames = new Map();
  const windowEvents = new Map();
  const documentEvents = new Map();
  const motionEvents = new Map();
  const errors = [];
  const calls = [];
  const element = () => {
    const classes = new Set();
    const attributes = new Map();
    let content = '';
    let name = '';
    return {
      style: new Proxy({}, {
        set(target, property, value) { domWrites++; target[property] = value; return true; },
      }),
      get textContent() { return content; },
      set textContent(value) { domWrites++; content = value; },
      get className() { return name; },
      set className(value) { domWrites++; name = value; },
      classList: {
        toggle(value, enabled) {
          if (classes.has(value) === enabled) return;
          domWrites++;
          if (enabled) classes.add(value); else classes.delete(value);
        },
        contains: value => classes.has(value),
      },
      setAttribute(name, value) { domWrites++; attributes.set(name, value); },
      getAttribute: name => attributes.get(name),
    };
  };
  const elements = Object.fromEntries(ids.map(id => [id, element()]));
  const track = { getBoundingClientRect: () => { layoutReads++; return { width }; } };
  elements['progress-fill'].parentElement = track;
  const document = {
    hidden: false,
    body: element(),
    getElementById(id) { domQueries++; return elements[id]; },
    addEventListener: (name, handler) => documentEvents.set(name, handler),
  };
  const media = {
    matches: reducedMotion,
    addEventListener: (name, handler) => motionEvents.set(name, handler),
  };
  const window = {
    matchMedia: () => media,
    addEventListener: (name, handler) => windowEvents.set(name, handler),
    requestAnimationFrame(handler) { const id = nextFrame++; frames.set(id, handler); return id; },
    cancelAnimationFrame: id => frames.delete(id),
  };
  const context = {
    window, document, performance: { now: () => now },
    console: { error: (...args) => errors.push(args) },
    i18n: {
      starting: 'Starting', errorBadge: 'Error', initStopped: 'Stopped', progressMetaReady: 'Ready',
    },
    splitSubtitle: value => ({ status: String(value || 'Loading'), tip: 'Tip' }),
    normalizeDetail: value => String(value || 'Initializing'),
    invoke: async name => {
      calls.push(name);
      if (invokeError) throw invokeError;
      return readyPayload;
    },
  };
  vm.runInNewContext(source, context, { filename: 'splash-progress.js' });
  const payload = (sequence, progress, rest = {}) => ({
    sequence, progress, title: 'Starting', detail: 'Preparing', subtitle: 'Loading',
    is_error: false, uv_progress: null, ...rest,
  });
  return {
    elements, window, document, errors, calls,
    send: (sequence, progress, rest) => window.__ALAS_SPLASH_UPDATE(payload(sequence, progress, rest)),
    tick(time) {
      now = time;
      const pending = [...frames.values()];
      frames.clear();
      for (const callback of pending) callback(time);
    },
    setNow: time => { now = time; },
    resize(value) { width = value; windowEvents.get('resize')(); },
    visibility(hidden) { document.hidden = hidden; documentEvents.get('visibilitychange')(); },
    motion(enabled) { media.matches = enabled; motionEvents.get('change')(); },
    get frames() { return frames.size; },
    get domWrites() { return domWrites; },
    get domQueries() { return domQueries; },
    get layoutReads() { return layoutReads; },
    fill: id => Number(elements[id || 'progress-fill'].style.transform.match(/^scaleX\((.*?)\)$/)[1]) * 100,
  };
}

test('linear interpolation keeps fill, percentage and decoration synchronized', () => {
  const view = harness();
  view.send(1, 60);
  assert.equal(view.fill(), 0);
  assert.equal(view.frames, 1);
  view.tick(125);
  assert.equal(view.fill(), 30);
  assert.equal(view.elements['progress-pct'].textContent, '30%');
  assert.equal(view.elements['progress-fill'].getAttribute('aria-valuenow'), '30');
  assert.equal(view.elements['progress-head'].style.transform, 'translate3d(120px, -80%, 0) translateX(-50%)');
  view.tick(250);
  assert.equal(view.fill(), 60);
  assert.equal(view.frames, 0);
  assert.equal(view.layoutReads, 1, 'animation frames do not measure layout');
  assert.equal(view.domQueries, ids.length, 'DOM nodes are cached at setup');
});

test('stale and duplicate snapshots cannot rewind progress or overwrite status', () => {
  const view = harness({ reducedMotion: true });
  view.send(3, 70, { title: 'Newest' });
  const writes = view.domWrites;
  view.send(2, 20, { title: 'Stale' });
  view.send(3, 10, { title: 'Duplicate' });
  assert.equal(view.fill(), 70);
  assert.equal(view.elements.title.textContent, 'Newest');
  assert.equal(view.domWrites, writes);
});

test('retry reports retain the main and continuously visible uv high-water marks', () => {
  const view = harness({ reducedMotion: true });
  view.send(1, 80, { uv_progress: { progress: 65, detail: 'Download' } });
  view.send(2, 30, { uv_progress: { progress: 10, detail: 'Retrying' } });
  assert.equal(view.fill(), 80);
  assert.equal(view.fill('uv-progress-fill'), 65);
  assert.equal(view.elements['uv-progress-detail'].textContent, 'Retrying');
  view.send(3, 20);
  assert.equal(view.elements['uv-progress-container'].getAttribute('aria-hidden'), 'true');
  assert.equal(view.fill('uv-progress-fill'), 0);
  view.send(4, 25, { uv_progress: { progress: 5, detail: 'New task' } });
  assert.equal(view.fill(), 80);
  assert.equal(view.fill('uv-progress-fill'), 5);
});

test('retargeting starts at the rendered value and repeated status updates keep the deadline', () => {
  const view = harness();
  view.send(1, 40);
  view.tick(100);
  assert.equal(view.fill(), 16);
  view.send(2, 80);
  assert.equal(view.fill(), 16);
  view.tick(200);
  assert.ok(Math.abs(view.fill() - 41.6) < 0.000001);
  view.send(3, 80, { detail: 'Status only' });
  view.tick(350);
  assert.equal(view.fill(), 80);
  assert.equal(view.frames, 0);
});

test('frame timestamps cannot rewind a value advanced while receiving a snapshot', () => {
  const view = harness();
  view.send(1, 80);
  view.setNow(100);
  view.send(2, 80, { detail: 'Status only' });
  assert.equal(view.fill(), 32);
  view.tick(99);
  assert.equal(view.fill(), 32);
  view.tick(250);
  assert.equal(view.fill(), 80);
});

test('completion and explicit terminal snapshots settle immediately while only terminal freezes updates', () => {
  for (const ending of [{ progress: 100 }, { progress: 50, terminal: true }]) {
    const view = harness();
    view.send(1, 40, { uv_progress: { progress: 60, detail: 'Download' } });
    view.tick(50);
    view.send(2, ending.progress, ending);
    assert.equal(view.fill(), ending.progress);
    assert.equal(view.frames, 0);
    assert.equal(view.elements['uv-progress-container'].getAttribute('aria-hidden'), 'true');
    view.send(3, 10, { title: 'Late callback' });
    assert.equal(view.fill(), ending.progress);
    assert.equal(view.elements.title.textContent, ending.terminal ? 'Starting' : 'Late callback');
  }
});

test('an ordinary error after completion remains visible and immediately settles at 100 percent', () => {
  const view = harness();
  view.send(1, 100, { title: 'Restarting' });
  view.send(2, 100, { is_error: true, title: 'Update replacement failed' });
  assert.equal(view.fill(), 100);
  assert.equal(view.elements.title.textContent, 'Update replacement failed');
  assert.equal(view.elements['badge-text'].textContent, 'Error');
  assert.equal(view.document.body.classList.contains('error-state'), true);
  assert.equal(view.frames, 0);
});

test('a visible new uv task resets its local progress after the previous task completed', () => {
  const view = harness({ reducedMotion: true });
  view.send(1, 60, { uv_progress: { task_id: 1, progress: 100, detail: 'Completed' } });
  assert.equal(view.fill('uv-progress-fill'), 100);
  view.send(2, 60, { uv_progress: { task_id: 2, progress: 2, detail: 'Resolving again' } });
  assert.equal(view.fill('uv-progress-fill'), 2);
  assert.equal(view.elements['uv-progress-container'].getAttribute('aria-hidden'), 'false');
  assert.equal(view.elements['uv-progress-detail'].textContent, 'Resolving again');
  view.send(3, 60, { uv_progress: { task_id: 2, progress: 1, detail: 'Same task' } });
  assert.equal(view.fill('uv-progress-fill'), 2);
});

test('malformed uv task IDs cannot reset an active task', () => {
  const view = harness({ reducedMotion: true });
  view.send(1, 60, { uv_progress: { task_id: 1, progress: 90, detail: 'Current task' } });
  for (const [index, task_id] of [null, -1, 1.5, Infinity, '2', Number.MAX_SAFE_INTEGER + 1].entries()) {
    view.send(index + 2, 60, { uv_progress: { task_id, progress: 1, detail: 'Malformed task' } });
    assert.equal(view.fill('uv-progress-fill'), 90);
    assert.equal(view.elements['uv-progress-detail'].textContent, 'Current task');
  }
});

test('cleanup terminal snapshots can replace each other while rejecting late ordinary progress', () => {
  const view = harness();
  view.send(1, 99, { terminal: true, title: 'Cleanup' });
  view.send(2, 99, { terminal: true, is_error: true, title: 'Cleanup failed' });
  assert.equal(view.elements.title.textContent, 'Cleanup failed');
  assert.equal(view.document.body.classList.contains('error-state'), true);
  view.send(3, 100, { title: 'Late completion' });
  assert.equal(view.fill(), 99);
  assert.equal(view.elements.title.textContent, 'Cleanup failed');
  view.send(4, 99, { terminal: true, title: 'Retry cleanup' });
  assert.equal(view.elements.title.textContent, 'Retry cleanup');
  assert.equal(view.document.body.classList.contains('error-state'), false);
  assert.equal(view.frames, 0);
});

test('errors stop animation at confirmed progress and allow a later recovery', () => {
  const view = harness();
  view.send(1, 40, { uv_progress: { progress: 60, detail: 'Download' } });
  view.tick(100);
  view.send(2, 50, { is_error: true });
  assert.equal(view.fill(), 50);
  assert.equal(view.frames, 0);
  assert.equal(view.elements['progress-pct'].textContent, '50%');
  assert.equal(view.elements['badge-text'].textContent, 'Error');
  assert.equal(view.elements.spinner.style.display, 'none');
  assert.equal(view.elements['error-dot'].style.display, 'flex');
  assert.equal(view.elements['splash-actions'].className, 'splash-actions splash-actions-err');
  assert.equal(view.document.body.classList.contains('error-state'), true);
  view.send(3, 60);
  assert.equal(view.frames, 1);
  assert.equal(view.document.body.classList.contains('error-state'), false);
  view.tick(350);
  assert.equal(view.fill(), 60);
});

test('invalid values never create NaN transforms or reset an active uv task', () => {
  const view = harness({ reducedMotion: true });
  view.send(1, 25, { uv_progress: { progress: 40, detail: 'Download' } });
  for (const [index, value] of [NaN, Infinity, -Infinity, undefined, null, '50'].entries()) {
    view.send(index + 2, value, { uv_progress: { progress: value, detail: 'Invalid' } });
    assert.equal(view.fill(), 25);
    assert.equal(view.fill('uv-progress-fill'), 40);
    assert.equal(view.elements['uv-progress-detail'].textContent, 'Download');
  }
  view.send(8, -1, { uv_progress: { progress: -10 } });
  assert.equal(view.fill(), 25);
  assert.equal(view.fill('uv-progress-fill'), 40);
  view.send(9, 150);
  assert.equal(view.fill(), 100);
  assert.equal(view.frames, 0);
});

test('invalid sequence values are rejected without consuming the current sequence', () => {
  const view = harness({ reducedMotion: true });
  for (const sequence of [undefined, null, '1', -1, 1.5, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    view.send(sequence, 50);
  }
  assert.equal(view.fill(), 0);
  view.send(0, 20);
  assert.equal(view.fill(), 20);
});

test('reduced motion immediately settles both bars and can change during interpolation', () => {
  const view = harness();
  view.send(1, 70, { uv_progress: { progress: 30, detail: 'Download' } });
  view.tick(100);
  view.motion(true);
  assert.equal(view.fill(), 70);
  assert.equal(view.fill('uv-progress-fill'), 30);
  assert.equal(view.frames, 0);
  view.send(2, 80, { uv_progress: { progress: 60, detail: 'Download' } });
  assert.equal(view.fill(), 80);
  assert.equal(view.fill('uv-progress-fill'), 60);
  view.motion(false);
  view.send(3, 90);
  assert.equal(view.fill(), 80);
  assert.equal(view.frames, 1);
});

test('visibility and resize refresh the decoration without scheduling background frames', () => {
  const view = harness();
  view.send(1, 50);
  view.tick(50);
  view.visibility(true);
  assert.equal(view.fill(), 50);
  assert.equal(view.frames, 0);
  view.send(2, 80);
  assert.equal(view.fill(), 80);
  view.resize(100);
  assert.equal(view.elements['progress-head'].style.transform, 'translate3d(70px, -80%, 0) translateX(-50%)');
  view.resize(40);
  assert.equal(view.elements['progress-head'].style.transform, 'translate3d(20px, -80%, 0) translateX(-50%)');
  view.visibility(false);
  assert.equal(view.frames, 0);
});

test('numeric labels floor the rendered fraction and equal snapshots avoid redundant DOM writes', () => {
  const view = harness({ reducedMotion: true });
  view.send(1, 24.75);
  assert.equal(view.elements['progress-pct'].textContent, '24%');
  const writes = view.domWrites;
  view.send(2, 24.75);
  assert.equal(view.domWrites, writes);
});

test('ready handshake receives the newest snapshot once and rejects older event updates', async () => {
  const view = harness({ reducedMotion: true, readyPayload: {
    sequence: 4, progress: 35, is_error: false, title: 'Ready snapshot', uv_progress: null,
  } });
  assert.equal(view.window.__ALAS_SPLASH_READY, true);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(view.calls, ['splash_ready']);
  assert.equal(view.fill(), 35);
  view.send(3, 5);
  assert.equal(view.fill(), 35);
});

test('ready handshake failure is reported without polling', async () => {
  const view = harness({ invokeError: new Error('IPC unavailable') });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(view.calls, ['splash_ready']);
  assert.equal(view.errors.length, 1);
  assert.equal(view.frames, 0);
});
