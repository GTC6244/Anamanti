// Anamanti website Worker.
//
// Serves the static site in ../website (via the ASSETS binding) and adds two tiny
// form endpoints backed by the D1 database "anamanti-site":
//   POST /api/waitlist  { email, name?, interest?, source?, company? }
//   POST /api/contact   { email, message, name?, topic?, company? }
// `company` is a honeypot: real visitors never see the field, so a non-empty value
// is treated as a bot and silently accepted (nothing is stored).
//
// The apex host (anamanti.com) permanently redirects to www.anamanti.com.
// Read submissions with:
//   npx wrangler d1 execute anamanti-site --remote --command "SELECT * FROM waitlist ORDER BY id DESC LIMIT 50"
//   npx wrangler d1 execute anamanti-site --remote --command "SELECT * FROM contact WHERE handled = 0"

const CANONICAL_HOST = 'www.anamanti.com';
const MAX_PER_HOUR = 5; // per client IP, per table
const EMAIL_RE = /^[^\s@<>()",;:]{1,64}@[^\s@<>()",;:]{1,190}\.[A-Za-z]{2,24}$/;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.hostname === 'anamanti.com') {
      url.hostname = CANONICAL_HOST;
      return Response.redirect(url.toString(), 301);
    }

    if (url.pathname === '/api/waitlist' || url.pathname === '/api/contact') {
      if (request.method !== 'POST') return json({ ok: false, error: 'method' }, 405);
      try {
        return url.pathname === '/api/waitlist'
          ? await waitlist(request, env)
          : await contact(request, env);
      } catch (err) {
        console.error('form error', err && err.message);
        return json({ ok: false, error: 'server' }, 500);
      }
    }
    if (url.pathname.startsWith('/api/')) return json({ ok: false, error: 'not_found' }, 404);

    return env.ASSETS.fetch(request);
  },
};

async function waitlist(request, env) {
  const body = await readBody(request);
  if (!body) return json({ ok: false, error: 'bad_request' }, 400);
  if (clean(body.company, 200)) return json({ ok: true }); // honeypot

  const email = clean(body.email, 254).toLowerCase();
  if (!EMAIL_RE.test(email)) return json({ ok: false, error: 'email' }, 400);
  const name = clean(body.name, 120);
  const interest = clean(body.interest, 500);
  const source = clean(body.source, 80);

  const ipHash = await hashIp(request, env);
  if (await overLimit(env, 'waitlist', ipHash)) return json({ ok: false, error: 'rate' }, 429);

  // Re-joining with the same address is fine: keep the first row, report success.
  await env.DB.prepare(
    'INSERT INTO waitlist (email, name, interest, source, ip_hash) VALUES (?, ?, ?, ?, ?) ON CONFLICT(email) DO NOTHING'
  ).bind(email, name || null, interest || null, source || null, ipHash).run();
  return json({ ok: true });
}

async function contact(request, env) {
  const body = await readBody(request);
  if (!body) return json({ ok: false, error: 'bad_request' }, 400);
  if (clean(body.company, 200)) return json({ ok: true }); // honeypot

  const email = clean(body.email, 254).toLowerCase();
  if (!EMAIL_RE.test(email)) return json({ ok: false, error: 'email' }, 400);
  const message = clean(body.message, 5000);
  if (message.length < 2) return json({ ok: false, error: 'message' }, 400);
  const name = clean(body.name, 120);
  const topic = clean(body.topic, 80);

  const ipHash = await hashIp(request, env);
  if (await overLimit(env, 'contact', ipHash)) return json({ ok: false, error: 'rate' }, 429);

  await env.DB.prepare(
    'INSERT INTO contact (name, email, topic, message, ip_hash) VALUES (?, ?, ?, ?, ?)'
  ).bind(name || null, email, topic || null, message, ipHash).run();
  return json({ ok: true });
}

async function readBody(request) {
  const type = request.headers.get('content-type') || '';
  if (Number(request.headers.get('content-length') || 0) > 20000) return null;
  try {
    if (type.includes('application/json')) return await request.json();
    if (type.includes('form')) return Object.fromEntries(await request.formData());
  } catch (_) { /* fall through */ }
  return null;
}

function clean(v, max) {
  return typeof v === 'string' ? v.replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F]/g, '').trim().slice(0, max) : '';
}

async function hashIp(request, env) {
  const ip = request.headers.get('cf-connecting-ip') || 'unknown';
  const data = new TextEncoder().encode((env.IP_SALT || 'anamanti-site') + ':' + ip);
  const digest = await crypto.subtle.digest('SHA-256', data);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

async function overLimit(env, table, ipHash) {
  const row = await env.DB.prepare(
    `SELECT COUNT(*) AS n FROM ${table} WHERE ip_hash = ? AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-1 hour')`
  ).bind(ipHash).first();
  return row && row.n >= MAX_PER_HOUR;
}

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' },
  });
}
