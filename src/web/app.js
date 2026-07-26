let cwd = '';
let entries = [];                 // raw listing from the API
let view = [];                    // entries actually shown (hidden filtered + sorted)
let showHidden = false;           // include dotfiles (e.g. macOS "._" sidecars)
let filterText = '';              // toolbar filter (client-side name match)
let selection = new Set();        // selected item paths (rel from Home)
let selectMode = false;           // touch multi-select: taps toggle, checkboxes shown
let clipboard = null;             // { mode:'cut'|'copy', paths:[fullpath...] }
let viewMode = 'list';            // 'list' | 'grid'
let sortKey = 'name', sortDir = 1;
let lastIndex = null;             // for shift-range selection
const rowEls = new Map();         // item path → row/card element, for selection-only updates (issue #41)
let perm = 'read_write';          // live permission level from settings
let multiRoot = false;            // server has named mounts: '' is a read-only mount listing
let defaultFolder = '';           // folder to open after login
let editing = null;               // entry currently open in the text editor

const $ = (id) => document.getElementById(id);
const join = (dir, name) => dir ? dir + '/' + name : name;
const enc = encodeURIComponent;

// ---- item identity ----
// Entries from /api/list belong to cwd; hits from /api/find carry their own
// `parent`, so a result set spans folders and two hits can share a name.
// Everything that identifies an item — the selection, rowEls, the sort anchor,
// every URL built from it — therefore keys on the *path*, never the name.
const parentOf = (e) => e.parent !== undefined ? e.parent : cwd;
const pathOf = (e) => join(parentOf(e), e.name);

// ---- URL <-> folder routing (deep links, back/forward, login survival) ----
// The current folder lives in the address bar as `?path=<folder>`, so it can
// be bookmarked, shared, and restored by Back/Forward. Home is the bare `/`
// (no query). go() keeps the URL in sync; popstate replays history entries.
const urlForPath = (path) => path ? location.pathname + '?path=' + enc(path) : location.pathname;
const pathFromUrl = () => new URLSearchParams(location.search).get('path') || '';
// A 401 means the session is gone (e.g. the server restarted). Bounce to the
// login page but carry where we are as `next`, so re-authenticating returns to
// this exact folder instead of Home — the server redirects back to it.
function gotoLogin() { location.href = '/login?next=' + enc(location.pathname + location.search); }

// ---- file type helpers ----
// These extension lists are duplicated elsewhere and must stay in rough sync:
// the server's text/preview gate (src/files.rs) and the device bounce icons
// (src/bounce.rs). There's no shared source today — change one, check both.
const IMG = ['png','jpg','jpeg','gif','bmp','webp','svg'];
const VID = ['mp4','m4v','webm','mov','mkv'];
const AUD = ['mp3','wav','ogg','flac','m4a','aac'];
const TXT = ['txt','md','cfg','ini','log','conf','sh','json','xml','csv'];
const ext = (n) => n.includes('.') ? n.split('.').pop().toLowerCase() : '';
const isImg = (e) => !e.dir && IMG.includes(ext(e.name));
const isVid = (e) => !e.dir && VID.includes(ext(e.name));
const isAud = (e) => !e.dir && AUD.includes(ext(e.name));
const isTxt = (e) => !e.dir && TXT.includes(ext(e.name));
const previewable = (e) => isImg(e) || isVid(e) || isAud(e) || isTxt(e);

const TYPES = { png:'Image',jpg:'Image',jpeg:'Image',gif:'Image',bmp:'Image',webp:'Image',svg:'Image',
  mp3:'Audio',wav:'Audio',ogg:'Audio',flac:'Audio',m4a:'Audio',aac:'Audio',
  mp4:'Video',m4v:'Video',mkv:'Video',avi:'Video',mov:'Video',webm:'Video',
  zip:'Archive','7z':'Archive',rar:'Archive',gz:'Archive', txt:'Text',md:'Text',cfg:'Config',ini:'Config',json:'JSON',
  sh:'Script', gb:'Game Boy',gbc:'Game Boy',gba:'GBA',nes:'NES',smc:'SNES',sfc:'SNES',n64:'N64',iso:'Disc Image',chd:'Disc Image' };
function typeOf(name, dir) {
  if (dir) return 'Folder';
  const e = ext(name);
  return TYPES[e] || (e ? e.toUpperCase() + ' File' : 'File');
}

// ---- icons ----
const ICON_DIR = '<svg class="ic dir" viewBox="0 0 24 24" fill="currentColor"><path d="M10 4H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-8l-2-2z"/></svg>';
const ICON_FILE = '<svg class="ic file" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/></svg>';
const rawUrl = (e) => '/api/raw?path=' + enc(pathOf(e));
// Server-side downscaled thumbnail (issue #28): the grid no longer pulls the
// full original per 128 px cell. One fixed width — the largest cell at 2x
// DPR — so the list and grid views share a single browser-cache entry per
// file instead of re-fetching per size.
const thumbUrl = (e) => '/api/thumb?path=' + enc(pathOf(e)) + '&w=256';
// Build the generic file icon as a DOM node, used as a thumbnail fallback.
function fileIcon() { const d = document.createElement('div'); d.innerHTML = ICON_FILE; return d.firstChild; }
// A thumbnail failed: first retry once with the original via /api/raw —
// /api/thumb 415s on formats its decoders don't know (SVG renders natively
// in the browser) — then, if the original doesn't decode either (e.g. a
// macOS "._" AppleDouble file named .png), swap in the file icon. Done in JS
// rather than an inline onerror that embeds the SVG — quoting an SVG (which
// contains ") inside an HTML attribute broke the row markup and leaked a
// stray '"> as text.
function thumbFail(img) {
  const raw = img.dataset.raw;
  if (raw) { img.removeAttribute('data-raw'); img.src = raw; return; }
  img.replaceWith(fileIcon());
}

// ---- helpers ----
function toast(msg, isErr) {
  const t = $('toast'); t.textContent = msg; t.className = 'toast show' + (isErr ? ' err' : '');
  clearTimeout(toast._t); toast._t = setTimeout(() => t.className = 'toast', isErr ? 4000 : 2000);
}
function humanSize(n) {
  if (n < 1024) return n + ' B';
  const u = ['KiB','MiB','GiB','TiB']; let i = -1; do { n /= 1024; i++; } while (n >= 1024 && i < u.length-1);
  return n.toFixed(n < 10 ? 1 : 0) + ' ' + u[i];
}
function fmtDate(ms) { if (!ms) return ''; const d = new Date(ms);
  return d.toLocaleDateString(undefined,{day:'2-digit',month:'short',year:'numeric'}) + ' ' +
         d.toLocaleTimeString(undefined,{hour:'2-digit',minute:'2-digit'}); }
function escapeHtml(s){return s.replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));}

async function api(method, url, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body); }
  const r = await fetch(url, opts);
  if (r.status === 401) { gotoLogin(); throw new Error('unauthorized'); }
  // Keep the HTTP status on the error so callers can react to specific
  // failures (doPaste turns a 409 collision into an overwrite prompt).
  if (!r.ok) { const err = new Error((await r.text()) || r.statusText); err.status = r.status; throw err; }
  const ct = r.headers.get('content-type') || '';
  return ct.includes('json') ? r.json() : r.text();
}

// ---- visible set (hidden filter + name filter) + sorting ----
// Recompute `view` from the raw `entries`: drop dotfiles unless showHidden,
// apply the toolbar name filter (case-insensitive substring), then sort.
// Call after a new listing, a filter/hidden change — sort-only changes go
// straight to sortView(), which keeps operating on the already-narrowed view.
function rebuild() {
  const base = showHidden ? entries.slice() : entries.filter(e => !e.name.startsWith('.'));
  // In search mode the server already applied the pattern (which may be a
  // glob the substring test would reject), so the local filter stands down —
  // `entries` *is* the match set.
  const q = inSearch() ? '' : filterText.trim().toLowerCase();
  view = q ? base.filter(e => e.name.toLowerCase().includes(q)) : base;
  // "N of M" while filtering (M = what the folder would show unfiltered);
  // the plain hit count while searching (the banner carries the detail).
  $('t-count').textContent = inSearch() ? view.length + ' found'
    : (q ? view.length + ' of ' + base.length : '');
  sortView();
  render();
}

// Set the filter text, keeping the input in sync (used by navigation resets
// and Esc; user typing flows through the input's own oninput instead).
function setFilter(text) { filterText = text; $('t-filter').value = text; }

// Filter and hidden-toggle changes reshuffle `view` indices and can hide
// selected rows, so they drop the selection (same policy as t-hidden).
// Typing also leaves search results: the box means "filter this folder"
// again, and the parked listing is right there, so the switch is instant.
function applyFilter(text) {
  filterText = text;
  exitSearch();
  selection = new Set(); lastIndex = null;
  rebuild(); syncToolbar();
}

// ---- recursive search (Enter in the filter box) ----
// The filter box narrows the current folder as you type; Enter hands the same
// text to /api/find, which walks every folder below cwd. Results replace the
// listing until cleared — the folder's own entries are parked rather than
// refetched, so returning to them costs nothing.
// One request is one page: the server stops at its own caps (it is walking an
// SD card, or a Steam Deck home directory with a million entries in it) and
// hands back a cursor. `searchCursor` set means the walk paused with more tree
// left — the banner's Continue button resumes it and appends the next page.
let searchQ = '';                 // active pattern; '' = plain folder listing
let searchRoot = '';              // folder the active search was launched from
let parkedEntries = null;         // the cwd listing, held while results show
let searchToken = 0;              // discards a superseded search response
let searchCursor = null;          // resume point, or null when nothing is left
let searchScanned = 0;            // entries examined, accumulated over pages
let searchLimit = null;           // which cap paused the last page
let searchBusy = false;           // a page is in flight (Continue is disabled)

const inSearch = () => searchQ !== '';

// How complete the answer is. A paused walk says so plainly: silence would
// read as "that's all there is", which is the one thing a search must never
// imply when it stopped early.
function searchNote() {
  const n = entries.length;
  const bits = [n + (n === 1 ? ' match' : ' matches'),
                'scanned ' + searchScanned.toLocaleString() + ' items'];
  if (searchBusy) bits.push('searching…');
  else if (searchCursor) bits.push('paused — Continue to search further');
  else if (searchLimit === 'depth') bits.push('some folders were nested too deeply to search');
  return bits.join(' · ');
}

// Fetch one page. `cursor` null starts fresh (replacing any results); a cursor
// appends the next page and keeps the selection, since the rows already shown
// do not move.
async function fetchSearchPage(q, root, cursor) {
  const stok = ++searchToken, ntok = navToken;
  searchBusy = true;
  if (cursor) renderSearchBar(); else $('t-count').textContent = 'searching…';
  let res;
  try {
    res = await api('GET', '/api/find?path=' + enc(root) + '&q=' + enc(q)
                    + (cursor ? '&after=' + enc(cursor) : ''));
  } catch (e) {
    searchBusy = false;
    if (stok === searchToken) { toast(e.message, true); renderSearchBar(); rebuild(); }
    return;
  }
  searchBusy = false;
  // A newer search, or any navigation, owns the UI now.
  if (stok !== searchToken || ntok !== navToken) return;
  if (cursor) {
    entries = entries.concat(res.hits);
    searchScanned += res.scanned;
  } else {
    if (!inSearch()) parkedEntries = entries;   // first search: park the listing
    searchQ = q; searchRoot = root;
    entries = res.hits;
    searchScanned = res.scanned;
    selection = new Set(); lastIndex = null;
  }
  searchCursor = res.cursor || null;
  searchLimit = res.limit || null;
  renderSearchBar(); rebuild(); syncToolbar();
}

function runSearch(pattern) {
  const q = pattern.trim();
  // Enter on an empty box means "stop searching", not "match everything".
  if (!q) { if (exitSearch()) { rebuild(); syncToolbar(); } return; }
  return fetchSearchPage(q, cwd, null);
}

function continueSearch() {
  if (!inSearch() || !searchCursor || searchBusy) return;
  return fetchSearchPage(searchQ, searchRoot, searchCursor);
}

// Drop the results and put the parked folder listing back. Returns true when
// there was a search to leave, so callers can skip a needless repaint.
function exitSearch() {
  if (!inSearch()) return false;
  searchQ = ''; searchRoot = ''; searchCursor = null; searchLimit = null; searchScanned = 0;
  entries = parkedEntries || []; parkedEntries = null;
  selection = new Set(); lastIndex = null;
  renderSearchBar();
  return true;
}

function renderSearchBar() {
  const bar = $('sbar');
  if (!inSearch()) { bar.style.display = 'none'; return; }
  $('sb-text').textContent =
    '🔍 "' + searchQ + '" in ' + (searchRoot ? '/' + searchRoot : 'Home') + ' — ' + searchNote();
  const more = $('sb-more');
  more.style.display = searchCursor ? '' : 'none';
  more.disabled = searchBusy;
  bar.style.display = '';
}

// Jump from a hit to the folder that holds it, keeping it selected — the
// "where is this?" answer, and the way back to the write actions.
async function doReveal() {
  const path = [...selection][0]; if (!path) return;
  const e = view.find(x => pathOf(x) === path); if (!e) return;
  await go(parentOf(e));
  selection = new Set([path]);
  const idx = view.findIndex(x => pathOf(x) === path);
  lastIndex = idx >= 0 ? idx : null;      // -1 = hidden by the dotfile toggle
  syncSelection(); syncToolbar();
}
function sortView() {
  view.sort((a, b) => {
    if (a.dir !== b.dir) return a.dir ? -1 : 1;       // folders always first
    let c = 0;
    if (sortKey === 'size') c = a.size - b.size;
    else if (sortKey === 'date') c = a.modified - b.modified;
    else if (sortKey === 'type') c = typeOf(a.name,a.dir).localeCompare(typeOf(b.name,b.dir));
    if (c === 0) c = a.name.toLowerCase().localeCompare(b.name.toLowerCase());
    return c * sortDir;
  });
}

// ---- rendering ----
function renderCrumbs() {
  const c = $('crumbs'); c.innerHTML = '';
  const segs = cwd ? cwd.split('/') : [];
  const mk = (label, path, last) => {
    const s = document.createElement('span'); s.className = 'seg' + (last ? ' last' : '');
    s.textContent = label; if (!last) s.onclick = () => go(path);
    // Non-last crumbs double as move targets: dropping rows on an ancestor
    // moves them up the tree (the last crumb is cwd — a no-op, not a target).
    // The multi-root Home crumb is the read-only mount listing, never a target.
    if (!last && !(multiRoot && path === '')) {
      s.ondragover = (ev) => { if (!dragNames) return; ev.preventDefault(); ev.stopPropagation();
        ev.dataTransfer.dropEffect = 'move'; s.classList.add('droptgt'); };
      s.ondragleave = () => s.classList.remove('droptgt');
      s.ondrop = (ev) => { if (!dragNames) return; ev.preventDefault(); ev.stopPropagation();
        s.classList.remove('droptgt'); moveDragged(path); };
    }
    c.appendChild(s);
  };
  mk('Home', '', segs.length === 0);
  let acc = '';
  segs.forEach((seg, i) => {
    const sep = document.createElement('span'); sep.className = 'sep'; sep.textContent = ' / '; c.appendChild(sep);
    acc = acc ? acc + '/' + seg : seg;
    mk(seg, acc, i === segs.length - 1);
  });
}

function attachRowEvents(el, e, idx) {
  rowEls.set(pathOf(e), el);      // selection updates re-style this element in place
  el.dataset.idx = idx;           // lets the long-press recognizer map element → view[idx]
  // In select mode every click/tap toggles — double-click open is off, like
  // iOS Files; use the Open button (or leave select mode) to enter folders.
  el.onclick = (ev) => { if (!selectMode && ev.detail >= 2) { onRowOpen(e); return; } onRowClick(ev, e, idx); };
  el.oncontextmenu = (ev) => { ev.preventDefault();
    if (!selection.has(pathOf(e))) { selection = new Set([pathOf(e)]); lastIndex = idx; syncSelection(); syncToolbar(); }
    openMenu(ev.clientX, ev.clientY); };
  // Drag-to-move (issue #44): every row drags; folder rows accept drops.
  el.draggable = canWriteHere();
  el.ondragstart = (ev) => dragStart(ev, e, el);
  el.ondragend = () => dragEnd(el);
  if (e.dir) {
    el.ondragover = (ev) => dragOverFolder(ev, e, el);
    el.ondragleave = () => el.classList.remove('droptgt');
    el.ondrop = (ev) => dropOnFolder(ev, e, el);
  }
}

function render() { rowEls.clear(); viewMode === 'grid' ? renderGrid() : renderList(); }

// Empty-state text: distinguish "this folder is empty" from "the filter
// matched nothing" so an active filter can't read as an empty folder — and, on
// a paused search, from "nothing found *so far*", which is not the same claim
// as "nothing is there".
function emptyHtml() {
  const msg = inSearch() && searchCursor ? '— nothing yet — press Continue to search further —'
    : (inSearch() || filterText.trim() ? '— no matches —' : '— empty —');
  return `<div class="empty">${msg}</div>`;
}

// Selection-only repaint (issue #41): toggle the `sel` class on the existing
// rows/cards instead of rebuilding the whole DOM — in a 3,000-entry folder a
// full render() per click is hundreds of ms of layout jank on a phone. Full
// render() remains for anything that changes row content or order (listing,
// sort, view mode, the select-mode checkboxes).
function syncSelection() {
  rowEls.forEach((el, path) => el.classList.toggle('sel', selection.has(path)));
}

// The folder a search hit lives in, shown on the row so a flat result set
// still says *where* each match is. '' (a hit at the search root's own level)
// reads as Home, matching the breadcrumbs.
const rowPathLabel = (e) => parentOf(e) || 'Home';

function renderList() {
  const arrow = (k) => sortKey === k ? `<span class="arrow">${sortDir>0?'▲':'▼'}</span>` : '';
  const wrap = $('listwrap');
  wrap.innerHTML =
    `<table><thead><tr>
      <th class="name sortable" data-k="name">Name${arrow('name')}</th>
      <th class="size sortable" data-k="size">Size${arrow('size')}</th>
      <th class="type sortable" data-k="type">Type${arrow('type')}</th>
      <th class="date sortable" data-k="date">Modified${arrow('date')}</th>
    </tr></thead><tbody id="rows"></tbody></table>` +
    (view.length ? '' : emptyHtml());
  wrap.querySelectorAll('th.sortable').forEach(th => th.onclick = () => setSort(th.dataset.k));
  const tb = $('rows');
  view.forEach((e, idx) => {
    const tr = document.createElement('tr');
    if (selection.has(pathOf(e))) tr.classList.add('sel');
    const icon = e.dir ? ICON_DIR : (isImg(e)
      ? `<img class="thumb" loading="lazy" src="${thumbUrl(e)}" data-raw="${rawUrl(e)}" onerror="thumbFail(this)">`
      : ICON_FILE);
    const ck = selectMode ? '<span class="ck">✓</span>' : '';
    const rp = inSearch() ? `<span class="rpath">${escapeHtml(rowPathLabel(e))}</span>` : '';
    tr.innerHTML =
      `<td class="name">${ck}${icon}<span class="nm">${escapeHtml(e.name)}</span>${rp}</td>` +
      `<td class="size">${e.dir ? '' : humanSize(e.size)}</td>` +
      `<td class="type">${typeOf(e.name, e.dir)}</td>` +
      `<td class="date">${fmtDate(e.modified)}</td>`;
    attachRowEvents(tr, e, idx);
    tb.appendChild(tr);
  });
}

function renderGrid() {
  const wrap = $('listwrap');
  if (!view.length) { wrap.innerHTML = emptyHtml(); return; }
  const grid = document.createElement('div'); grid.className = 'grid';
  view.forEach((e, idx) => {
    const card = document.createElement('div'); card.className = 'gcard' + (selection.has(pathOf(e)) ? ' sel' : '');
    const inner = e.dir ? ICON_DIR : (isImg(e)
      ? `<img loading="lazy" src="${thumbUrl(e)}" data-raw="${rawUrl(e)}" onerror="thumbFail(this)">`
      : ICON_FILE);
    const ck = selectMode ? '<span class="ck">✓</span>' : '';
    const rp = inSearch() ? `<div class="gpath">${escapeHtml(rowPathLabel(e))}</div>` : '';
    card.innerHTML = `${ck}<div class="gthumb">${inner}</div><div class="gname">${escapeHtml(e.name)}</div>${rp}`;
    attachRowEvents(card, e, idx);
    grid.appendChild(card);
  });
  wrap.innerHTML = ''; wrap.appendChild(grid);
}

function setSort(k) {
  if (sortKey === k) sortDir = -sortDir; else { sortKey = k; sortDir = 1; }
  // Keep the shift-range anchor on the same *entry* across the re-sort: the
  // anchor is an index into `view`, so re-sorting under it would silently
  // re-point it at whatever row landed there and the next shift-click would
  // select a wrong contiguous range (issue #41).
  const anchor = lastIndex !== null && view[lastIndex] ? pathOf(view[lastIndex]) : null;
  sortView();
  lastIndex = anchor !== null ? view.findIndex(e => pathOf(e) === anchor) : null;
  if (lastIndex === -1) lastIndex = null;
  render();
}
function setView(v) {
  viewMode = v; $('v-list').classList.toggle('on', v === 'list'); $('v-grid').classList.toggle('on', v === 'grid');
  render();
}

function canWrite() { return perm !== 'read_only'; }
function canDelete() { return perm === 'read_write_delete'; }
// In multi-root mode '' is the synthetic mount listing — read-only on the
// server (issue #76), so every write affordance is disabled there. Copying a
// whole mount stays allowed (pasting it *inside* a mount works).
function atVirtualRoot() { return multiRoot && !cwd; }
// Search results are read-and-navigate only: the rows come from many folders,
// so "the current folder" — the destination a new folder, an upload, a paste
// or a rename would land in — is ambiguous. Reveal a hit (or open its folder)
// to get the write affordances back.
function canWriteHere() { return canWrite() && !atVirtualRoot() && !inSearch(); }

function syncToolbar() {
  const n = selection.size, w = canWriteHere();
  $('t-up').disabled = !w;
  $('t-updir').disabled = !w;
  $('t-mkdir').disabled = !w;
  $('t-newfile').disabled = !w;
  $('t-open').disabled = n !== 1;
  $('t-dl').disabled = n < 1;   // download (read) always allowed
  $('t-rename').disabled = n !== 1 || !w;
  $('t-cut').disabled = n < 1 || !w;
  $('t-copy').disabled = n < 1 || !canWrite() || inSearch();
  $('t-del').disabled = n < 1 || !canDelete() || atVirtualRoot() || inSearch();
  $('t-paste').disabled = !clipboard || !w;
}

// ---- navigation / selection ----
// All listing refreshes (navigation, refresh button, post-mutation reloads)
// route through go(); navToken discards /api/list responses that resolve
// after a newer go() started, so a slow response can never overwrite the
// entries of the directory the user is actually in (mirrors previewToken).
let navToken = 0;
// `push` (default) adds a history entry so Back returns to the previous
// folder; it's false for the initial load and for popstate-driven navigation
// (we're already moving *through* history then, not adding to it). Refreshes
// of the same folder never push — the path is unchanged.
async function go(path, push = true) {
  const token = ++navToken;
  const changed = path !== cwd;
  // Entering a different folder clears the filter; a refresh of the same
  // folder (t-refresh, post-mutation reloads) keeps it active.
  if (changed) setFilter('');
  // Any navigation ends a search: the listing below replaces the results, so
  // the parked entries are dropped rather than restored.
  searchQ = ''; searchRoot = ''; searchCursor = null; searchLimit = null;
  searchScanned = 0; parkedEntries = null;
  renderSearchBar();
  if (push && changed) history.pushState({ path }, '', urlForPath(path));
  cwd = path; selection = new Set(); lastIndex = null;
  try {
    const list = await api('GET', '/api/list?path=' + enc(cwd));
    if (token !== navToken) return;        // superseded by a newer navigation
    entries = list;
  } catch (e) {
    if (token !== navToken) return;        // stale failure — newer go() owns the UI
    toast(e.message, true); entries = [];
  }
  renderCrumbs(); rebuild(); syncToolbar();
}
// Refresh means "redo what is on screen": re-run the active search, otherwise
// re-list the current folder.
function doRefresh() { inSearch() ? runSearch(searchQ) : go(cwd); }

function onRowClick(ev, e, idx) {
  const p = pathOf(e);
  if (selectMode) {               // touch multi-select: every tap toggles (issue #31)
    selection.has(p) ? selection.delete(p) : selection.add(p); lastIndex = idx;
  } else if (ev.shiftKey && lastIndex !== null) {
    const [lo, hi] = [Math.min(lastIndex, idx), Math.max(lastIndex, idx)];
    selection = new Set(); for (let i = lo; i <= hi; i++) selection.add(pathOf(view[i]));
  } else if (ev.ctrlKey || ev.metaKey) {
    selection.has(p) ? selection.delete(p) : selection.add(p); lastIndex = idx;
  } else { selection = new Set([p]); lastIndex = idx; }
  syncSelection(); syncToolbar();
}
function onRowOpen(e) {
  if (e.dir) go(pathOf(e));
  else if (previewable(e)) openPreview(e);
  else downloadPaths([pathOf(e)]);
}

// ---- actions ----
// The selection already holds paths, and `view` is what can be selected —
// including search hits, which live outside cwd.
function selectedPaths() { return [...selection]; }
const selectedEntries = () => view.filter(e => selection.has(pathOf(e)));
function doOpen() {
  const sel = selectedEntries(); if (sel.length !== 1) return;
  onRowOpen(sel[0]);
}
function downloadPaths(paths) {
  paths.forEach((p, i) => setTimeout(() => {
    const a = document.createElement('a'); a.href = '/api/download?path=' + enc(p); a.download = '';
    document.body.appendChild(a); a.click(); a.remove();
  }, i * 400));
}
// base64 of a UTF-8 JSON array (handles non-Latin filenames).
function b64json(arr) { return btoa(unescape(encodeURIComponent(JSON.stringify(arr)))); }
function downloadZip(paths) {
  const a = document.createElement('a');
  a.href = '/api/zip?p=' + enc(b64json(paths)); a.download = '';
  document.body.appendChild(a); a.click(); a.remove();
}
// Single file → direct download; folders or multiple items → a zip.
function doDownload() {
  const sel = selectedEntries(); if (!sel.length) return;
  if (sel.length === 1 && !sel[0].dir) { downloadPaths([pathOf(sel[0])]); return; }
  toast('Preparing zip…');
  downloadZip(selectedPaths());
}
async function doMkdir() {
  const name = prompt('New folder name:'); if (!name) return;
  try { await api('POST', '/api/mkdir', { path: cwd, name }); toast('Folder created'); go(cwd); }
  catch (e) { toast(e.message, true); }
}
// Create an empty file and drop straight into the editor — the editor is the
// way to maintain config.json in place, but it could only open files that
// already existed (issue #57).
async function doNewFile() {
  const name = prompt('New file name:'); if (!name) return;
  // Refuse an existing name outright: an empty PUT over a file would
  // truncate it, and "new file" can only ever mean create — so no overwrite
  // offer, unlike uploads. The server still 409s as the backstop (no
  // &overwrite), e.g. when another client created the name after our listing.
  if (entries.some(e => e.name === name)) {
    toast('"' + name + '" already exists' + (entries.find(e => e.name === name).dir ? ' (folder)' : ''), true);
    return;
  }
  try {
    // fetch, not api(): the upload endpoint takes query params + raw body.
    // Empty body = create empty file; safe_name validates server-side.
    const r = await fetch('/api/upload?path=' + enc(cwd) + '&name=' + enc(name), { method: 'PUT', body: '' });
    if (r.status === 401) { gotoLogin(); return; }
    if (!r.ok) throw new Error((await r.text()) || r.statusText);
  } catch (e) { toast(e.message, true); return; }
  toast('Created ' + name);
  // Make the new file visible and selected: drop a filter that would hide
  // it, then refresh (go() clears the selection, so select after the await).
  const q = filterText.trim().toLowerCase();
  if (q && !name.toLowerCase().includes(q)) setFilter('');
  await go(cwd);
  selection = new Set([join(cwd, name)]);
  const idx = view.findIndex(e => e.name === name);
  lastIndex = idx >= 0 ? idx : null;       // -1 = dotfile hidden by the toggle
  syncSelection(); syncToolbar();
  // openEditor guards unsaved changes in an already-open buffer; the file is
  // created either way, so declining just leaves it in the listing.
  openEditor({ name });
}
async function doRename() {
  const path = [...selection][0]; if (!path) return;
  const cur = path.split('/').pop();
  const name = prompt('Rename to:', cur); if (!name || name === cur) return;
  try { await api('POST', '/api/rename', { path, name }); toast('Renamed'); go(cwd); }
  catch (e) { toast(e.message, true); }
}
async function doDelete() {
  const paths = selectedPaths(); if (!paths.length) return;
  if (!confirm('Delete ' + paths.length + ' item(s)? This cannot be undone.')) return;
  try { await api('POST', '/api/delete', { paths }); toast('Deleted'); go(cwd); refreshDisk(); }
  catch (e) { toast(e.message, true); }
}
// Web-UI path (what the breadcrumbs show), not the server's filesystem path:
// selectedPaths() is already cwd-relative, so a leading '/' makes it read as
// an absolute path rooted at Home. Multiple selections copy one per line.
async function doCopyPath() {
  const paths = selectedPaths(); if (!paths.length) return;
  const text = paths.map(p => '/' + p).join('\n');
  const ok = await copyText(text);
  toast(ok ? 'Copied path' + (paths.length > 1 ? 's' : '') : 'Copy failed', !ok);
}
// Clipboard write with a fallback for the plain-HTTP LAN case: the async
// Clipboard API only exists in a secure context, so on http://<ip> it's
// undefined and we drop to a hidden-textarea + execCommand('copy').
async function copyText(text) {
  try {
    if (navigator.clipboard && isSecureContext) { await navigator.clipboard.writeText(text); return true; }
  } catch { /* fall through to the legacy path */ }
  try {
    const ta = document.createElement('textarea');
    ta.value = text; ta.style.position = 'fixed'; ta.style.opacity = '0';
    document.body.appendChild(ta); ta.focus(); ta.select();
    const ok = document.execCommand('copy'); ta.remove(); return ok;
  } catch { return false; }
}
function doCut() { clipboard = { mode: 'cut', paths: selectedPaths() }; toast('Cut ' + clipboard.paths.length + ' item(s)'); syncToolbar(); }
function doCopy() { clipboard = { mode: 'copy', paths: selectedPaths() }; toast('Copied ' + clipboard.paths.length + ' item(s)'); syncToolbar(); }
async function doPaste() {
  if (!clipboard) return;
  const ep = clipboard.mode === 'cut' ? '/api/move' : '/api/copy';
  const body = { srcs: clipboard.paths, dest: cwd };
  try {
    try { await api('POST', ep, body); }
    catch (e) {
      // 409 = name collision(s); the server validated the whole batch before
      // touching anything, so one confirm covers it and the retry redoes the
      // lot. Same-file copies come back as 400 (never retried — overwriting
      // a file with itself is the truncation bug) and land in the outer catch.
      if (e.status !== 409) throw e;
      if (!confirm(e.message + '\n\nOverwrite?')) return;
      await api('POST', ep, { ...body, overwrite: true });
    }
  } catch (e) { toast(e.message, true); return; }
  toast(clipboard.mode === 'cut' ? 'Moved' : 'Copied');
  if (clipboard.mode === 'cut') clipboard = null;
  syncToolbar(); go(cwd);
}

// ---- drag-and-drop move (issue #44) ----
// Rows drag within the list; folder rows and breadcrumb segments accept
// drops and run the same /api/move + overwrite-confirm flow as Cut/Paste.
// Internal drags carry a custom DataTransfer type and never 'Files', so the
// OS-file upload overlay (which keys on 'Files') stays out of the way; the
// reverse holds too — OS-file drags leave `dragNames` null, so rows ignore
// them and the drop falls through to the upload path. HTML5 DnD never fires
// from touch, so phones keep Cut/Paste + long-press unchanged.
const DRAG_TYPE = 'application/x-amberdav-move';
let dragNames = null;               // paths being dragged; null = not our drag

function dragStart(ev, e, el) {
  if (!canWriteHere()) { ev.preventDefault(); return; }
  // Dragging a selected row carries the whole selection; dragging an
  // unselected row moves just that row (the selection is left alone).
  dragNames = selection.has(pathOf(e)) ? [...selection] : [pathOf(e)];
  ev.dataTransfer.setData(DRAG_TYPE, JSON.stringify(dragNames));
  ev.dataTransfer.effectAllowed = 'move';
  el.classList.add('dragging');
}
function dragEnd(el) { dragNames = null; el.classList.remove('dragging'); }

// A folder row is a valid target unless it is itself being dragged (folder
// into itself). Deeper cases (same-location no-ops, symlinked aliases) are
// validated server-side by plan_transfer before anything is touched.
function validRowTarget(e) { return !!dragNames && e.dir && !dragNames.includes(pathOf(e)); }

function dragOverFolder(ev, e, el) {
  if (!validRowTarget(e)) return;   // no preventDefault → browser shows "no drop"
  ev.preventDefault(); ev.stopPropagation();
  ev.dataTransfer.dropEffect = 'move';
  el.classList.add('droptgt');
}
function dropOnFolder(ev, e, el) {
  if (!validRowTarget(e)) return;
  ev.preventDefault(); ev.stopPropagation();
  el.classList.remove('droptgt');
  moveDragged(pathOf(e));
}

async function moveDragged(dest) {
  const names = dragNames; dragNames = null;
  if (!names || !names.length) return;
  const body = { srcs: names, dest };
  try {
    try { await api('POST', '/api/move', body); }
    catch (e) {
      // Same collision contract as Cut/Paste (issue #23): 409 names every
      // conflict and nothing has moved, so one confirm retries the whole
      // batch with overwrite; 400s (folder into itself, …) fall through to
      // the outer catch as a toast.
      if (e.status !== 409) throw e;
      if (!confirm(e.message + '\n\nOverwrite?')) return;
      await api('POST', '/api/move', { ...body, overwrite: true });
    }
  } catch (e) { toast(e.message, true); return; }
  toast('Moved ' + names.length + ' item(s)');
  selection = new Set(); lastIndex = null;    // the moved rows left this folder
  go(cwd); refreshDisk();
}

// ---- preview modal (navigable gallery) ----
let previewList = [], previewIdx = -1, previewToken = 0;
// Largest text file the preview pane will fetch whole; bigger ones say
// "download instead" rather than stalling the tab on a phone.
const TEXT_PREVIEW_MAX_BYTES = 2 * 1024 * 1024;

function openPreview(e) {
  // Gallery = all previewable items currently shown, in display order (a
  // search result set spans folders, so the gallery does too).
  previewList = view.filter(previewable);
  previewIdx = previewList.findIndex(x => pathOf(x) === pathOf(e));
  if (previewIdx < 0) { previewList = [e]; previewIdx = 0; }
  $('modal').classList.add('show');
  showPreviewAt(previewIdx);
}
function navigatePreview(delta) {
  if (previewIdx < 0 || previewList.length < 2) return;
  previewIdx = (previewIdx + delta + previewList.length) % previewList.length;  // wrap
  showPreviewAt(previewIdx);
}
async function showPreviewAt(i) {
  const e = previewList[i]; if (!e) return;
  const token = ++previewToken;                 // guards against stale async text loads
  const p = pathOf(e);
  const url = '/api/raw?path=' + enc(p);
  $('mtitle').textContent = e.name;
  $('mcount').textContent = previewList.length > 1 ? (i + 1) + ' / ' + previewList.length : '';
  $('mdl').href = '/api/download?path=' + enc(p);
  // Offer in-place editing for text files when writable.
  const medit = $('medit');
  if (isTxt(e) && canWriteHere()) { medit.style.display = ''; medit.onclick = (ev) => { ev.preventDefault(); closePreview(); openEditor(e); }; }
  else medit.style.display = 'none';
  const body = $('mbody'); body.innerHTML = '<div class="note">Loading…</div>';
  if (isImg(e)) body.innerHTML = `<img src="${url}">`;
  else if (isVid(e)) body.innerHTML = `<video src="${url}" controls autoplay></video>`;
  else if (isAud(e)) body.innerHTML = `<audio src="${url}" controls autoplay></audio>`;
  else if (isTxt(e)) {
    if (e.size > TEXT_PREVIEW_MAX_BYTES) { body.innerHTML = '<div class="note">File too large to preview — download instead.</div>'; return; }
    // cache:'no-cache' forces a conditional revalidation (If-None-Match) so an
    // in-app edit is reflected immediately — without it the browser's heuristic
    // freshness serves a stale copy of a recently-modified file (issue #104).
    try { const r = await fetch(url, { cache: 'no-cache' }); const t = await r.text();
      if (token !== previewToken) return;        // user navigated away while loading
      const pre = document.createElement('pre'); pre.textContent = t; body.innerHTML = ''; body.appendChild(pre); }
    catch (err) { if (token === previewToken) body.innerHTML = '<div class="note">' + escapeHtml(err.message) + '</div>'; }
  }
}
function closePreview() { $('modal').classList.remove('show'); $('mbody').innerHTML = ''; previewIdx = -1; previewList = []; }

// ---- text editor (config files etc.) ----
let edBaseline = null;            // editor text as last loaded/saved; null = nothing loaded yet
const editorDirty = () => editing !== null && edBaseline !== null && $('ed-area').value !== edBaseline;
// Single guard for every action that would throw away the buffer (✕ button,
// Esc, opening another file). Returns true when it is safe to proceed.
function confirmDiscard() {
  return !editorDirty() || confirm('Discard unsaved changes to ' + editing.name + '?');
}
async function openEditor(e) {
  if (!canWriteHere()) { toast('Read-only — editing is disabled', true); return; }
  if (!confirmDiscard()) return;
  editing = e; edBaseline = null;
  $('ed-title').textContent = e.name;
  const area = $('ed-area'); area.value = ''; area.disabled = true;
  $('editor').classList.add('show');
  // Fetch raw bytes directly (not via api(), which would JSON-parse .json).
  // cache:'no-cache' revalidates rather than trusting heuristic freshness, so
  // re-opening a file just edited in-app loads the new bytes (issue #104).
  try {
    const r = await fetch('/api/raw?path=' + enc(pathOf(e)), { cache: 'no-cache' });
    if (r.status === 401) { gotoLogin(); return; }
    if (!r.ok) throw new Error((await r.text()) || r.statusText);
    area.value = await r.text(); edBaseline = area.value; area.disabled = false; area.focus();
  } catch (err) { toast(err.message, true); closeEditor(); }
}
async function saveEditor() {
  if (!editing) return;
  // Write back to the file's own folder, which the upload endpoint takes
  // separately from the name (always cwd today — the editor is unreachable
  // from search results — but derived from the entry rather than assumed).
  const name = editing.name, dir = parentOf(editing), text = $('ed-area').value;
  try {
    // Saving rewrites the file being edited on purpose → overwrite=true
    // (without it the server now 409s instead of replacing an existing file).
    const r = await fetch('/api/upload?path=' + enc(dir) + '&name=' + enc(name) + '&overwrite=true',
                          { method: 'PUT', body: text });
    if (r.status === 401) { gotoLogin(); return; }
    if (!r.ok) throw new Error((await r.text()) || r.statusText);
    edBaseline = text;                // what we just persisted — editor is clean again
    toast('Saved ' + name); closeEditor(); go(cwd);
  } catch (err) { toast('Save failed: ' + err.message, true); }
}
// Unconditional teardown — callers that may discard edits go through requestCloseEditor().
function closeEditor() { $('editor').classList.remove('show'); $('ed-area').value = ''; editing = null; edBaseline = null; }
function requestCloseEditor() { if (confirmDiscard()) closeEditor(); }

// ---- uploads (XHR so we get upload progress; folders carry a rel dir) ----
function showUpload() { $('uploadbar').classList.add('show'); }
function hideUpload() { $('uploadbar').classList.remove('show'); $('ub-fill').style.width = '0'; }
function setUpload(name, idx, n, overallFrac, filePct) {
  $('ub-name').textContent = `${name}  (${idx}/${n})`;
  $('ub-pct').textContent = filePct + '%';
  $('ub-fill').style.width = (overallFrac * 100).toFixed(1) + '%';
}
// `rel` is the file's folder path under `dest` ('' = dest itself) — the
// server validates every segment and creates the missing folders (issue #30).
function putFile(dest, file, rel, onProgress, overwrite) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', '/api/upload?path=' + enc(dest) + '&name=' + enc(file.name)
                  + (rel ? '&dir=' + enc(rel) : '')
                  + (overwrite ? '&overwrite=true' : ''));
    xhr.upload.onprogress = (e) => { if (e.lengthComputable) onProgress(e.loaded / e.total); };
    xhr.onload = () => {
      if (xhr.status === 401) { gotoLogin(); return; }
      if (xhr.status >= 200 && xhr.status < 300) { resolve(); return; }
      // Keep the status so uploadItems can turn a 409 into an overwrite prompt.
      const err = new Error(xhr.responseText || xhr.statusText); err.status = xhr.status; reject(err);
    };
    xhr.onerror = () => reject(new Error('network error'));
    xhr.send(file);
  });
}
// Create `dest/rel` level by level via /api/mkdir — used for the *empty*
// directories of a dropped folder, which no file upload would create.
// "Already exists" (409) is success; anything else aborts the batch.
async function ensureDir(dest, rel) {
  let parent = dest;
  for (const seg of rel.split('/')) {
    if (!seg) continue;
    try { await api('POST', '/api/mkdir', { path: parent, name: seg }); }
    catch (e) { if (e.status !== 409) throw e; }
    parent = join(parent, seg);
  }
}
// Upload a batch into the current folder. `items` is [{file, rel}] where
// `rel` is the folder path each file keeps under the destination ('' for
// flat uploads); `emptyDirs` are folders with no files at all, recreated
// explicitly so a dropped tree arrives complete.
async function uploadItems(items, emptyDirs = []) {
  if (!items.length && !emptyDirs.length) return;
  const dest = cwd;                 // pin: the user may navigate away mid-batch
  showUpload();
  try { for (const d of emptyDirs) await ensureDir(dest, d); }
  catch (e) { hideUpload(); toast('Could not create folder: ' + e.message, true); go(cwd); return; }
  let done = 0, skipped = 0;
  let applyAll = null;              // remembered overwrite answer: true | false | null = ask
  for (const it of items) {
    const label = it.rel ? it.rel + '/' + it.file.name : it.file.name;
    const idx = done + skipped + 1;
    const overall = (frac) => (done + skipped + frac) / items.length;
    const prog = (frac) => setUpload(label, idx, items.length, overall(frac), Math.round(frac * 100));
    prog(0);
    try {
      try { await putFile(dest, it.file, it.rel, prog, applyAll === true); }
      catch (e) {
        // 409 = a file with this name already exists; the server refused to
        // clobber it. Ask — and since a folder drop can collide hundreds of
        // times, offer to reuse the answer for the rest of the batch.
        if (e.status !== 409) throw e;
        let ow = applyAll;
        if (ow === null) {
          ow = confirm('"' + label + '" already exists. Overwrite it?');
          if (items.length > 1 &&
              confirm((ow ? 'Overwrite' : 'Skip') + ' every other existing file in this upload too?'))
            applyAll = ow;
        }
        if (!ow) { skipped++; continue; }
        await putFile(dest, it.file, it.rel, prog, true);
      }
      done++;
    } catch (e) {
      // A mid-batch failure aborts the rest; say how far it got — "uploaded
      // X of N" — so the user knows which files made it (issue #24).
      hideUpload();
      toast('Upload failed on "' + label + '": ' + e.message
            + ' — uploaded ' + done + ' of ' + items.length
            + (skipped ? ' (skipped ' + skipped + ')' : ''), true);
      go(cwd); refreshDisk(); return;     // partial batches still wrote bytes
    }
  }
  hideUpload();
  const bits = ['Uploaded ' + done + ' file(s)'];
  if (skipped) bits.push('skipped ' + skipped);
  if (emptyDirs.length) bits.push('created ' + emptyDirs.length + ' empty folder(s)');
  toast(bits.join(', '));
  go(cwd); refreshDisk();
}
// Flat upload (file picker, non-folder drops): everything lands in cwd.
function uploadFiles(fileList) {
  return uploadItems([...fileList].map(f => ({ file: f, rel: '' })));
}
// Folder picker (webkitdirectory): each File carries webkitRelativePath
// ("Folder/sub/file.txt") — keep the dirname as the rel path. Note the
// picker only yields files, so empty folders inside the selection are not
// visible here (drag & drop does see them via the entry traversal below).
function uploadPickedFolder(fileList) {
  const items = [...fileList].map(f => {
    const p = f.webkitRelativePath || '';
    const i = p.lastIndexOf('/');
    return { file: f, rel: i < 0 ? '' : p.slice(0, i) };
  });
  return uploadItems(items);
}
// Resolve one FileSystemEntry's File object (callback API → promise).
function entryFile(entry) { return new Promise((res, rej) => entry.file(res, rej)); }
// Drain a directory reader: readEntries() returns at most ~100 entries per
// call and an empty batch only when exhausted, so it must be called in a loop.
function readAllEntries(reader) {
  return new Promise((resolve, reject) => {
    const out = [];
    const step = () => reader.readEntries((batch) => {
      if (!batch.length) { resolve(out); return; }
      out.push(...batch); step();
    }, reject);
    step();
  });
}
// Walk dropped FileSystemEntrys (webkitGetAsEntry) into the uploadItems
// shape: files with their folder path relative to the drop target, plus the
// folders that contain no files at all (recreated via mkdir).
async function collectDropped(entries) {
  const files = [], emptyDirs = [];
  const walk = async (entry, rel) => {
    if (entry.isFile) { files.push({ file: await entryFile(entry), rel }); return; }
    if (!entry.isDirectory) return;
    const sub = rel ? rel + '/' + entry.name : entry.name;
    const children = await readAllEntries(entry.createReader());
    if (!children.length) { emptyDirs.push(sub); return; }
    for (const c of children) await walk(c, sub);
  };
  for (const en of entries) await walk(en, '');
  return { files, emptyDirs };
}

// ---- context menu ----
function openMenu(x, y) {
  const m = $('menu'); const n = selection.size;
  const w = canWriteHere();
  // The single selected entry, if exactly one — used to gate text editing.
  const selEntry = n === 1 ? view.find(e => pathOf(e) === [...selection][0]) : null;
  const canEdit = !!(selEntry && isTxt(selEntry) && w);
  const items = [
    { label: '＋ New Folder', fn: doMkdir, off: !w },
    { label: '＋ New File', fn: doNewFile, off: !w },
    { label: '⬆ Upload Files', fn: () => $('filepick').click(), off: !w },
    { label: '⬆ Upload Folder', fn: () => $('dirpick').click(), off: !w },
    { label: '⬇ Download Selected', fn: doDownload, off: n < 1 },
    // Only meaningful for a search hit, which is the one case where a row is
    // not in the folder the breadcrumbs point at.
    { label: '📍 Reveal in Folder', fn: doReveal, off: !inSearch() || n !== 1 },
    'sep',
    { label: '✂ Cut', fn: doCut, off: n < 1 || !w },
    { label: '⧉ Copy', fn: doCopy, off: n < 1 || !canWrite() || inSearch() },
    { label: '📋 Copy Path', fn: doCopyPath, off: n < 1 },
    { label: '⤵ Paste', fn: doPaste, off: !clipboard || !w },
    'sep',
    { label: '🗑 Delete', fn: doDelete, off: n < 1 || !canDelete() || atVirtualRoot() || inSearch() },
    { label: '✎ Rename', fn: doRename, off: n !== 1 || !w },
    { label: '📝 Edit', fn: () => openEditor(selEntry), off: !canEdit },
    'sep',
    // Select mode is reachable from a long-press too, so the flow on a phone
    // is: long-press → "Select Items" → tap the rest → long-press → action.
    { label: selectMode ? '☑ Done Selecting' : '☑ Select Items', fn: () => setSelectMode(!selectMode) },
    { label: '↻ Refresh', fn: doRefresh },
  ];
  m.innerHTML = '';
  for (const it of items) {
    if (it === 'sep') { const s = document.createElement('div'); s.className = 'sep'; m.appendChild(s); continue; }
    const d = document.createElement('div'); d.className = 'mi' + (it.off ? ' disabled' : ''); d.textContent = it.label;
    if (!it.off) d.onclick = () => { closeMenu(); it.fn(); };
    m.appendChild(d);
  }
  m.style.display = 'block';
  const r = m.getBoundingClientRect();
  m.style.left = Math.min(x, innerWidth - r.width - 8) + 'px';
  m.style.top = Math.min(y, innerHeight - r.height - 8) + 'px';
}
function closeMenu() { $('menu').style.display = 'none'; }

// ---- touch: select mode + long-press menu (issue #31) ----
// Phones have no Ctrl/Shift-click and iOS Safari never fires contextmenu, so
// the QR-to-phone flow dead-ended at single-select with no menu. Two pieces:
// a "Select" toggle that turns every tap into a selection toggle (checkboxes
// appear in both views), and a long-press recognizer (pointer events + timer)
// that opens the existing context menu at the touch point. Mouse pointers are
// ignored throughout — desktop behavior is unchanged.
function setSelectMode(on) {
  selectMode = on;
  $('t-select').classList.toggle('on', on);
  if (!on) { selection = new Set(); lastIndex = null; }   // leaving = done with the batch
  render(); syncToolbar();
}

const LP_MS = 500, LP_SLOP = 10;    // hold time · movement tolerance (px)
let lpTimer = null, lpStart = null; // pending recognizer state
let lpSwallow = false;              // a long-press fired; eat the synthetic events that trail it
function cancelLP() { clearTimeout(lpTimer); lpTimer = null; lpStart = null; }
function fireLP() {
  const { x, y, row } = lpStart;
  cancelLP();
  lpSwallow = true;
  if (row) {
    const idx = +row.dataset.idx, e = view[idx];
    // Same rule as right-click: pressing outside the selection refocuses it,
    // pressing inside keeps the multi-selection for a bulk action.
    if (e && !selection.has(pathOf(e))) { selection = new Set([pathOf(e)]); lastIndex = idx; syncSelection(); syncToolbar(); }
  } else { selection = new Set(); syncSelection(); syncToolbar(); }   // background press, like blank-space right-click
  openMenu(x, y);
}
const lw = $('listwrap');
lw.addEventListener('pointerdown', (ev) => {
  if (ev.pointerType === 'mouse') return;   // mouse already has a real right-click
  if (lpTimer) { cancelLP(); return; }      // second finger → pinch/scroll, not a press
  lpStart = { id: ev.pointerId, x: ev.clientX, y: ev.clientY,
              row: ev.target.closest('#rows tr, .gcard') };
  lpTimer = setTimeout(fireLP, LP_MS);
});
lw.addEventListener('pointermove', (ev) => {
  if (!lpTimer || ev.pointerId !== lpStart.id) return;
  if (Math.hypot(ev.clientX - lpStart.x, ev.clientY - lpStart.y) > LP_SLOP) cancelLP();
});
lw.addEventListener('pointerup', cancelLP);
lw.addEventListener('pointercancel', cancelLP);   // the browser took the gesture (scroll/pinch)
// After a recognized long-press, browsers fire synthetic events on release —
// iOS a click, Android a native contextmenu (sometimes both). Left alone they
// would instantly close the just-opened menu (the document click handler), or
// "tap" whatever menu item rendered under the finger. Swallow them in the
// capture phase; any new pointerdown starts a fresh gesture and lifts the
// guard, so the user's next real tap lands normally.
document.addEventListener('pointerdown', () => { lpSwallow = false; }, true);
document.addEventListener('contextmenu', (ev) => {
  if (lpSwallow) { ev.preventDefault(); ev.stopPropagation(); return; }
  // Android's native long-press contextmenu can beat our timer: let the
  // normal contextmenu path open the menu, but stand down the pending timer
  // (no double-open) and still swallow the trailing click.
  if (lpTimer) { cancelLP(); lpSwallow = true; }
}, true);
document.addEventListener('click', (ev) => {
  if (lpSwallow) { lpSwallow = false; ev.preventDefault(); ev.stopPropagation(); }
}, true);

// ---- status tab (live input) ----
const held = new Map(), axes = new Map();
function renderHeld() { const b = $('buttons'); b.innerHTML = '';
  for (const l of held.values()) { const s = document.createElement('span'); s.className = 'pill on'; s.textContent = l; b.appendChild(s); } }
function renderAxes() { const a = $('axes'); a.innerHTML = '';
  for (const [k,v] of [...axes.entries()].sort()) { const kk=document.createElement('div'); kk.textContent=k;
    const vv=document.createElement('div'); vv.className='v'; vv.textContent=v; a.appendChild(kk); a.appendChild(vv); } }
function ilog(s){ const l=$('ilog'); l.textContent=(s+'\n'+l.textContent).slice(0,4000); }
let es = null;
function startInput() {
  if (es) return; es = new EventSource('/events');
  es.onmessage = (ev) => { const e = JSON.parse(ev.data); const id = e.device + ':' + e.name;
    if (e.kind === 'button') { e.state === 'down' ? held.set(id, e.name) : e.state === 'up' && held.delete(id); renderHeld(); ilog(e.name+' ('+e.code+') '+e.state); }
    else if (e.name.indexOf('HAT') !== -1) { e.value === 0 ? held.delete(id) : held.set(id, e.name+' '+(e.value>0?'+':'-')); renderHeld(); ilog(e.name+' = '+e.value); }
    else { axes.set(e.name, e.value); renderAxes(); } };
}
// ---- disk gauge (issue #43) ----
// Sidebar footer gauge from /api/info's disk_free/disk_total. The fields are
// null when the server can't report them (e.g. non-unix hosts) — then the
// gauge stays hidden and the Status line says "unknown". "Low" (red) means
// under 5% or under 1 GiB free, whichever is larger.
function diskText(i) {
  return i.disk_total ? humanSize(i.disk_free) + ' free of ' + humanSize(i.disk_total) : 'unknown';
}
function renderDisk(i) {
  const box = $('disk');
  if (!i || !i.disk_total) { box.style.display = 'none'; return; }
  const usedPct = Math.min(100, Math.max(0, (1 - i.disk_free / i.disk_total) * 100));
  const low = i.disk_free < Math.max(i.disk_total * 0.05, 1 << 30);
  $('disk-fill').style.width = usedPct.toFixed(1) + '%';
  $('disk-fill').classList.toggle('low', low);
  $('disk-text').textContent = (low ? '⚠ ' : '') + diskText(i);
  $('disk-text').classList.toggle('low', low);
  box.style.display = '';
}
// Refresh the gauge after anything that changes disk usage (upload batches,
// deletes) and once at startup. Failures keep the last shown value.
function refreshDisk() { api('GET', '/api/info').then(renderDisk).catch(() => {}); }

async function loadConn() {
  try { const i = await api('GET', '/api/info');
    // Browser tab title: use the configured device name as the subtitle so
    // multiple open instances can be told apart at a glance (issue #101).
    document.title = 'AmberDAV · ' + (i.name || 'web access');
    // A broken config.json boots with defaults; make that loud here since
    // stderr is invisible on a handheld (issue #19).
    const warn = $('conn-warn');
    if (i.config_error) {
      warn.textContent = '⚠ config.json not applied — ' + i.config_error;
      warn.style.display = '';
    } else { warn.style.display = 'none'; }
    renderDisk(i);
    // Built with DOM nodes, not an HTML string (issue #55): the values come
    // from the server's own config, but they must still render as text, and
    // escapeHtml is not attribute-safe for the href.
    const conn = $('conn');
    conn.textContent = '';
    const bold = (t) => { const b = document.createElement('b'); b.textContent = t; return b; };
    const dav = document.createElement('a');
    dav.setAttribute('href', i.dav);
    dav.textContent = i.dav;
    [['Device IP: ', bold(i.ip + ':' + i.port)],
     ['WebDAV mount: ', dav],
     ['Serving: ', i.root],
     ['Disk: ', bold(diskText(i))],
     ['Screen: ', i.screen],
    ].forEach(([label, value], idx) => {
      if (idx) conn.appendChild(document.createElement('br'));
      conn.append(label, value); // strings become text nodes, never markup
    });
    // Gamepad input only streams on device (fb/sdl) builds; elsewhere the card
    // would sit empty, so only reveal it and open the stream when supported.
    if (i.live_input) { $('card-live-input').style.display = ''; startInput(); }
  } catch (e) { $('conn').textContent = e.message; }
}

// ---- settings (read-only view) ----
async function loadSettings() {
  try {
    const s = await api('GET', '/api/settings');
    // Values are text context only, so escapeHtml suffices (issue #55) — a
    // favorite named "<b>roms" must render literally, like filenames do.
    const row = (k, v) => k + ': <b>' + escapeHtml(v) + '</b>';
    $('set-view').innerHTML = [
      row('Password', s.password_hash ? 'fixed hash' : (s.password ? 'fixed code' : 'random (new each launch)')),
      row('Show code on screen', s.display_password ? 'yes' : 'no'),
      row('Permission', s.permission),
      row('Default folder', s.default_folder || '(Home)'),
      row('Favorites', (s.favorites && s.favorites.length)
        ? s.favorites.map(f => f.name).join(', ') : '(none)'),
      row('Root', s.root || '(default)'),
    ].join('<br>');
  } catch (e) { $('set-view').textContent = e.message; }
  loadVersion();
}

// ---- software update ----
let updCheckResult = null;

async function loadVersion() {
  try {
    const info = await api('GET', '/api/info');
    $('upd-version').innerHTML = 'Installed version: <b>' + escapeHtml(info.version || '0.0.0') + '</b>';
    // Same fetch fills the help card's real config location (issue #60) —
    // device builds keep it next to the binary, desktop builds don't.
    if (info.config_path) $('cfg-path').textContent = info.config_path;
  } catch (e) {
    $('upd-version').textContent = 'Version unknown';
  }
}

async function checkUpdate() {
  const btn = $('upd-check');
  const status = $('upd-status');
  btn.disabled = true;
  $('upd-apply').style.display = 'none';
  status.style.color = '';
  status.textContent = 'Checking…';
  try {
    updCheckResult = await api('GET', '/api/update/check');
    if (updCheckResult.up_to_date) {
      status.textContent = 'Already up to date (v' + updCheckResult.current + ').';
    } else if (!updCheckResult.asset_url) {
      status.textContent = 'Update available (v' + updCheckResult.latest + ') but no binary for this platform.';
    } else if (updCheckResult.dev_build) {
      // Dev builds (unstamped 0.0.0) are pre-release test builds, deliberately
      // replaceable by the published release in one click — that's the point
      // of the 0.0.0 builds (issue #46). Same offer as a regular update, with
      // an informational dev-build label only.
      status.innerHTML = 'Update available: dev build <b>v' + escapeHtml(updCheckResult.current)
        + '</b> → <b>v' + escapeHtml(updCheckResult.latest) + '</b>';
      $('upd-apply').style.display = '';
    } else {
      status.innerHTML = 'Update available: <b>v' + escapeHtml(updCheckResult.current)
        + '</b> → <b>v' + escapeHtml(updCheckResult.latest) + '</b>';
      $('upd-apply').style.display = '';
    }
  } catch (e) {
    status.style.color = 'var(--danger)';
    status.textContent = 'Check failed: ' + e.message;
  } finally {
    btn.disabled = false;
  }
}

async function applyUpdate() {
  if (!updCheckResult || !updCheckResult.asset_url) return;
  // No confirm gate for dev builds: they are pre-release test builds meant to
  // be one-click replaced by the published release, exactly like release
  // builds (issue #46).
  const btn = $('upd-apply');
  const status = $('upd-status');
  btn.disabled = true;
  $('upd-check').disabled = true;
  status.style.color = '';
  status.textContent = 'Downloading and applying update… (this may take a moment)';
  try {
    // No body: the server re-resolves the right asset for its own platform.
    await api('POST', '/api/update/apply');
    $('upd-apply').style.display = 'none';
    status.style.color = 'var(--amber)';
    status.textContent = '✓ Update applied to v' + updCheckResult.latest + '. Restart the app to use the new version.';
    loadVersion();
  } catch (e) {
    status.style.color = 'var(--danger)';
    status.textContent = 'Update failed: ' + e.message;
    btn.disabled = false;
  } finally {
    $('upd-check').disabled = false;
  }
}

// ---- view switching ----
function showView(v) {
  document.querySelectorAll('nav a').forEach(a => a.classList.toggle('active', a.dataset.view === v));
  $('view-files').style.display = v === 'files' ? 'flex' : 'none';
  $('view-status').style.display = v === 'status' ? 'block' : 'none';
  $('view-settings').style.display = v === 'settings' ? 'block' : 'none';
  if (v === 'status') { loadConn(); }
  if (v === 'settings') loadSettings();
}

// ---- wire up ----
$('t-up').onclick = () => $('filepick').click();
$('t-updir').onclick = () => $('dirpick').click();
$('t-mkdir').onclick = doMkdir;
$('t-newfile').onclick = doNewFile;
$('t-open').onclick = doOpen;
$('t-dl').onclick = doDownload;
$('t-rename').onclick = doRename;
$('t-cut').onclick = doCut; $('t-copy').onclick = doCopy; $('t-paste').onclick = doPaste;
$('t-del').onclick = doDelete; $('t-refresh').onclick = doRefresh;
$('sb-clear').onclick = () => { if (exitSearch()) { rebuild(); syncToolbar(); } $('t-filter').focus(); };
$('sb-more').onclick = continueSearch;
$('v-list').onclick = () => setView('list'); $('v-grid').onclick = () => setView('grid');
$('t-select').onclick = () => setSelectMode(!selectMode);
$('t-hidden').onclick = () => { showHidden = !showHidden; $('t-hidden').classList.toggle('on', showHidden);
  selection = new Set(); lastIndex = null; rebuild(); syncToolbar(); };
$('t-filter').oninput = () => applyFilter($('t-filter').value);
$('t-filter').onkeydown = (e) => {
  // Esc: clear the filter (and any search results) and hand focus back to the
  // list. Stop it here so the document-level Esc (close preview/menu) doesn't
  // also fire, and preventDefault so type=search's native clear doesn't race
  // our state.
  if (e.key === 'Escape') {
    e.preventDefault(); e.stopPropagation();
    if (filterText || inSearch()) { setFilter(''); applyFilter(''); }
    $('t-filter').blur();
  } else if (e.key === 'Enter') {
    // Enter widens the same text from "filter this folder" to "search every
    // folder below it" — the one thing the client-side filter cannot do.
    e.preventDefault();
    runSearch($('t-filter').value);
    $('t-filter').blur();
  }
};
$('ed-save').onclick = saveEditor;
$('ed-close').onclick = requestCloseEditor;
$('upd-check').onclick = checkUpdate;
$('upd-apply').onclick = applyUpdate;
$('filepick').onchange = (e) => { uploadFiles(e.target.files); e.target.value = ''; };
$('dirpick').onchange = (e) => { uploadPickedFolder(e.target.files); e.target.value = ''; };
$('i-clear').onclick = () => { held.clear(); axes.clear(); $('ilog').textContent=''; renderHeld(); renderAxes(); };
function closeSidebar() {
  document.querySelector('aside').classList.remove('open');
  $('side-overlay').classList.remove('show');
}
$('menu-toggle').onclick = () => {
  const open = document.querySelector('aside').classList.toggle('open');
  $('side-overlay').classList.toggle('show', open);
};
$('side-overlay').onclick = closeSidebar;
document.querySelectorAll('nav a').forEach(a => a.onclick = () => { showView(a.dataset.view); closeSidebar(); });
$('mclose').onclick = closePreview;
$('modal').onclick = (e) => { if (e.target === $('modal')) closePreview(); };
document.addEventListener('keydown', (e) => {
  // Editor captures its own keys (don't let Esc bubble to the file list).
  if ($('editor').classList.contains('show')) {
    if (e.key === 'Escape') { e.preventDefault(); requestCloseEditor(); }
    else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') { e.preventDefault(); saveEditor(); }
    return;
  }
  if (e.key === 'Escape') {
    const overlay = $('modal').classList.contains('show') || $('menu').style.display === 'block';
    closePreview(); closeMenu();
    // With nothing overlaying the list, Esc leaves the search results — the
    // same thing it does when the filter box has focus.
    if (!overlay && inSearch()) { setFilter(''); applyFilter(''); }
    return;
  }
  if ($('modal').classList.contains('show')) {
    if (e.key === 'ArrowRight') { e.preventDefault(); navigatePreview(1); }
    else if (e.key === 'ArrowLeft') { e.preventDefault(); navigatePreview(-1); }
    return;
  }
  // "/" focuses the filter — but never while typing somewhere else (an
  // input/textarea or contenteditable), and never with modifiers held
  // (browser shortcuts like Cmd+/ stay intact). The editor and preview
  // branches above already returned. focus() is a no-op when the files
  // view is hidden, so Status/Settings are unaffected.
  if (e.key === '/' && !e.ctrlKey && !e.metaKey && !e.altKey) {
    const t = e.target;
    const typing = t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable);
    if (!typing) { e.preventDefault(); $('t-filter').focus(); $('t-filter').select(); }
  }
});

// Warn before tab close / reload / back navigation while the editor holds
// unsaved edits. Inert when clean: browsers only show the leave-page prompt
// when the handler calls preventDefault / sets returnValue.
window.addEventListener('beforeunload', (e) => {
  if (!editorDirty()) return;
  e.preventDefault();
  e.returnValue = '';                 // legacy Chrome requires a set returnValue
});

// Browser Back/Forward: re-navigate to the folder the popped history entry
// points at (state.path, falling back to the URL's ?path=). push=false — we're
// moving through existing history, not adding to it. Close any open preview so
// Back reads as "go up/out" rather than leaving a modal stranded over a folder
// that changed underneath it.
window.addEventListener('popstate', (e) => {
  // Guard Back out of the editor when it holds unsaved edits — the same
  // protection as the close button / Esc / tab-close (beforeunload), now for
  // the browser Back button. popstate can't be prevented (the history move
  // already happened), so a declined discard is undone by re-pushing the
  // folder the editor sits over, keeping the user on the editor.
  if ($('editor').classList.contains('show')) {
    if (editorDirty() && !confirmDiscard()) { history.pushState({ path: cwd }, '', urlForPath(cwd)); return; }
    closeEditor();
  }
  const path = e.state && typeof e.state.path === 'string' ? e.state.path : pathFromUrl();
  closePreview();
  showView('files');
  go(path, false);
});

// click-away to clear selection / close menu
document.addEventListener('click', (e) => { closeMenu();
  if (e.target.closest('#view-files .listwrap') && !e.target.closest('tr') && !e.target.closest('.gcard')) {
    selection = new Set(); syncSelection(); syncToolbar(); } });
$('listwrap').oncontextmenu = (e) => { if (!e.target.closest('tr') && !e.target.closest('.gcard')) {
  e.preventDefault(); selection = new Set(); syncSelection(); syncToolbar(); openMenu(e.clientX, e.clientY); } };

// drag & drop upload
const main = document.querySelector('main'); let dragDepth = 0;
['dragenter','dragover'].forEach(ev => main.addEventListener(ev, (e) => {
  if (![...e.dataTransfer.types].includes('Files')) return; e.preventDefault();
  if (ev === 'dragenter') dragDepth++; $('drop').classList.add('show');
}));
main.addEventListener('dragleave', () => { if (--dragDepth <= 0) { dragDepth = 0; $('drop').classList.remove('show'); } });
main.addEventListener('drop', async (e) => { e.preventDefault(); dragDepth = 0; $('drop').classList.remove('show');
  if (!canWrite()) { toast('Read-only — uploads are disabled', true); return; }
  if (atVirtualRoot()) { toast('The root lists the shared mounts — open one to upload', true); return; }
  if (inSearch()) { toast('Search results span folders — clear the search to upload', true); return; }
  // Folder-aware path: snapshot the entries *synchronously* — the
  // DataTransferItemList is gone after the first await — then traverse
  // directories recursively. A dropped folder used to surface as a useless
  // directory File that failed with a generic error (issue #30).
  const entries = [...(e.dataTransfer.items || [])]
    .map(it => it.webkitGetAsEntry ? it.webkitGetAsEntry() : null)
    .filter(Boolean);
  if (entries.length) {
    try {
      const { files, emptyDirs } = await collectDropped(entries);
      uploadItems(files, emptyDirs);
    } catch (err) { toast('Could not read the dropped items: ' + err.message, true); }
    return;
  }
  // Fallback (no entry API): flat files only.
  if (!e.dataTransfer.files.length) return;
  uploadFiles(e.dataTransfer.files); });

// Render the sidebar favorites (named folder shortcuts). Empty → hidden.
function renderFavorites(favs) {
  const c = $('favs'); c.innerHTML = '';
  const list = Array.isArray(favs) ? favs.filter(f => f && typeof f.name === 'string') : [];
  c.classList.toggle('show', list.length > 0);
  if (!list.length) return;
  const head = document.createElement('div'); head.className = 'fav-head'; head.textContent = 'Favorites';
  c.appendChild(head);
  list.forEach(f => {
    const path = typeof f.path === 'string' ? f.path : '';
    const a = document.createElement('a');
    a.textContent = f.name; a.title = path || '(Home)';
    a.onclick = () => { showView('files'); go(path); closeSidebar(); };
    c.appendChild(a);
  });
}

// Read settings (permission + default folder + favorites) before the first listing.
(async () => {
  try {
    const s = await api('GET', '/api/settings');
    perm = s.permission || 'read_write'; defaultFolder = s.default_folder || '';
    multiRoot = !!s.multi_root;
    renderFavorites(s.favorites);
  }
  catch (e) { /* fall back to defaults */ }
  // A `?path=` in the URL (a shared deep link, or where Back/restart-login
  // dropped us) wins over the configured default folder; otherwise open the
  // default. Normalize the bar to match the folder we actually open, and seed
  // the first history entry's state so an immediate Back has a path to read.
  const start = new URLSearchParams(location.search).has('path') ? pathFromUrl() : defaultFolder;
  await go(start, false);
  history.replaceState({ path: start }, '', urlForPath(start));
  refreshDisk();                    // populate the sidebar gauge on first paint
})();
