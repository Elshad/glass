// "This device": what the browser can do for Glass beyond the stream itself, and the menu
// for it. Notifications, keeping the screen awake, picture in picture, every screen of
// this computer, capturing the mouse, sending the clipboard back, pausing while away,
// and installing Glass as an app.

const AWAKE_KEY = 'glass.awake';
const SMOOTH_KEY = 'glass.smooth';
const CLIPBOARD_KEY = 'glass.clipboardSync';
const AWAY_KEY = 'glass.pauseAway';

function storage(fn) {
  try { return fn(localStorage); } catch { return undefined; }
}

export function createDevice({ ui, push, control, log, input, screens, streams, displays, onPause, onPressure, standalone, sendClip = (c) => control({ t: 'clipboard', text: c.text }) }) {
  const touch = matchMedia('(hover: none)').matches;
  let connected = false;
  let installPrompt; // Chromium's beforeinstallprompt, kept for the menu

  // ---- Smooth motion: a fast screen (120 Hz) asks the host for more frames ----

  let smooth = storage((s) => s.getItem(SMOOTH_KEY)) !== '0';
  let refreshHz = 0; // measured; 0 until known
  /** Times animation frames for about a second: the screen's refresh rate. */
  function measureRefresh() {
    if (document.visibilityState !== 'visible') return;
    const times = [];
    const tick = (t) => {
      times.push(t);
      if (times.length < 40) return requestAnimationFrame(tick);
      const gaps = times.slice(1).map((t, i) => t - times[i]).sort((a, b) => a - b);
      const median = gaps[gaps.length >> 1];
      const hz = median > 0 ? 1000 / median : 0;
      if (Math.abs(hz - refreshHz) < 3) return;
      refreshHz = hz;
      log('info', `screen refresh ${hz.toFixed(0)} Hz`);
      sendRefresh();
      render();
    };
    requestAnimationFrame(tick);
  }
  const fast = () => refreshHz > 68;
  /** The panel rate a measurement means (as the host snaps it), at most 120. */
  const snapHz = (hz) => Math.min(120, [60, 75, 90, 100, 120, 144, 165, 240].reduce((a, b) => (Math.abs(b - hz) < Math.abs(a - hz) ? b : a)));
  function sendRefresh() {
    if (connected && fast()) control({ t: 'refresh', hz: smooth ? refreshHz : 60 });
  }
  measureRefresh();

  // ---- Keep the screen awake while watching ----

  let awake = storage((s) => s.getItem(AWAKE_KEY)) !== '0';
  let wakeLock;
  async function holdAwake() {
    if (!('wakeLock' in navigator) || !awake || !connected || document.visibilityState !== 'visible' || wakeLock) return;
    try {
      wakeLock = await navigator.wakeLock.request('screen');
      wakeLock.addEventListener('release', () => { wakeLock = undefined; });
    } catch (e) {
      log('info', `wake lock: ${e.message ?? e}`);
    }
  }
  function releaseAwake() {
    wakeLock?.release().catch(() => {});
    wakeLock = undefined;
  }

  // ---- Picture in picture ----

  let pipWindow; // Document Picture-in-Picture (Chromium): the live screens, clickable
  let pipVideo; // a <video> of the stream, for browsers with only video PiP
  const pipSupported = () => 'documentPictureInPicture' in window || document.pictureInPictureEnabled;

  // A canvas floats as a <video> of its live picture. Safari starts picture in picture only
  // within the tap and only once the video has a frame, so it's readied when the menu opens.
  let pipIdle;
  function preparePip() {
    const el = streams()[0];
    if ('documentPictureInPicture' in window || !document.pictureInPictureEnabled || !(el instanceof HTMLCanvasElement)) return;
    clearTimeout(pipIdle);
    if (!pipVideo) {
      pipVideo = Object.assign(document.createElement('video'), { muted: true, playsInline: true, autoplay: true });
      pipVideo.setAttribute('playsinline', '');
      // In the page (iOS floats only attached videos) but out of sight.
      pipVideo.style.cssText = 'position:fixed;left:0;top:0;width:2px;height:2px;opacity:0;pointer-events:none';
      document.body.append(pipVideo);
      pipVideo.addEventListener('leavepictureinpicture', releasePip);
    }
    if (!pipVideo.srcObject) {
      pipVideo.srcObject = el.captureStream(30);
      pipVideo.play().catch(() => {});
    }
    // Opened the menu but didn't float it: stop copying frames.
    pipIdle = setTimeout(() => { if (document.pictureInPictureElement !== pipVideo) releasePip(); }, 20_000);
  }
  function releasePip() {
    pipVideo?.srcObject?.getTracks().forEach((t) => t.stop());
    if (pipVideo) pipVideo.srcObject = null;
  }

  async function pictureInPicture() {
    if (pipWindow) return pipWindow.close();
    if (document.pictureInPictureElement) return document.exitPictureInPicture();
    const el = streams()[0];
    if (!el) return;
    try {
      if ('documentPictureInPicture' in window) {
        const r = el.getBoundingClientRect();
        const width = Math.min(640, Math.round(r.width / 2)) || 480;
        pipWindow = await documentPictureInPicture.requestWindow({ width, height: Math.round(width * (r.height / r.width || 0.625)) });
        const style = pipWindow.document.createElement('style');
        style.textContent = 'html,body{margin:0;height:100%;background:#000;overflow:hidden}#screens{position:fixed;inset:0;display:flex;gap:2px}#screens>canvas,#screens>video{flex:1 1 0;min-width:0;width:100%;height:100%;object-fit:contain;outline:none}#screens>.hidden{display:none}';
        pipWindow.document.head.append(style);
        const home = screens.parentNode;
        const next = screens.nextSibling;
        pipWindow.document.body.append(screens);
        pipWindow.addEventListener('pagehide', () => {
          home.insertBefore(screens, next);
          pipWindow = undefined;
          render();
        });
      } else if (el instanceof HTMLVideoElement) {
        el.disablePictureInPicture = false;
        await el.requestPictureInPicture();
      } else {
        preparePip();
        // No await before this: it must run inside the tap.
        const started = pipVideo.readyState >= 1
          ? pipVideo.requestPictureInPicture()
          : pipVideo.play().then(() => pipVideo.requestPictureInPicture());
        clearTimeout(pipIdle);
        await started;
      }
      log('info', 'picture in picture on');
    } catch (e) {
      log('warn', `picture in picture: ${e.name}: ${e.message ?? e}`);
      if (el instanceof HTMLCanvasElement && document.pictureInPictureElement !== pipVideo) releasePip();
      ui.toast({ text: `Picture in picture didn't start: ${e.message ?? e}`, kind: 'error' });
    }
    render();
  }

  // ---- Every screen of this computer (Window Management) ----

  async function useAllScreens() {
    try {
      const details = await window.getScreenDetails();
      const others = details.screens.filter((s) => s !== details.currentScreen);
      const ds = displays();
      if (!others.length) {
        ui.toast({ text: 'This computer has one screen.' });
        return;
      }
      if (ds.length < 2) {
        ui.toast({ text: 'The Mac has one display.' });
        return;
      }
      // This window takes the first display, full screen; each other screen gets the next.
      const [first, ...rest] = ds;
      control({ t: 'selectDisplay', id: first.id });
      await document.documentElement.requestFullscreen({ screen: details.currentScreen }).catch(() => {});
      const base = location.href.replace(/[?#].*$/, '');
      const hash = location.hash.startsWith('#r=') ? location.hash : '';
      rest.slice(0, others.length).forEach((d, i) => {
        const s = others[i];
        window.open(`${base}?display=${d.id}${hash}`, `glass-display-${d.id}`,
          `popup,left=${s.availLeft},top=${s.availTop},width=${s.availWidth},height=${s.availHeight}`);
      });
      log('info', 'spread across screens', { screens: details.screens.length, displays: ds.length });
    } catch (e) {
      ui.toast({ text: `Couldn't use your other screens: ${e.message ?? e}`, kind: 'error' });
    }
  }

  // ---- Capture the mouse (pointer lock: games, 3D) ----

  async function captureMouse() {
    const el = streams()[0];
    if (!el) return;
    try {
      await el.requestPointerLock({ unadjustedMovement: true });
    } catch {
      try { await el.requestPointerLock(); } catch (e) {
        ui.toast({ text: `Couldn't capture the mouse: ${e.message ?? e}`, kind: 'error' });
        return;
      }
    }
    ui.toast({ text: 'Mouse captured. Press Esc to release it.', timeout: 3000 });
  }
  document.addEventListener('pointerlockchange', () => {
    input.setPointerLocked(Boolean(document.pointerLockElement && screens.contains(document.pointerLockElement)));
  });

  // ---- Send this device's clipboard when coming back ----

  let clipboardSync = storage((s) => s.getItem(CLIPBOARD_KEY)) === '1';
  let lastSent; // a fingerprint of the last clipboard sent, so an unchanged one isn't resent
  /** This device's clipboard: text, rich text and an image, where present. */
  async function readClipboard() {
    if (!navigator.clipboard.read) return { text: await navigator.clipboard.readText() };
    const clip = { text: '' };
    for (const item of await navigator.clipboard.read()) {
      const get = (type) => (item.types.includes(type) ? item.getType(type) : undefined);
      if (!clip.text) clip.text = await (await get('text/plain'))?.text() ?? '';
      clip.html ??= await (await get('text/html'))?.text();
      clip.image ??= await get('image/png');
    }
    return clip;
  }
  const fingerprint = (c) => `${c.text}\u0000${c.html ?? ''}\u0000${c.image?.size ?? ''}`;
  async function sendClipboard() {
    if (!clipboardSync || !connected || !document.hasFocus()) return;
    try {
      const clip = await readClipboard();
      if ((!clip.text && !clip.image) || fingerprint(clip) === lastSent) return;
      lastSent = fingerprint(clip);
      sendClip(clip);
      log('info', 'clipboard sent on return', { length: clip.text.length, html: Boolean(clip.html), image: clip.image?.size });
    } catch {}
  }
  // Chromium reads it by itself once allowed. Safari (and Firefox) read only in a tap, then
  // show their own Paste button, so coming back offers that tap instead.
  const silentRead = Boolean(navigator.clipboard?.readText && navigator.userAgentData?.brands?.some((b) => b.brand === 'Chromium'));
  const tapRead = !silentRead && Boolean(navigator.clipboard?.read);
  const clipboardSupported = silentRead || tapRead;
  let offered = false;
  function offerClipboard() {
    if (!clipboardSync || !connected || offered || document.visibilityState !== 'visible') return;
    offered = true;
    setTimeout(() => { offered = false; }, 8000);
    ui.toast({
      text: 'Copied something here? Send it to the computer.',
      icon: 'info',
      timeout: 8000,
      action: {
        label: 'Send',
        // read() has to start inside the tap; readClipboard calls it before its first await.
        run: () => readClipboard().then((clip) => {
          if (!clip.text && !clip.image) return;
          lastSent = fingerprint(clip);
          sendClip(clip);
          log('info', 'clipboard sent after a tap', { length: clip.text.length, html: Boolean(clip.html), image: clip.image?.size });
        }).catch((e) => log('info', `clipboard not sent: ${e.message ?? e}`)),
      },
    });
  }
  const comeBack = () => setTimeout(silentRead ? sendClipboard : offerClipboard, 50);
  addEventListener('focus', comeBack);
  if (tapRead) document.addEventListener('visibilitychange', comeBack);

  // ---- Pause while away (Idle Detection) ----

  let pauseAway = storage((s) => s.getItem(AWAY_KEY)) === '1';
  let idleAbort;
  async function watchIdle() {
    idleAbort?.abort();
    idleAbort = undefined;
    if (!pauseAway || !('IdleDetector' in window)) return;
    try {
      if ((await IdleDetector.requestPermission()) !== 'granted') throw new Error('not allowed');
      const detector = new IdleDetector();
      idleAbort = new AbortController();
      detector.addEventListener('change', () => {
        // Locked, or untouched for 10 minutes: stop the video until they're back.
        const away = detector.screenState === 'locked' || detector.userState === 'idle';
        onPause(away);
        log('info', `away: ${away}`, { user: detector.userState, screen: detector.screenState });
      });
      await detector.start({ threshold: 10 * 60_000, signal: idleAbort.signal });
    } catch (e) {
      pauseAway = false;
      storage((s) => s.setItem(AWAY_KEY, '0'));
      ui.toast({ text: `Can't tell when you're away: ${e.message ?? e}`, kind: 'error' });
      render();
    }
  }
  if (pauseAway && 'IdleDetector' in window) {
    // Already allowed earlier: start without a tap.
    navigator.permissions?.query({ name: 'idle-detection' }).then((p) => p.state === 'granted' && watchIdle()).catch(() => {});
  }

  // ---- CPU pressure: ask for a smaller picture while this device struggles ----

  if ('PressureObserver' in window) {
    try {
      // Each change resizes the stream (a new encoder), so only act on a lasting state:
      // smaller after 10 s under pressure, back up after 30 s of calm, one step at a time.
      const STEPS = [1, 0.75, 0.5];
      let step = 0;
      let wanted = 0;
      let timer;
      // The observer reports changes only, so a timer checks once the state has lasted.
      const settle = () => {
        clearTimeout(timer);
        if (wanted === step) return;
        timer = setTimeout(() => {
          step += wanted > step ? 1 : -1;
          onPressure(STEPS[step]);
          settle(); // the next step waits its turn too
        }, wanted > step ? 10_000 : 30_000);
      };
      const observer = new PressureObserver((records) => {
        const state = records.at(-1)?.state;
        wanted = state === 'critical' ? 2 : state === 'serious' ? 1 : 0;
        settle();
      });
      observer.observe('cpu', { sampleInterval: 2000 }).catch(() => {});
    } catch {}
  }

  // ---- Install as an app ----

  addEventListener('beforeinstallprompt', (e) => {
    e.preventDefault();
    installPrompt = e;
    render();
  });
  addEventListener('appinstalled', () => {
    installPrompt = undefined;
    render();
  });
  const ios = /iPhone|iPad/.test(navigator.userAgent) || (/Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1);
  // Installed: ask the browser to keep Glass's storage (the offline copy, notifications).
  if (standalone) navigator.storage?.persist?.().catch(() => {});

  // ---- The menu ----

  const note = (text) => Object.assign(document.createElement('p'), { className: 'note', textContent: text });
  const hr = () => document.createElement('hr');
  const PUSH_META = {
    on: 'on', off: 'off', blocked: 'blocked in browser settings', install: 'add to Home Screen first',
    unsupported: 'not in this browser', unavailable: 'not on this computer',
  };

  function render() {
    const items = [];
    const state = push.state;
    const notify = ui.menuItem('Notifications', PUSH_META[state], {
      checked: state === 'on',
      onSelect: () => {
        if (state === 'on') push.disable();
        else if (state === 'off' || state === 'install') push.enable();
      },
    });
    items.push(notify);
    if (state === 'on') {
      const kind = (label, meta, key) =>
        ui.menuItem(label, meta, { checked: push.kinds[key], onSelect: () => push.setKind(key, !push.kinds[key]) });
      items.push(kind('Messages', 'chat', 'chat'));
      if (push.owner) {
        items.push(kind('Questions', 'agents, people asking to connect', 'asks'));
        items.push(kind('Activity', 'someone joined, updates', 'activity'));
      }
    } else if (state === 'off' || state === 'install') {
      items.push(note(push.owner
        ? 'When an agent needs you, someone asks to connect, or a message comes, even with Glass closed.'
        : 'When a message comes, even with Glass closed.'));
    }
    items.push(hr());
    if (fast()) {
      items.push(ui.menuItem('Smooth motion', `${snapHz(refreshHz)} Hz`, {
        checked: smooth,
        onSelect: () => {
          smooth = !smooth;
          storage((s) => s.setItem(SMOOTH_KEY, smooth ? '1' : '0'));
          sendRefresh();
          render();
        },
      }));
    }
    if ('wakeLock' in navigator) {
      items.push(ui.menuItem('Keep screen awake', 'while connected', {
        checked: awake,
        onSelect: () => {
          awake = !awake;
          storage((s) => s.setItem(AWAKE_KEY, awake ? '1' : '0'));
          if (awake) holdAwake();
          else releaseAwake();
          render();
        },
      }));
    }
    if (pipSupported()) {
      items.push(ui.menuItem('Picture in picture', 'a floating window', {
        checked: Boolean(pipWindow || document.pictureInPictureElement),
        onSelect: () => { ui.closeMenus(); pictureInPicture(); },
      }));
    }
    if ('getScreenDetails' in window && displays().length > 1 && window.screen.isExtended) {
      items.push(ui.menuItem('Use all my screens', 'one Mac display on each', {
        onSelect: () => { ui.closeMenus(); useAllScreens(); },
      }));
    }
    if (!touch && 'requestPointerLock' in Element.prototype) {
      items.push(ui.menuItem('Capture mouse', 'for games · Esc releases', {
        onSelect: () => { ui.closeMenus(); captureMouse(); },
      }));
    }
    if (clipboardSupported) {
      items.push(ui.menuItem('Sync clipboard', silentRead ? 'when you switch back' : 'offers to when you switch back', {
        checked: clipboardSync,
        onSelect: async () => {
          clipboardSync = !clipboardSync;
          storage((s) => s.setItem(CLIPBOARD_KEY, clipboardSync ? '1' : '0'));
          // Ask for permission now, in the tap.
          if (clipboardSync && silentRead) await readClipboard().then((c) => { lastSent = fingerprint(c); }).catch(() => {});
          render();
        },
      }));
    }
    if ('IdleDetector' in window) {
      items.push(ui.menuItem('Pause while away', 'screen locked or idle', {
        checked: pauseAway,
        onSelect: () => {
          pauseAway = !pauseAway;
          storage((s) => s.setItem(AWAY_KEY, pauseAway ? '1' : '0'));
          if (!pauseAway) onPause(false);
          watchIdle();
          render();
        },
      }));
    }
    if (installPrompt) {
      items.push(hr(), ui.menuItem('Install Glass', 'its own window and icon', {
        onSelect: async () => {
          ui.closeMenus();
          await installPrompt.prompt();
          installPrompt = undefined;
          render();
        },
      }));
    } else if (ios && !standalone) {
      items.push(hr(), note('Add Glass to your Home Screen (Share › Add to Home Screen) for full screen and notifications.'));
    }
    ui.setDeviceItems(items);
  }
  push.onChange(render);
  // Opening the menu readies picture in picture, so the tap on it can start it at once.
  for (const id of ['btn-device', 'tb-view']) {
    document.getElementById(id)?.addEventListener('pointerdown', () => pipSupported() && preparePip(), true);
  }
  document.addEventListener('enterpictureinpicture', render, true);
  document.addEventListener('leavepictureinpicture', render, true);

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      // Possibly on another screen now (a window dragged to a 60 Hz monitor).
      measureRefresh();
      holdAwake();
      push.seen();
    }
  });

  return {
    render,
    /** The session is live (welcome) or gone. */
    setConnected(on) {
      connected = on;
      if (on) sendRefresh();
      if (on) holdAwake();
      else releaseAwake();
      render();
    },
  };
}

/**
 * Files dropped on the page, folders included: each file named with its path inside the
 * dropped folder ("Photos/2024/a.jpg"), which the host recreates in Downloads.
 */
export async function droppedFiles(dataTransfer) {
  const entries = [...(dataTransfer?.items ?? [])].map((i) => i.webkitGetAsEntry?.()).filter(Boolean);
  if (!entries.length) return [...(dataTransfer?.files ?? [])];
  const out = [];
  async function walk(entry, path) {
    if (entry.isFile) {
      const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
      out.push(path ? new File([file], `${path}/${file.name}`, { type: file.type, lastModified: file.lastModified }) : file);
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      const dir = path ? `${path}/${entry.name}` : entry.name;
      // readEntries returns a batch at a time, until an empty one.
      for (;;) {
        const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
        if (!batch.length) break;
        for (const e of batch) await walk(e, dir);
      }
    }
  }
  for (const e of entries) await walk(e, '');
  return out;
}
