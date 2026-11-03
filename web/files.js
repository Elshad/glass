// The file browser (owner only): the computer's folders in a sheet over the screen.
// Browse, preview, download (or drag a file out), upload into the folder you're in, make
// folders, rename and move to the Trash. Long folders are virtualized: only the rows in
// view exist, positioned from a fixed row height, so scrolling never reads layout.

import { droppedFiles } from './device.js';

const VIEW_KEY = 'glass.filesView'; // 'list' or 'grid'
const ROW = 48; // list row height (px)
const TILE_W = 116; // grid tile width and height (px)
const TILE_H = 128;
const OVERSCAN = 6; // rows rendered beyond the visible ones
const MAX_THUMB_REQUESTS = 4; // previews asked for at once

const QUICK = [
  ['Home', ''],
  ['Desktop', 'Desktop'],
  ['Documents', 'Documents'],
  ['Downloads', 'Downloads'],
];

const ICONS = {
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  file: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/>',
  image: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="9" cy="10" r="1.6"/><path d="m21 16-5-5-9 9"/>',
  pdf: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5M8.5 15h7M8.5 12h4"/>',
  video: '<rect x="3" y="5" width="13" height="14" rx="2"/><path d="m16 10 5-3v10l-5-3"/>',
  audio: '<path d="M9 18V6l11-2v12"/><circle cx="6" cy="18" r="3"/><circle cx="17" cy="16" r="3"/>',
  text: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5M8.5 13h7M8.5 16.5h7M8.5 9.5h2"/>',
  archive: '<rect x="3" y="4" width="18" height="5" rx="1"/><path d="M5 9v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V9M10 13h4"/>',
  app: '<rect x="4" y="4" width="16" height="16" rx="4"/><path d="M9 15.5 12 8l3 7.5M10 13h4"/>',
};
const svg = (paths, cls = 'icon') =>
  `<svg class="${cls}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;
const iconFor = (e) => svg(e.dir ? ICONS.folder : ICONS[e.kind] ?? ICONS.file, `icon kind-${e.dir ? 'folder' : e.kind || 'file'}`);
const PREVIEWABLE = new Set(['image', 'pdf', 'video']);

function storage(fn) {
  try { return fn(localStorage); } catch { return undefined; }
}

function formatSize(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${Math.round(n / 1024)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}
const dateFormat = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
const timeFormat = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' });
function formatDate(unix) {
  if (!unix) return '';
  const d = new Date(unix * 1000);
  return d.toDateString() === new Date().toDateString() ? `Today ${timeFormat.format(d)}` : dateFormat.format(d);
}
const join = (dir, name) => (dir ? `${dir}/${name}` : name);

export function createFiles({ control, toast, upload, download, rawUrl, log }) {
  const button = document.getElementById('btn-files');
  let trashName = 'Trash';
  const sheet = document.createElement('section');
  sheet.id = 'files';
  sheet.className = 'glass hidden';
  sheet.setAttribute('role', 'dialog');
  sheet.setAttribute('aria-label', 'Files');
  sheet.innerHTML = `
    <header>
      <nav class="crumbs" aria-label="Folder"></nav>
      <button type="button" class="act mkdir" title="New folder">${svg('<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2zM12 10.5v5M9.5 13h5"/>')}</button>
      <button type="button" class="act view" title="Grid view" aria-pressed="false">${svg('<rect x="4" y="4" width="7" height="7" rx="1.5"/><rect x="13" y="4" width="7" height="7" rx="1.5"/><rect x="4" y="13" width="7" height="7" rx="1.5"/><rect x="13" y="13" width="7" height="7" rx="1.5"/>')}</button>
      <button type="button" class="close" title="Close" aria-label="Close">${svg('<path d="M6 6l12 12M18 6 6 18"/>')}</button>
    </header>
    <div class="bar">
      <div class="quick" role="group" aria-label="Places"></div>
      <label class="search">${svg('<circle cx="11" cy="11" r="6.5"/><path d="m20 20-4.2-4.2"/>')}<input type="search" placeholder="Filter" aria-label="Filter this folder" autocomplete="off" spellcheck="false"></label>
      <button type="button" class="act upload" title="Upload here">${svg('<path d="M12 16V4M7 9l5-5 5 5M4 16v3a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-3"/>')}<span>Upload</span></button>
    </div>
    <div class="scroll" tabindex="0" role="listbox" aria-label="Files"><div class="spacer"></div><p class="empty hidden"></p></div>
    <form class="prompt hidden"><label></label><input autocomplete="off" spellcheck="false" required><button type="submit">OK</button><button type="button" class="cancel">Cancel</button></form>
    <footer>
      <span class="status"></span>
      <span class="actions">
        <button type="button" class="act rename" disabled>Rename</button>
        <button type="button" class="act trash" disabled>Move to Trash</button>
        <button type="button" class="act open primary" disabled>Download</button>
      </span>
    </footer>
    <div class="drop hidden">${svg('<path d="M12 16V4M7 9l5-5 5 5M4 16v3a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-3"/>')}<span></span></div>
    <input type="file" multiple class="hidden picker">`;
  document.body.append(sheet);
  const $ = (sel) => sheet.querySelector(sel);
  const crumbs = $('.crumbs');
  const quick = $('.quick');
  const search = $('.search input');
  const scroll = $('.scroll');
  const spacer = $('.spacer');
  const empty = $('.empty');
  const prompt = $('.prompt');
  const promptLabel = prompt.querySelector('label');
  const promptInput = prompt.querySelector('input');
  const status = $('.status');
  const openButton = $('.open');
  const renameButton = $('.rename');
  const trashButton = $('.trash');
  const viewButton = $('.view');
  const drop = $('.drop');
  const picker = $('.picker');

  let available = false;
  let path = ''; // the folder shown, relative to home
  let entries = [];
  let shown = []; // entries after the filter
  let selected = -1; // index into `shown`
  let ticket = '';
  let loading = false;
  let view = storage((s) => s.getItem(VIEW_KEY)) === 'grid' ? 'grid' : 'list';
  let size = [0, 0]; // the scroll box, from ResizeObserver
  let promptAction;
  const thumbs = new Map(); // path -> data URL, or null (none)
  const thumbQueue = [];
  let thumbsInFlight = 0;
  const rows = new Map(); // index -> element (rendered rows only)

  for (const [label, p] of QUICK) {
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.dataset.path = p;
    b.addEventListener('click', () => go(p));
    quick.append(b);
  }

  // ---- Open, close, navigate ----

  const isOpen = () => !sheet.classList.contains('hidden');
  function setOpen(open) {
    sheet.classList.toggle('hidden', !open);
    button.setAttribute('aria-expanded', String(open));
    if (open) {
      go(path);
      scroll.focus({ preventScroll: true });
    } else {
      closePrompt();
    }
  }
  button.addEventListener('click', (e) => {
    e.preventDefault();
    setOpen(!isOpen());
  });
  $('.close').addEventListener('click', () => setOpen(false));

  function go(p) {
    loading = true;
    status.textContent = 'Loading…';
    control({ t: 'filesList', path: p });
  }

  function renderCrumbs() {
    const parts = path ? path.split('/') : [];
    const items = [['Home', '']].concat(parts.map((name, i) => [name, parts.slice(0, i + 1).join('/')]));
    crumbs.replaceChildren(...items.flatMap(([name, p], i) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.textContent = name;
      if (i === items.length - 1) b.setAttribute('aria-current', 'page');
      b.addEventListener('click', () => go(p));
      return i ? [Object.assign(document.createElement('span'), { className: 'sep', textContent: '›' }), b] : [b];
    }));
    for (const b of quick.children) b.classList.toggle('on', b.dataset.path === path);
  }

  // ---- The list: virtualized ----

  const perRow = () => (view === 'grid' ? Math.max(1, Math.floor(size[0] / TILE_W)) : 1);
  const rowHeight = () => (view === 'grid' ? TILE_H : ROW);

  function applyFilter() {
    const q = search.value.trim().toLowerCase();
    shown = q ? entries.filter((e) => e.name.toLowerCase().includes(q)) : entries;
    selected = shown.length ? Math.min(Math.max(selected, 0), shown.length - 1) : -1;
    for (const el of rows.values()) el.remove();
    rows.clear();
    spacer.style.height = `${Math.ceil(shown.length / perRow()) * rowHeight()}px`;
    empty.classList.toggle('hidden', shown.length > 0 || loading);
    empty.textContent = q ? `Nothing here matches “${search.value.trim()}”.` : 'This folder is empty. Drop files here to upload them.';
    render();
    updateFooter();
  }

  let frame;
  function render() {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(draw);
  }
  function draw() {
    const cols = perRow();
    const h = rowHeight();
    const top = scroll.scrollTop;
    const first = Math.max(0, Math.floor(top / h) - OVERSCAN) * cols;
    const last = Math.min(shown.length, (Math.ceil((top + size[1]) / h) + OVERSCAN) * cols);
    for (const [i, el] of rows) {
      if (i < first || i >= last) {
        el.remove();
        rows.delete(i);
      }
    }
    for (let i = first; i < last; i++) {
      let el = rows.get(i);
      if (!el) {
        el = row(shown[i], i);
        rows.set(i, el);
        spacer.append(el);
      }
      const x = view === 'grid' ? (i % cols) * TILE_W : 0;
      const y = Math.floor(i / cols) * h;
      el.style.transform = `translate(${x}px, ${y}px)`;
      el.classList.toggle('selected', i === selected);
      el.setAttribute('aria-selected', String(i === selected));
    }
  }

  function row(e, i) {
    const el = document.createElement('div');
    el.className = 'item';
    el.setAttribute('role', 'option');
    el.dataset.i = String(i);
    const full = join(path, e.name);
    const thumb = thumbs.get(full);
    const pic = thumb ? `<img class="thumb" src="${thumb}" alt="" draggable="false">` : iconFor(e);
    const meta = e.dir ? 'Folder' : [formatSize(e.size), formatDate(e.modified)].filter(Boolean).join(' · ');
    el.innerHTML = `<span class="pic">${pic}</span><span class="name"></span><span class="meta"></span>`;
    el.querySelector('.name').textContent = e.name;
    el.querySelector('.meta').textContent = meta;
    el.title = e.name;
    if (!e.dir && thumb === undefined && PREVIEWABLE.has(e.kind)) askThumb(full);
    // Drag a file out onto this device's desktop (Chromium, from the computer's own page).
    const url = !e.dir && rawUrl(ticket, full);
    if (url) {
      el.draggable = true;
      el.addEventListener('dragstart', (ev) => {
        ev.dataTransfer.setData('DownloadURL', `application/octet-stream:${e.name}:${url}`);
        ev.dataTransfer.effectAllowed = 'copy';
      });
    }
    return el;
  }

  function askThumb(full) {
    if (thumbs.has(full) || thumbQueue.includes(full)) return;
    thumbQueue.push(full);
    pumpThumbs();
  }
  function pumpThumbs() {
    while (thumbsInFlight < MAX_THUMB_REQUESTS && thumbQueue.length) {
      const p = thumbQueue.shift();
      thumbs.set(p, undefined);
      thumbsInFlight++;
      control({ t: 'filesThumb', path: p });
    }
  }

  // Clicks: select (or open on touch / double-click).
  spacer.addEventListener('click', (ev) => {
    const el = ev.target.closest('.item');
    if (!el) return;
    const i = Number(el.dataset.i);
    const touch = matchMedia('(hover: none)').matches;
    if (touch && shown[i]?.dir) return open(i);
    select(i);
  });
  spacer.addEventListener('dblclick', (ev) => {
    const el = ev.target.closest('.item');
    if (el) open(Number(el.dataset.i));
  });
  scroll.addEventListener('scroll', render, { passive: true });
  new ResizeObserver(([entry]) => {
    const was = perRow();
    size = [entry.contentRect.width, entry.contentRect.height];
    if (perRow() !== was) applyFilter();
    else render();
  }).observe(scroll);

  function select(i) {
    selected = i;
    render();
    updateFooter();
  }

  function open(i) {
    const e = shown[i];
    if (!e) return;
    const full = join(path, e.name);
    if (e.dir) {
      search.value = '';
      go(full);
      return;
    }
    fetchFile(e, full);
  }

  function fetchFile(e, full) {
    const url = rawUrl(ticket, full);
    if (url) download(url, e.name);
    else {
      control({ t: 'filesFetch', path: full });
      toast({ text: `Getting ${e.name}…`, icon: 'file', timeout: 2500 });
    }
    log('info', 'file browser: download', { size: e.size });
  }

  function updateFooter() {
    const e = shown[selected];
    openButton.disabled = renameButton.disabled = trashButton.disabled = !e;
    openButton.textContent = e?.dir ? 'Open' : 'Download';
    const count = `${shown.length} item${shown.length === 1 ? '' : 's'}`;
    status.textContent = loading ? 'Loading…' : e ? `${e.name}${e.dir ? '' : ` · ${formatSize(e.size)}`}` : count;
  }

  openButton.addEventListener('click', () => open(selected));
  renameButton.addEventListener('click', () => {
    const e = shown[selected];
    if (!e) return;
    askName('Rename to', e.name, (name) => name !== e.name && control({ t: 'filesRename', path: join(path, e.name), name }));
  });
  trashButton.addEventListener('click', () => {
    const e = shown[selected];
    if (e && confirm(`Move “${e.name}” to the ${trashName}?`)) control({ t: 'filesTrash', path: join(path, e.name) });
  });
  $('.mkdir').addEventListener('click', () => askName('New folder', 'untitled folder', (name) => control({ t: 'filesMkdir', path, name })));
  viewButton.addEventListener('click', () => {
    view = view === 'grid' ? 'list' : 'grid';
    storage((s) => s.setItem(VIEW_KEY, view));
    setView();
    applyFilter();
  });
  function setView() {
    sheet.dataset.view = view;
    viewButton.setAttribute('aria-pressed', String(view === 'grid'));
    viewButton.title = view === 'grid' ? 'List view' : 'Grid view';
  }
  setView();

  // ---- Naming (rename, new folder) ----

  function askName(label, value, done) {
    promptLabel.textContent = label;
    promptInput.value = value;
    promptAction = done;
    prompt.classList.remove('hidden');
    promptInput.focus();
    // Select the name without its extension, like Finder.
    const dot = value.lastIndexOf('.');
    promptInput.setSelectionRange(0, dot > 0 ? dot : value.length);
  }
  function closePrompt() {
    prompt.classList.add('hidden');
    promptAction = undefined;
  }
  prompt.addEventListener('submit', (e) => {
    e.preventDefault();
    const name = promptInput.value.trim();
    const action = promptAction;
    closePrompt();
    if (name && action) action(name);
    scroll.focus({ preventScroll: true });
  });
  prompt.querySelector('.cancel').addEventListener('click', () => {
    closePrompt();
    scroll.focus({ preventScroll: true });
  });

  // ---- Uploading into this folder ----

  $('.upload').addEventListener('click', () => picker.click());
  picker.addEventListener('change', () => {
    if (picker.files.length) send([...picker.files]);
    picker.value = '';
  });
  function send(files) {
    upload(files, path);
    toast({ text: `Uploading ${files.length === 1 ? files[0].name : `${files.length} files`} to ${path || 'Home'}`, icon: 'file', timeout: 2500 });
  }
  let dragDepth = 0;
  sheet.addEventListener('dragenter', (e) => {
    if (![...(e.dataTransfer?.types ?? [])].includes('Files')) return;
    dragDepth++;
    drop.querySelector('span').textContent = `Drop to upload to ${path ? path.split('/').pop() : 'Home'}`;
    drop.classList.remove('hidden');
  });
  sheet.addEventListener('dragleave', () => {
    if (--dragDepth <= 0) {
      dragDepth = 0;
      drop.classList.add('hidden');
    }
  });
  sheet.addEventListener('dragover', (e) => {
    e.preventDefault();
    e.stopPropagation();
  });
  sheet.addEventListener('drop', async (e) => {
    e.preventDefault();
    e.stopPropagation(); // not also to Downloads (the page-wide drop)
    dragDepth = 0;
    drop.classList.add('hidden');
    const files = await droppedFiles(e.dataTransfer).catch(() => [...(e.dataTransfer?.files ?? [])]);
    if (files.length) send(files);
  });

  // ---- Keyboard: arrows, Enter, Backspace (up), Delete (Trash), F2 (rename) ----

  function up() {
    if (!path) return;
    go(path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
  }
  for (const type of ['keydown', 'keyup', 'keypress']) {
    // Typing here is for the browser, not the remote keyboard.
    sheet.addEventListener(type, (e) => e.stopPropagation());
  }
  sheet.addEventListener('keydown', (e) => {
    const inPrompt = e.target === promptInput;
    const inSearch = e.target === search;
    if (e.key === 'Escape') {
      e.preventDefault();
      if (inPrompt) closePrompt();
      else if (search.value) {
        search.value = '';
        applyFilter();
      } else setOpen(false);
      return;
    }
    if (inPrompt) return;
    const cols = perRow();
    const move = (d) => {
      e.preventDefault();
      if (!shown.length) return;
      const i = Math.min(shown.length - 1, Math.max(0, (selected < 0 ? 0 : selected + d)));
      select(i);
      // Keep it in view, from the known geometry (no layout reads).
      const y = Math.floor(i / cols) * rowHeight();
      if (y < scroll.scrollTop) scroll.scrollTop = y;
      else if (y + rowHeight() > scroll.scrollTop + size[1]) scroll.scrollTop = y + rowHeight() - size[1];
    };
    if (e.key === 'ArrowDown') return move(cols);
    if (e.key === 'ArrowUp') return move(-cols);
    if (view === 'grid' && e.key === 'ArrowRight' && !inSearch) return move(1);
    if (view === 'grid' && e.key === 'ArrowLeft' && !inSearch) return move(-1);
    if (e.key === 'Enter' && selected >= 0) {
      e.preventDefault();
      return open(selected);
    }
    if (inSearch) return;
    if ((e.key === 'Backspace' && !e.metaKey) || (e.key === 'ArrowUp' && e.metaKey)) {
      e.preventDefault();
      return up();
    }
    if ((e.key === 'Delete' || (e.key === 'Backspace' && e.metaKey)) && selected >= 0) {
      e.preventDefault();
      return trashButton.click();
    }
    if (e.key === 'F2' && selected >= 0) {
      e.preventDefault();
      return renameButton.click();
    }
    // Typing a name filters (Cmd/Ctrl+F too).
    if ((e.key === 'f' && (e.metaKey || e.ctrlKey)) || (e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey)) {
      if (e.key === 'f' && (e.metaKey || e.ctrlKey)) e.preventDefault();
      search.focus();
    }
  });
  search.addEventListener('input', applyFilter);
  addEventListener('pointerdown', (e) => {
    if (isOpen() && !sheet.contains(e.target) && !button.contains(e.target)) setOpen(false);
  }, { capture: true });

  // ---- From the host ----

  let refreshTimer;
  return {
    /** Windows calls it the Recycle Bin. */
    setHostOs(os) {
      trashName = os === 'windows' ? 'Recycle Bin' : 'Trash';
      sheet.querySelector('.act.trash').textContent = `Move to ${trashName}`;
    },
    /** Owners with files allowed get the button. */
    setAvailable(on) {
      available = on;
      button.classList.toggle('hidden', !on);
      if (!on) setOpen(false);
    },
    open() {
      if (available) setOpen(true);
    },
    onMessage(msg) {
      if (msg.t === 'filesThumb') {
        thumbsInFlight = Math.max(0, thumbsInFlight - 1);
        thumbs.set(msg.path, msg.data ?? null);
        if (msg.data) {
          for (const [i, el] of rows) {
            const e = shown[i];
            if (e && join(path, e.name) === msg.path) el.querySelector('.pic').innerHTML = `<img class="thumb" src="${msg.data}" alt="" draggable="false">`;
          }
        }
        pumpThumbs();
        return;
      }
      // A folder listing.
      loading = false;
      if (msg.error) {
        toast({ text: msg.error, kind: 'error' });
        updateFooter();
        if (msg.path !== path) return;
      }
      const changed = msg.path !== path;
      const keep = !changed && shown[selected]?.name;
      path = msg.path;
      entries = msg.entries;
      ticket = msg.ticket || ticket;
      if (changed) {
        selected = -1;
        scroll.scrollTop = 0;
      }
      if (msg.truncated) toast({ text: `Showing the first ${entries.length} items in this folder.` });
      renderCrumbs();
      applyFilter();
      if (keep) {
        const i = shown.findIndex((e) => e.name === keep);
        if (i >= 0) select(i);
      }
    },
    /** A file landed on the computer: show it if it's here. */
    onFileSaved() {
      if (!isOpen()) return;
      clearTimeout(refreshTimer);
      refreshTimer = setTimeout(() => go(path), 300);
    },
  };
}
