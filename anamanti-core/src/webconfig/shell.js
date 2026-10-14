// Client-side helpers shared by every admin page. Served from /shell.js and loaded
// (blocking) before each page body, so a body's inline script — which runs as it is
// parsed — can rely on $/esc/escAttr/fmtTime/getJSON/apiJSON/postJSON being defined.
// The component kit (toasts, save bar, choice cards, locality chips, drawers, tabs…)
// is /ui.js, loaded right after this file.

function $(id){ return document.getElementById(id); }
// Escape text for HTML element content.
function esc(s){ return (s==null?'':String(s)).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
// Escape text for a quoted HTML attribute value.
function escAttr(s){ return (s==null?'':String(s)).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
function fmtTime(ts){ if(!ts) return ''; try { return new Date(ts*1000).toLocaleString(); } catch(e){ return String(ts); } }
async function getJSON(url){ const r = await fetch(url); return r.json(); }
// fetch + parse JSON with arbitrary fetch options.
async function apiJSON(url, opts){ const r = await fetch(url, opts); return r.json(); }
// POST a JSON body (default `{}`) and parse the JSON reply. Every admin POST goes
// through this: the server rejects a POST without `Content-Type: application/json`
// (a cross-site form post cannot set it).
async function postJSON(url, body){
  return apiJSON(url, {method:'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify(body === undefined ? {} : body)});
}

// Shell behavior: active-nav highlight + page-title glyph, the collapsible rail, the
// mobile drawer, the theme toggle, page-head slots, and the app-bar status pills.
(function(){
  function initShell(){
    var path = location.pathname || '/';
    if (path === '/index.html') path = '/';
    document.querySelectorAll('.nav a.navlink').forEach(function(a){
      if (a.getAttribute('href') === path) { a.classList.add('active'); a.setAttribute('aria-current', 'page'); }
    });
    // Mirror the active nav glyph next to the page title.
    var activeIcon = document.querySelector('.nav a.navlink.active svg');
    var h1 = document.querySelector('.page-head h1');
    if (activeIcon && h1 && !h1.querySelector('svg')) h1.insertBefore(activeIcon.cloneNode(true), h1.firstChild);

    // Page-head slots: a body element marked data-head="sub" becomes the page
    // description under the title; data-head="actions" moves into the right side.
    var headText = document.querySelector('.page-head-text');
    var actions = document.getElementById('pageActions');
    document.querySelectorAll('[data-head="sub"]').forEach(function(el){ if (headText) headText.appendChild(el); });
    document.querySelectorAll('[data-head="actions"]').forEach(function(el){ if (actions) actions.appendChild(el); });

    // Mobile drawer.
    var menu = document.getElementById('menuBtn');
    var drawer = document.getElementById('drawer');
    var scrim = document.getElementById('scrim');
    function close(){
      if (drawer) drawer.classList.remove('open');
      if (scrim) scrim.classList.remove('open');
      if (menu) menu.setAttribute('aria-expanded', 'false');
    }
    if (menu && drawer) {
      menu.addEventListener('click', function(){
        var open = drawer.classList.toggle('open');
        if (scrim) scrim.classList.toggle('open', open);
        menu.setAttribute('aria-expanded', open ? 'true' : 'false');
      });
    }
    if (scrim) scrim.addEventListener('click', close);

    // Collapsible rail (persisted).
    var rail = document.getElementById('railBtn');
    if (rail) rail.addEventListener('click', function(){
      var on = document.body.classList.toggle('rail');
      try { localStorage.setItem('anamanti.rail', on ? '1' : '0'); } catch (e) {}
    });

    // Theme toggle: flips between light and dark (persisted); the initial value
    // follows the OS until the user picks one.
    var theme = document.getElementById('themeBtn');
    if (theme) theme.addEventListener('click', function(){
      var root = document.documentElement;
      var cur = root.getAttribute('data-theme');
      if (!cur) cur = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      var next = cur === 'dark' ? 'light' : 'dark';
      root.setAttribute('data-theme', next);
      try { localStorage.setItem('anamanti.theme', next); } catch (e) {}
    });

    loadPills();
  }

  // App-bar pills: instance name, connected displays, and the locality summary of
  // the live pipeline. Uses the probe-free overview so every page load stays cheap.
  async function loadPills(){
    var box = document.getElementById('appPills');
    if (!box) return;
    try {
      var o = await getJSON('/overview.json?probe=0');
      if (!o || !o.ok) return;
      var inst = document.getElementById('appInstance');
      if (inst && o.service_name) inst.textContent = o.service_name + (o.instance_id && o.instance_id !== o.service_name ? ' · ' + o.instance_id : '');
      var counts = {local:0, lan:0, cloud:0};
      (o.stages||[]).forEach(function(s){ if (counts[s.tier] != null) counts[s.tier]++; });
      var n = (o.devices||[]).length;
      var html = '<a class="chip lg ' + (n ? 'ok' : 'neutral') + '" href="/devices#displays" title="Connected displays" style="text-decoration:none">'
        + '<span class="dot ' + (n ? 'ok' : 'off') + '"></span>' + n + ' display' + (n === 1 ? '' : 's') + '</a>';
      html += '<a class="chip lg neutral" href="/" title="Where the live pipeline runs" style="text-decoration:none;gap:.5rem">'
        + '<span style="color:var(--loc-local-fg)">● ' + counts.local + ' local</span>'
        + (counts.lan ? '<span style="color:var(--loc-lan-fg)">● ' + counts.lan + ' LAN</span>' : '')
        + '<span style="color:var(--loc-cloud-fg)">● ' + counts.cloud + ' cloud</span></a>';
      box.innerHTML = html;
    } catch (e) { /* the pills are decoration — never break a page over them */ }
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', initShell);
  else initShell();
})();
