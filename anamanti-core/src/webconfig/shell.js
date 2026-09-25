// Client-side helpers shared by every admin page. Emitted before each page body
// so a body's inline load() (which runs as it is parsed) can rely on them.
function esc(s){ return (s==null?'':String(s)).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
function fmtTime(ts){ if(!ts) return ''; try { return new Date(ts*1000).toLocaleString(); } catch(e){ return String(ts); } }
async function getJSON(url){ const r = await fetch(url); return r.json(); }
