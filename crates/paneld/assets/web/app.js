// paneld web app: vanilla JS, no build step. All server strings go through
// textContent. The page is same-origin with the API.
'use strict';

const $ = (id) => document.getElementById(id);
let info = { version: '', canvas: { width: 64, height: 32 }, icons: [], data_prefix: 'push', preview: false };
let timer = null;
let sceneKey = '';
let lastOk = null;

// ---- API helper: every request goes through here -------------------------

async function api(method, path, body) {
  const headers = {};
  // AUTH HOOK: when token support is added, set
  // headers['Authorization'] = 'Bearer ' + token; here, and nowhere else.
  const opts = { method, headers };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(path, opts);
  } catch (e) {
    throw new Error('Cannot reach paneld');
  }
  let data = null;
  try {
    data = await res.json();
  } catch (e) {
    data = null;
  }
  if (res.status === 401) {
    throw new Error('This paneld requires an API token; the web app does not support tokens yet.');
  }
  if (!res.ok) {
    throw new Error((data && data.error) || 'HTTP ' + res.status);
  }
  return data;
}

// ---- toasts ---------------------------------------------------------------

function toast(msg, isError) {
  const el = document.createElement('div');
  el.className = 'toast' + (isError ? ' error' : '');
  el.textContent = msg;
  $('toasts').appendChild(el);
  setTimeout(() => el.remove(), isError ? 6000 : 2500);
}

let lastErr = '';
let lastErrAt = 0;
function pollError(e) {
  // Polling fails every second while the daemon is down; don't spam.
  const now = Date.now();
  if (e.message !== lastErr || now - lastErrAt > 30000) {
    toast(e.message, true);
    lastErr = e.message;
    lastErrAt = now;
  }
}

// ---- commands -------------------------------------------------------------

async function command(method, path, body, okMsg) {
  try {
    const data = await api(method, path, body);
    if (okMsg) toast(okMsg);
    refresh();
    setTimeout(refresh, 300);
    return data;
  } catch (e) {
    toast(e.message, true);
    return null;
  }
}

// ---- rendering state --------------------------------------------------------

function setConn(ok) {
  const c = $('conn');
  c.className = 'conn ' + (ok ? 'ok' : 'bad');
  c.setAttribute('aria-label', ok ? 'Connected' : 'Disconnected');
  c.title = ok ? 'Connected' : 'Disconnected';
}

function renderScenes(st) {
  const box = $('scenes');
  const key = st.scenes.join('\n');
  if (key !== sceneKey) {
    sceneKey = key;
    box.textContent = '';
    for (const name of [null, ...st.scenes]) {
      const b = document.createElement('button');
      b.type = 'button';
      b.textContent = name === null ? 'Auto' : name;
      b.addEventListener('click', () => {
        if (name === null) command('DELETE', '/api/scene');
        else command('POST', '/api/scene/' + encodeURIComponent(name));
      });
      b.dataset.scene = name === null ? '' : name;
      box.appendChild(b);
    }
    const sel = $('n-scene');
    const prev = sel.value;
    sel.textContent = '';
    for (const name of st.scenes) {
      const o = document.createElement('option');
      o.value = name;
      o.textContent = name;
      sel.appendChild(o);
    }
    if (st.scenes.includes(prev)) sel.value = prev;
  }
  const pinned = st.pinned === null || st.pinned === undefined ? '' : st.pinned;
  for (const b of box.children) {
    b.setAttribute('aria-pressed', String(b.dataset.scene === pinned));
  }
}

function renderQueue(list) {
  const ul = $('queue');
  ul.textContent = '';
  for (const n of list) {
    const li = document.createElement('li');
    if (n.showing) li.className = 'showing';
    li.textContent = n.summary + ' (priority ' + n.priority + ', ' + n.repeats_left +
      ' left' + (n.showing ? ', showing' : '') + ')';
    ul.appendChild(li);
  }
  $('queue-empty').hidden = list.length > 0;
}

function render(st) {
  const showing = st.notifications.some((n) => n.showing);
  $('st-scene').textContent = st.scene !== null ? st.scene : showing ? 'notification' : 'black';
  $('st-mode').textContent = st.pinned !== null ? 'Pinned: ' + st.pinned : 'Auto';
  $('st-power').textContent = st.on ? 'On' : 'Off';
  $('st-bright').textContent = st.brightness + ' / 255';
  $('st-dim').textContent = st.dim + ' / 255';
  $('st-eff').textContent = Math.round((st.brightness * st.dim) / 255) + ' / 255';
  $('st-fps').textContent = st.fps.toFixed(1);
  renderQueue(st.notifications);
  renderScenes(st);
  if (document.activeElement !== $('power')) $('power').checked = st.on;
  const slider = $('brightness');
  if (document.activeElement !== slider) {
    slider.value = st.brightness;
    $('bright-out').textContent = st.brightness;
  }
}

// ---- polling ---------------------------------------------------------------

async function refresh() {
  clearTimeout(timer);
  try {
    render(await api('GET', '/api/state'));
    setConn(true);
    lastOk = true;
  } catch (e) {
    setConn(false);
    if (lastOk !== false) pollError(e);
    else if (e.message.includes('token')) pollError(e);
    lastOk = false;
  }
  const visible = document.visibilityState === 'visible';
  timer = setTimeout(refresh, visible ? 1000 : 10000);
}

// ---- preview: at most 2/s, only while visible, one request at a time ---------

function setupPreview() {
  const img = $('preview');
  $('preview-panel').hidden = !info.preview;
  if (!info.preview) return;
  img.width = info.canvas.width;
  img.height = info.canvas.height;
  let busy = false;
  const done = () => { busy = false; };
  img.addEventListener('load', done);
  img.addEventListener('error', done);
  const tick = () => {
    if (!busy && document.visibilityState === 'visible') {
      busy = true;
      img.src = '/preview.png?t=' + Date.now();
    }
  };
  tick();
  setInterval(tick, 500);
}

// ---- forms -----------------------------------------------------------------

function setupPower() {
  $('power').addEventListener('change', (e) => {
    command('POST', '/api/power', { on: e.target.checked });
  });
  const slider = $('brightness');
  slider.addEventListener('input', () => { $('bright-out').textContent = slider.value; });
  slider.addEventListener('change', () => {
    command('POST', '/api/brightness', { value: Number(slider.value) });
  });
}

function setupNotify() {
  const form = $('notify-form');
  const kind = () => form.elements.kind.value;
  const sync = () => {
    $('n-text-fields').hidden = kind() !== 'text';
    $('n-scene-fields').hidden = kind() !== 'scene';
  };
  for (const r of form.elements.kind) r.addEventListener('change', sync);
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const body = {};
    if (kind() === 'text') {
      const text = $('n-text').value.trim();
      if (!text) return toast('Enter some text', true);
      body.text = text;
      if ($('n-icon').value) body.icon = $('n-icon').value;
      body.color = $('n-color').value;
    } else {
      if (!$('n-scene').value) return toast('No scene selected', true);
      body.scene = $('n-scene').value;
    }
    body.duration_s = Number($('n-dur').value);
    body.priority = parseInt($('n-prio').value, 10);
    body.repeat = parseInt($('n-rep').value, 10);
    if (await command('POST', '/api/notify', body, 'Notification sent')) $('n-text').value = '';
  });
  $('n-clear').addEventListener('click', () => {
    command('DELETE', '/api/notify', undefined, 'Notifications cleared');
  });
}

const NUMBER = /^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/;

function setupPush() {
  const key = () => $('p-key').value.trim();
  const path = () => '/api/data/' + encodeURIComponent(key());
  $('push-form').addEventListener('submit', (e) => {
    e.preventDefault();
    const raw = $('p-value').value.trim();
    const unit = $('p-unit').value.trim();
    if (!key()) return toast('Enter a key', true);
    if (!raw) return toast('Enter a value, or use Remove', true);
    let body;
    if (NUMBER.test(raw)) {
      body = Number(raw);
      if (unit) body = { value: body, unit };
    } else {
      if (unit) return toast('A unit only applies to numbers', true);
      body = raw;
    }
    command('POST', path(), body, 'Pushed ' + key());
  });
  $('p-remove').addEventListener('click', () => {
    if (!key()) return toast('Enter a key', true);
    command('POST', path(), null, 'Removed ' + key());
  });
}

// ---- start -------------------------------------------------------------------

async function start() {
  try {
    info = Object.assign(info, await api('GET', '/api/info'));
  } catch (e) {
    toast('Could not load /api/info: ' + e.message, true);
  }
  $('version').textContent = info.version ? 'v' + info.version : '';
  $('p-key').value = info.data_prefix + ':';
  const sel = $('n-icon');
  for (const name of info.icons) {
    const o = document.createElement('option');
    o.value = name;
    o.textContent = name;
    sel.appendChild(o);
  }
  setupPreview();
  setupPower();
  setupNotify();
  setupPush();
  document.addEventListener('visibilitychange', refresh);
  refresh();
}

start();
