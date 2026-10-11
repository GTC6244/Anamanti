// Client-side helpers shared by every admin page. Served from /shell.js and loaded
// (blocking) before each page body, so a body's inline load() — which runs as it is
// parsed — can rely on esc/fmtTime/getJSON being defined.
function esc(s){ return (s==null?'':String(s)).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
function fmtTime(ts){ if(!ts) return ''; try { return new Date(ts*1000).toLocaleString(); } catch(e){ return String(ts); } }
async function getJSON(url){ const r = await fetch(url); return r.json(); }

// Shell behavior: highlight the active nav link for the current path, and wire the
// app-bar menu button to the navigation drawer on narrow screens. Runs once the DOM
// is ready (the shell markup precedes this script, but the nav lives above the body).
(function(){
  function initShell(){
    var path = location.pathname || '/';
    document.querySelectorAll('.nav a.navlink').forEach(function(a){
      if (a.getAttribute('href') === path) a.classList.add('active');
    });
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
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initShell);
  } else {
    initShell();
  }
})();
