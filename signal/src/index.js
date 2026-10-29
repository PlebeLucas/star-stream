// STAR // STREAM — servidor de encontro (Cloudflare Worker, plano gratuito)
//
// O que ele faz:
//   1. Login com Discord (guarda o client secret aqui, longe do app)
//   2. Só libera quem é membro do servidor do Discord de vocês (GUILD_ID)
//   3. Mantém a "sala": quem está online e quem está ao vivo
//   4. Repassa os sinais WebRTC (offer/answer/candidates) entre os PCs
//   5. Avisa no canal do Discord (webhook) quando alguém entra ao vivo
//
// O vídeo e o áudio NUNCA passam por aqui: vão direto de PC para PC.

const DISCORD_API = 'https://discord.com/api/v10';
const enc = new TextEncoder();

// ---------- utilidades de token assinado (HMAC-SHA256) ----------

function b64url(bytes) {
  let s = '';
  const arr = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  for (const b of arr) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function b64urlDecode(str) {
  const pad = str.length % 4 === 0 ? '' : '='.repeat(4 - (str.length % 4));
  const bin = atob(str.replace(/-/g, '+').replace(/_/g, '/') + pad);
  return Uint8Array.from(bin, (c) => c.charCodeAt(0));
}

async function hmacKey(secret) {
  return crypto.subtle.importKey('raw', enc.encode(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
}

async function sign(payload, secret) {
  const body = b64url(enc.encode(JSON.stringify(payload)));
  const sig = await crypto.subtle.sign('HMAC', await hmacKey(secret), enc.encode(body));
  return body + '.' + b64url(sig);
}

async function verify(token, secret) {
  if (typeof token !== 'string') return null;
  const [body, sig] = token.split('.');
  if (!body || !sig) return null;
  let ok = false;
  try {
    ok = await crypto.subtle.verify('HMAC', await hmacKey(secret), b64urlDecode(sig), enc.encode(body));
  } catch {
    return null;
  }
  if (!ok) return null;
  const payload = JSON.parse(new TextDecoder().decode(b64urlDecode(body)));
  if (payload.exp && payload.exp < Date.now() / 1000) return null;
  return payload;
}

// ---------- respostas ----------

function html(msg, status = 200) {
  const page = `<!doctype html><meta charset="utf-8"><title>STAR // STREAM</title>
<body style="margin:0;background:#07080A;color:#F2F4F5;font-family:system-ui,sans-serif;display:grid;place-items:center;min-height:100vh">
<div style="border:1px solid #FF2A3D;padding:32px 40px;max-width:480px"><div style="letter-spacing:.2em;font-weight:700">STAR <span style="color:#FF2A3D">//</span> STREAM</div>
<p style="color:#9AA1AB;line-height:1.5">${msg}</p></div></body>`;
  return new Response(page, { status, headers: { 'content-type': 'text/html; charset=utf-8' } });
}

function json(data, status = 200) {
  return new Response(JSON.stringify(data), { status, headers: { 'content-type': 'application/json' } });
}

async function iceServers(env) {
  const servers = [{ urls: ['stun:stun.cloudflare.com:3478', 'stun:stun.l.google.com:19302'] }];
  // Relay reserva da Cloudflare (credenciais temporárias geradas na hora)
  if (env.TURN_KEY_ID && env.TURN_KEY_API_TOKEN) {
    try {
      const r = await fetch(`https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`, {
        method: 'POST',
        headers: { authorization: `Bearer ${env.TURN_KEY_API_TOKEN}`, 'content-type': 'application/json' },
        body: JSON.stringify({ ttl: 86400 }),
      });
      if (r.ok) {
        const { iceServers: list } = await r.json();
        for (const srv of [].concat(list || [])) {
          const urls = [].concat(srv.urls).filter((u) => !/:53(\?|$)/.test(u)); // porta 53 costuma travar
          if (srv.username && urls.length) servers.push({ urls, username: srv.username, credential: srv.credential });
        }
      }
    } catch {
      /* segue só com STUN */
    }
  } else if (env.TURN_URLS) {
    // Qualquer outro serviço de TURN com usuário/senha fixos
    servers.push({
      urls: env.TURN_URLS.split(',').map((s) => s.trim()),
      username: env.TURN_USERNAME || '',
      credential: env.TURN_CREDENTIAL || '',
    });
  }
  return servers;
}

// ---------- rotas ----------

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const redirectUri = `${url.origin}/callback`;

    // 1) O app abre: /login?port=PORTA_LOCAL&nonce=ALEATORIO
    if (url.pathname === '/login') {
      const port = Number(url.searchParams.get('port'));
      const nonce = url.searchParams.get('nonce') || '';
      if (!Number.isInteger(port) || port < 1024 || port > 65535 || !/^[A-Za-z0-9_-]{8,64}$/.test(nonce)) {
        return html('Pedido de login inválido. Abra o login pelo app.', 400);
      }
      const state = await sign({ port, nonce, exp: Math.floor(Date.now() / 1000) + 600 }, env.SESSION_SECRET);
      const auth = new URL('https://discord.com/oauth2/authorize');
      auth.searchParams.set('client_id', env.DISCORD_CLIENT_ID);
      auth.searchParams.set('response_type', 'code');
      auth.searchParams.set('redirect_uri', redirectUri);
      auth.searchParams.set('scope', 'identify guilds');
      auth.searchParams.set('state', state);
      auth.searchParams.set('prompt', 'none');
      return Response.redirect(auth.toString(), 302);
    }

    // 2) O Discord devolve aqui
    if (url.pathname === '/callback') {
      const st = await verify(url.searchParams.get('state'), env.SESSION_SECRET);
      const code = url.searchParams.get('code');
      if (!st || !code) return html('Login cancelado ou expirado. Tente de novo pelo app.', 400);

      const tokRes = await fetch(`${DISCORD_API}/oauth2/token`, {
        method: 'POST',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: new URLSearchParams({
          client_id: env.DISCORD_CLIENT_ID,
          client_secret: env.DISCORD_CLIENT_SECRET,
          grant_type: 'authorization_code',
          code,
          redirect_uri: redirectUri,
        }),
      });
      if (!tokRes.ok) return html('O Discord recusou o login. Tente de novo.', 502);
      const { access_token } = await tokRes.json();
      const headers = { authorization: `Bearer ${access_token}` };

      const [meRes, guildsRes] = await Promise.all([
        fetch(`${DISCORD_API}/users/@me`, { headers }),
        fetch(`${DISCORD_API}/users/@me/guilds`, { headers }),
      ]);
      if (!meRes.ok || !guildsRes.ok) return html('Não consegui ler sua conta do Discord.', 502);
      const me = await meRes.json();
      const guilds = await guildsRes.json();

      if (!guilds.some((g) => g.id === env.GUILD_ID)) {
        return html('Acesso negado: você não faz parte do servidor da STAR Corp.', 403);
      }

      const token = await sign(
        {
          id: me.id,
          name: me.global_name || me.username,
          avatar: me.avatar ? `https://cdn.discordapp.com/avatars/${me.id}/${me.avatar}.png?size=64` : null,
          exp: Math.floor(Date.now() / 1000) + 60 * 60 * 24 * 30,
        },
        env.SESSION_SECRET,
      );
      const back = new URL(`http://127.0.0.1:${st.port}/callback`);
      back.searchParams.set('token', token);
      back.searchParams.set('nonce', st.nonce);
      return Response.redirect(back.toString(), 302);
    }

    // 3) Sala em tempo real (WebSocket)
    if (url.pathname === '/ws') {
      if (request.headers.get('Upgrade') !== 'websocket') return json({ error: 'websocket esperado' }, 426);
      const user = await verify(url.searchParams.get('token'), env.SESSION_SECRET);
      if (!user) return json({ error: 'sessão inválida' }, 401);
      const id = env.ROOM.idFromName('main');
      const fwd = new URL(request.url);
      fwd.searchParams.set('u', btoa(unescape(encodeURIComponent(JSON.stringify(user)))));
      return env.ROOM.get(id).fetch(new Request(fwd.toString(), request));
    }

    // 4) Link curto de download: sempre aponta pro instalador da versão mais nova
    if (url.pathname === '/baixar') {
      if (!env.GITHUB_REPO) return html('O download ainda não foi configurado.', 404);
      const latest = `https://github.com/${env.GITHUB_REPO}/releases/latest`;
      try {
        const r = await fetch(`${latest}/download/latest.json`, { cf: { cacheTtl: 300 } });
        if (r.ok) {
          const info = await r.json();
          const exe = info.platforms?.['windows-x86_64']?.url || info.platforms?.['windows-x86_64-nsis']?.url;
          if (exe) return Response.redirect(exe, 302);
        }
      } catch {
        /* cai na página da versão */
      }
      return Response.redirect(latest, 302);
    }

    if (url.pathname === '/') {
      return html(
        'Servidor de encontro no ar.' +
          (env.GITHUB_REPO ? ' <a style="color:#FF2A3D" href="/baixar">Baixar o STAR Stream</a>' : ''),
      );
    }
    return json({ error: 'não encontrado' }, 404);
  },
};

// ---------- a sala ----------

export class Room {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    this.peers = new Map(); // peerId -> { ws, user, live }
  }

  async fetch(request) {
    const url = new URL(request.url);
    const user = JSON.parse(decodeURIComponent(escape(atob(url.searchParams.get('u')))));
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);
    server.accept();

    const peerId = crypto.randomUUID();
    const peer = { ws: server, user: { id: user.id, name: user.name, avatar: user.avatar }, live: null };
    this.peers.set(peerId, peer);

    this.send(server, { t: 'hello', you: { peerId, user: peer.user }, ice: await iceServers(this.env) });
    this.broadcastPresence();

    server.addEventListener('message', (ev) => this.onMessage(peerId, ev.data));
    const bye = () => {
      if (!this.peers.has(peerId)) return;
      this.peers.delete(peerId);
      for (const [, p] of this.peers) this.send(p.ws, { t: 'gone', peerId });
      this.broadcastPresence();
    };
    server.addEventListener('close', bye);
    server.addEventListener('error', bye);

    return new Response(null, { status: 101, webSocket: client });
  }

  onMessage(peerId, raw) {
    const me = this.peers.get(peerId);
    if (!me || typeof raw !== 'string' || raw.length > 64 * 1024) return;
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch {
      return;
    }

    if (msg.t === 'live') {
      const wasLive = !!me.live;
      me.live = msg.on ? { since: Date.now(), info: String(msg.info || '').slice(0, 120) } : null;
      this.broadcastPresence();
      if (msg.on && !wasLive) this.announce(me.user.name, me.live.info);
      return;
    }

    if (msg.t === 'signal' && typeof msg.to === 'string') {
      const target = this.peers.get(msg.to);
      if (target) this.send(target.ws, { t: 'signal', from: peerId, fromUser: me.user, data: msg.data });
      return;
    }

    if (msg.t === 'ping') this.send(me.ws, { t: 'pong' });
  }

  broadcastPresence() {
    const list = [...this.peers].map(([peerId, p]) => ({ peerId, user: p.user, live: p.live }));
    for (const [, p] of this.peers) this.send(p.ws, { t: 'presence', peers: list });
  }

  send(ws, obj) {
    try {
      ws.send(JSON.stringify(obj));
    } catch {
      /* conexão já fechou */
    }
  }

  announce(name, info) {
    if (!this.env.DISCORD_WEBHOOK_URL) return;
    const content = `🔴 **${name}** entrou ao vivo no STAR // STREAM${info ? ` — ${info}` : ''}. Abra o app pra assistir.`;
    fetch(this.env.DISCORD_WEBHOOK_URL, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ username: 'STAR // STREAM', content, allowed_mentions: { parse: [] } }),
    }).catch(() => {});
  }
}
