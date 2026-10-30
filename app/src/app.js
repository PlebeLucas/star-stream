// STAR // STREAM: interface.
// Fala com o servidor de encontro (login, sala, sinais) e com o núcleo em
// Rust (captura, encoder, envio). Quem assiste usa o WebRTC da própria janela.

const T = window.__TAURI__;
const invoke = T ? T.core.invoke : async () => { throw new Error('rodando fora do app'); };
const listen = T ? T.event.listen : async () => {};

const $ = (id) => document.getElementById(id);

// Tudo de errado na interface vai pro arquivo de diagnóstico
const jsLog = (level, message) => invoke('js_log', { level, message: String(message) }).catch(() => {});
window.addEventListener('error', (e) => jsLog('error', `${e.message} @ ${e.filename}:${e.lineno}`));
window.addEventListener('unhandledrejection', (e) => jsLog('error', 'promise: ' + (e.reason && e.reason.stack || e.reason)));
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch {} },
};

const PRESETS = {
  economica: '720p · 30fps · 2,5 Mbps',
  leve: '1080p · 30fps · 6 Mbps',
  equilibrada: '1080p · 60fps · 10 Mbps',
  maxima: '1440p · 60fps · 15 Mbps',
};
const AUDIO_LABEL = { pc: 'PC inteiro', app: 'Só um app', off: 'Sem som' };

const S = {
  signalUrl: '',
  token: null,
  me: null, // { peerId, user }
  ice: [],
  ws: null,
  wsRetry: 0,
  peers: [],
  sources: [],
  cfg: {
    source: null, // { kind, handle, name, pid }
    preset: store.get('preset') || 'equilibrada',
    audio: store.get('audio') || 'pc',
    appPid: null,
    volume: Number(store.get('volume') || 100),
    notify: store.get('notify') !== '0',
  },
  live: false,
  liveSince: 0,
  viewers: new Map(), // peerId -> { user, state }
  watch: null, // { peerId, user, pc, pendingIce, remoteSet, statsTimer, lastBytes, lastTs }
};

// ---------------- utilidades ----------------

function show(screen) {
  for (const s of ['login', 'home', 'live', 'watch']) $('screen-' + s).hidden = s !== screen;
}

let toastTimer;
function toast(msg) {
  jsLog('warn', 'aviso na tela: ' + msg);
  const t = $('toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 6000);
}

function avatarImg(user, cls) {
  const img = document.createElement('img');
  img.alt = '';
  if (cls) img.className = cls;
  if (user && user.avatar) img.src = user.avatar;
  return img;
}

function el(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids) e.append(k);
  return e;
}

function send(obj) {
  if (S.ws && S.ws.readyState === WebSocket.OPEN) S.ws.send(JSON.stringify(obj));
}

// role: 'v' = vindo de quem assiste, 'h' = vindo de quem transmite.
// Necessário porque dois amigos podem se assistir ao mesmo tempo.
function sendSignal(to, data, role) {
  send({ t: 'signal', to, data: { ...data, role } });
}

// ---------------- login ----------------

async function initLogin() {
  const def = await invoke('default_signal_url').catch(() => '');
  S.signalUrl = store.get('signalUrl') || def || '';
  $('server-url').value = S.signalUrl;
  $('server-field').hidden = !!def && !store.get('signalUrl');
  S.token = store.get('token');
  if (S.token && S.signalUrl) return enter();
  show('login');
}

$('btn-login').addEventListener('click', async () => {
  const url = $('server-url').value.trim().replace(/\/+$/, '');
  if (!/^https?:\/\//.test(url)) {
    $('server-field').hidden = false;
    $('login-status').textContent = 'Coloque o endereço do servidor STAR (começa com https://).';
    return;
  }
  S.signalUrl = url;
  store.set('signalUrl', url);
  $('btn-login').disabled = true;
  $('login-status').textContent = 'Abrindo o Discord no navegador…';
  try {
    S.token = await invoke('login', { signalUrl: url });
    store.set('token', S.token);
    enter();
  } catch (e) {
    $('login-status').textContent = 'Não deu: ' + e;
  } finally {
    $('btn-login').disabled = false;
  }
});

$('btn-log').addEventListener('click', () => invoke('open_log_folder').catch((e) => toast('Não abriu: ' + e)));

$('btn-logout').addEventListener('click', () => {
  store.set('token', null);
  S.token = null;
  if (S.ws) S.ws.close();
  show('login');
});

function enter() {
  show('home');
  connect();
  refreshSources();
  invoke('encoder_info')
    .then((e) => ($('encoder-line').textContent = `ENCODER: ${e.label.toUpperCase()} · DETECTADO AUTOMATICAMENTE`))
    .catch(() => ($('encoder-line').textContent = 'ENCODER: NÃO DETECTADO'));
  renderConfig();
}

// ---------------- sala (servidor de encontro) ----------------

function setNet(state, text) {
  $('net-dot').className = 'dot' + (state === 'ok' ? ' ok' : state === 'bad' ? ' bad' : '');
  $('net-text').textContent = text;
}

function connect() {
  if (!S.token) return;
  const wsUrl = S.signalUrl.replace(/^http/, 'ws') + '/ws?token=' + encodeURIComponent(S.token);
  setNet('wait', 'CONECTANDO…');
  let opened = false;
  const ws = new WebSocket(wsUrl);
  S.ws = ws;
  ws.onopen = () => {
    opened = true;
    S.wsRetry = 0;
  };
  ws.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return; }
    onRoomMessage(m);
  };
  ws.onclose = () => {
    if (S.ws !== ws) return;
    setNet('bad', 'SEM CONEXÃO');
    if (!opened && S.wsRetry >= 2) {
      // Provavelmente sessão expirada
      store.set('token', null);
      S.token = null;
      toast('Sua sessão expirou. Entre de novo com o Discord.');
      show('login');
      return;
    }
    S.wsRetry++;
    setTimeout(connect, Math.min(1000 * 2 ** S.wsRetry, 15000));
  };
}

setInterval(() => send({ t: 'ping' }), 25000);

function onRoomMessage(m) {
  switch (m.t) {
    case 'hello':
      S.me = m.you;
      tryPendingWatch();
      S.ice = m.ice || [];
      setNet('ok', 'SERVIDOR ONLINE');
      $('me-name').textContent = m.you.user.name;
      if (m.you.user.avatar) $('me-avatar').src = m.you.user.avatar;
      // Reconectou no meio de uma live: volta pra lista de "ao vivo", sem avisar no Discord de novo
      if (S.live) send({ t: 'live', on: true, info: liveInfo(), notify: false });
      break;
    case 'presence':
      S.peers = m.peers || [];
      renderLiveNow();
      tryPendingWatch();
      if (S.watch && !S.peers.some((p) => p.peerId === S.watch.peerId && p.live)) {
        stopWatching('A transmissão terminou.');
      }
      break;
    case 'gone':
      if (S.viewers.has(m.peerId)) {
        S.viewers.delete(m.peerId);
        renderViewers();
        invoke('viewer_gone', { peer: m.peerId }).catch(() => {});
      }
      break;
    case 'signal':
      onSignal(m.from, m.fromUser, m.data);
      break;
  }
}

function renderLiveNow() {
  const list = S.peers.filter((p) => p.live && (!S.me || p.peerId !== S.me.peerId));
  $('live-now').hidden = list.length === 0;
  const item = (p) =>
    el(
      'div',
      { class: 'live-item' },
      avatarImg(p.user),
      el('div', { class: 'who' }, el('b', {}, p.user.name), el('span', {}, p.live.info || 'ao vivo')),
      S.watch && S.watch.peerId === p.peerId
        ? el('button', { class: 'btn-outline', onclick: () => show('watch') }, 'VENDO')
        : el('button', { class: 'btn-primary', onclick: () => startWatching(p) }, 'ASSISTIR'),
    );
  $('live-list').replaceChildren(...list.map(item));
  // No painel de quem está ao vivo também dá pra entrar na live dos outros
  $('live-others').hidden = list.length === 0;
  $('live-list-2').replaceChildren(...list.map(item));
}

function renderCross() {
  // Barra "você está ao vivo" na tela de assistir
  $('watch-livebar').hidden = !S.live;
  $('watch-livebar-n').textContent = String(S.viewers.size);
  // Quadro "você está assistindo" no painel ao vivo
  $('live-watching').hidden = !S.watch;
  if (S.watch) $('live-watching-who').textContent = S.watch.user.name.toUpperCase();
  renderLiveNow();
}
$('watch-livebar').addEventListener('click', () => show('live'));
$('btn-back-video').addEventListener('click', () => show('watch'));

function onSignal(from, fromUser, data) {
  if (!data || !data.kind) return;
  // Eu assistindo: mensagens de quem transmite
  if (data.role === 'h') {
    if (S.watch && from === S.watch.peerId) onViewerSignal(data);
    return;
  }
  // Eu transmitindo: mensagens de quem assiste
  if (data.role === 'v' && S.live && ['join', 'answer', 'ice', 'leave'].includes(data.kind)) {
    const { role, ...core } = data;
    data = core;
    if (data.kind === 'join') {
      S.viewers.set(from, { user: fromUser, state: 'conectando' });
      renderViewers();
    }
    if (data.kind === 'leave') {
      S.viewers.delete(from);
      renderViewers();
    }
    // Em fila: o candidato ICE não pode chegar no núcleo antes da answer
    signalQueue = signalQueue
      .then(() => invoke('signal_in', { from, data }))
      .catch((e) => console.warn('signal_in', e));
  }
}
let signalQueue = Promise.resolve();

// ---------------- configurar ----------------

async function refreshSources() {
  try {
    S.sources = await invoke('list_sources');
  } catch {
    S.sources = [];
  }
  if (!S.cfg.source || !S.sources.some((s) => s.handle === S.cfg.source.handle)) {
    S.cfg.source = S.sources[0] || null;
  }
  renderSources();
  renderSummary();
  refreshThumbs();
  refreshPreview();
}
$('btn-refresh').addEventListener('click', refreshSources);

function renderSources() {
  const box = $('sources');
  if (!S.sources.length) {
    box.replaceChildren(el('p', { class: 'mono dim small' }, 'Nenhuma tela encontrada.'));
    return;
  }
  box.replaceChildren(
    ...S.sources.map((s) => {
      const on = S.cfg.source && S.cfg.source.handle === s.handle;
      return el(
        'button',
        {
          class: 'src' + (on ? ' on' : ''),
          type: 'button',
          title: s.name,
          onclick: () => {
            S.cfg.source = s;
            renderSources();
            renderSummary();
            refreshPreview();
          },
        },
        el('span', { class: 'thumb' }, thumbImg(s), el('span', {}, s.detail.toUpperCase())),
        el(
          'span',
          { class: 'meta' },
          el('span', { class: 'kind' }, s.kind === 'monitor' ? 'TELA' : 'JANELA'),
          el('span', { class: 'name' }, s.name),
        ),
      );
    }),
  );
}

async function renderAudioApps() {
  let apps = [];
  try { apps = await invoke('audio_apps'); } catch {}
  const sel = $('audio-app');
  const srcPid = S.cfg.source && S.cfg.source.pid;
  sel.replaceChildren(...apps.map((a) => el('option', { value: String(a.pid) }, a.name)));
  // Sugere o programa da janela escolhida
  const pick = apps.find((a) => a.pid === S.cfg.appPid) || apps.find((a) => a.pid === srcPid) || apps[0];
  if (pick) {
    sel.value = String(pick.pid);
    S.cfg.appPid = pick.pid;
  }
}
$('audio-app').addEventListener('change', (e) => {
  S.cfg.appPid = Number(e.target.value);
  renderSummary();
});

function renderConfig() {
  for (const b of document.querySelectorAll('[data-audio]')) b.classList.toggle('on', b.dataset.audio === S.cfg.audio);
  for (const b of document.querySelectorAll('[data-preset]')) b.classList.toggle('on', b.dataset.preset === S.cfg.preset);
  $('app-picker').hidden = S.cfg.audio !== 'app';
  if (S.cfg.audio === 'app') renderAudioApps();
  $('vol').value = S.cfg.volume;
  $('vol-val').textContent = S.cfg.volume + '%';
  renderSummary();
}

for (const b of document.querySelectorAll('[data-audio]')) {
  b.addEventListener('click', () => {
    S.cfg.audio = b.dataset.audio;
    store.set('audio', S.cfg.audio);
    renderConfig();
  });
}
for (const b of document.querySelectorAll('[data-preset]')) {
  b.addEventListener('click', () => {
    S.cfg.preset = b.dataset.preset;
    store.set('preset', S.cfg.preset);
    renderConfig();
  });
}
$('notify').checked = S.cfg.notify;
$('notify').addEventListener('change', (e) => {
  S.cfg.notify = e.target.checked;
  store.set('notify', S.cfg.notify ? '1' : '0');
});

$('vol').addEventListener('input', (e) => {
  S.cfg.volume = Number(e.target.value);
  store.set('volume', String(S.cfg.volume));
  $('vol-val').textContent = S.cfg.volume + '%';
});

function renderSummary() {
  $('sum-src').textContent = S.cfg.source ? S.cfg.source.name : '—';
  $('sum-q').textContent = PRESETS[S.cfg.preset];
  $('sum-a').textContent = AUDIO_LABEL[S.cfg.audio] + (S.cfg.audio === 'off' ? '' : ' (sem Discord)');
}

function liveInfo() {
  const src = S.cfg.source ? S.cfg.source.name : '';
  return `${src} · ${PRESETS[S.cfg.preset]}`.slice(0, 120);
}

// ---------------- transmitir ----------------

$('btn-golive').addEventListener('click', goLive);

async function goLive() {
  if (!S.cfg.source) return toast('Escolha uma tela ou janela primeiro.');
  if (!S.me) return toast('Ainda conectando ao servidor STAR, tenta de novo em um segundo.');
  const audio =
    S.cfg.audio === 'app' ? { mode: 'app', pid: S.cfg.appPid || 0 } : { mode: S.cfg.audio };
  const config = {
    source: S.cfg.source.kind === 'monitor'
      ? { kind: 'monitor', handle: S.cfg.source.handle }
      : { kind: 'window', hwnd: S.cfg.source.handle },
    preset: S.cfg.preset,
    audio,
    volume: S.cfg.volume,
  };
  $('btn-golive').disabled = true;
  $('home-status').textContent = 'Ligando a captura…';
  try {
    await invoke('go_live', { config, ice: S.ice });
  } catch (e) {
    $('home-status').textContent = 'Não consegui iniciar: ' + e;
    $('btn-golive').disabled = false;
    return;
  }
  $('btn-golive').disabled = false;
  $('home-status').textContent = '';
  S.live = true;
  S.liveSince = Date.now();
  S.viewers.clear();
  send({ t: 'live', on: true, info: liveInfo(), notify: S.cfg.notify });

  $('live-src').textContent = S.cfg.source.name;
  $('live-src2').textContent = S.cfg.source.name.toUpperCase();
  $('live-q').textContent = PRESETS[S.cfg.preset];
  $('live-audio').textContent = AUDIO_LABEL[S.cfg.audio] + ' · ' + S.cfg.volume + '%';
  $('live-vol').value = S.cfg.volume;
  $('live-vol-val').textContent = S.cfg.volume + '%';
  ['t-fps', 't-kbps', 't-enc'].forEach((id) => ($(id).textContent = '—'));
  renderViewers();
  renderCross();
  show('live');
}

$('btn-stop').addEventListener('click', stopLive);

async function stopLive() {
  S.live = false;
  send({ t: 'live', on: false });
  await invoke('stop_live').catch(() => {});
  S.viewers.clear();
  renderCross();
  show(S.watch ? 'watch' : 'home');
}

$('live-vol').addEventListener('input', (e) => {
  const v = Number(e.target.value);
  S.cfg.volume = v;
  store.set('volume', String(v));
  $('live-vol-val').textContent = v + '%';
  $('live-audio').textContent = AUDIO_LABEL[S.cfg.audio] + ' · ' + v + '%';
  invoke('set_volume', { volume: v }).catch(() => {});
});

setInterval(() => {
  if (!S.live) return;
  const s = Math.floor((Date.now() - S.liveSince) / 1000);
  const p = (n) => String(n).padStart(2, '0');
  $('live-timer').textContent = `${p(Math.floor(s / 3600))}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}`;
}, 1000);

function renderViewers() {
  $('watch-livebar-n').textContent = String(S.viewers.size);
  const box = $('viewer-list');
  $('viewer-count').textContent = String(S.viewers.size);
  $('t-viewers').textContent = String(S.viewers.size);
  if (!S.viewers.size) {
    box.replaceChildren(el('p', { class: 'mono dim small pad' }, 'Ninguém ainda. O aviso já foi pro Discord.'));
    return;
  }
  box.replaceChildren(
    ...[...S.viewers.values()].map((v) =>
      el(
        'div',
        { class: 'viewer' },
        avatarImg(v.user),
        el('b', {}, v.user ? v.user.name : 'amigo'),
        el('span', { class: 'st' + (v.state === 'connected' ? ' ok' : '') }, v.state === 'connected' ? 'ESTÁVEL' : v.state.toUpperCase()),
      ),
    ),
  );
}

// Eventos do núcleo em Rust
listen('hub', ({ payload: ev }) => {
  switch (ev.type) {
    case 'signal':
      sendSignal(ev.to, ev.data, 'h');
      break;
    case 'viewer_state': {
      const v = S.viewers.get(ev.peer);
      if (v) {
        v.state = ev.state;
        if (ev.state === 'failed' || ev.state === 'closed') S.viewers.delete(ev.peer);
        renderViewers();
      }
      break;
    }
    case 'stats':
      $('t-fps').textContent = ev.fps ? ev.fps.toFixed(0) : '—';
      $('t-kbps').textContent = (ev.kbps / 1000).toFixed(1) + ' Mbps';
      $('t-enc').textContent = ev.encoder;
      break;
    case 'error':
      toast(ev.message);
      $('live-status').textContent = ev.message;
      break;
  }
});

listen('update', ({ payload: version }) => toast(`Atualizando o STAR Stream pra versão ${version}… o app reabre sozinho.`));

// ---------------- assistir ----------------

function startWatching(peer) {
  stopWatching(null, true);
  const pc = new RTCPeerConnection({ iceServers: S.ice });
  const w = { peerId: peer.peerId, user: peer.user, pc, pendingIce: [], remoteSet: false, statsTimer: null, lastBytes: 0, lastTs: 0 };
  S.watch = w;

  const video = $('video');
  video.volume = Number($('watch-vol').value) / 100;
  const stream = new MediaStream();
  video.srcObject = stream;

  pc.ontrack = (ev) => {
    stream.addTrack(ev.track);
    // Menor atraso possível no amigo
    try { ev.receiver.playoutDelayHint = 0; } catch {}
    try { ev.receiver.jitterBufferTarget = 0; } catch {}
    video.play().catch(() => {});
  };
  pc.onicecandidate = (ev) => {
    if (ev.candidate) jsLog('info', 'assistir: meu candidato ' + ev.candidate.candidate);
    if (ev.candidate) sendSignal(w.peerId, { kind: 'ice', candidate: ev.candidate.toJSON() }, 'v');
  };
  pc.onconnectionstatechange = () => {
    const st = pc.connectionState;
    if (st === 'connected') $('watch-msg').textContent = '';
    if (st === 'connecting') $('watch-msg').textContent = 'CONECTANDO…';
    jsLog('info', 'assistir: conexão ' + st);
    if (st === 'connected') logPair(pc);
    if (st === 'failed') {
      $('watch-msg').textContent = 'NÃO CONSEGUI CONECTAR. Confiram se os dois estão com o Radmin VPN ligado e na mesma rede.';
      logPair(pc);
    }
    if (st === 'disconnected') $('watch-msg').textContent = 'CONEXÃO OSCILANDO…';
  };

  $('watch-who').textContent = peer.user.name.toUpperCase();
  $('watch-msg').textContent = 'CONECTANDO…';
  $('watch-stats').textContent = '—';
  show('watch');
  renderCross();
  sendSignal(peer.peerId, { kind: 'join' }, 'v');

  w.statsTimer = setInterval(async () => {
    if (!S.watch || S.watch !== w) return;
    const r = await pc.getStats();
    r.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') {
        const kbps = w.lastTs ? ((s.bytesReceived - w.lastBytes) * 8) / (s.timestamp - w.lastTs) : 0;
        w.lastBytes = s.bytesReceived;
        w.lastTs = s.timestamp;
        const res = s.frameHeight ? `${s.frameHeight}p` : '—';
        const fps = s.framesPerSecond ? Math.round(s.framesPerSecond) + 'fps' : '';
        $('watch-stats').textContent = `${res} · ${fps} · ${(kbps / 1000).toFixed(1)} Mbps`;
      }
    });
  }, 1000);
}

// Registra no diagnóstico por qual caminho a conexão foi (ou tentou ir)
async function logPair(pc) {
  try {
    const r = await pc.getStats();
    const all = {};
    r.forEach((x) => (all[x.id] = x));
    Object.values(all)
      .filter((x) => x.type === 'candidate-pair')
      .forEach((p) => {
        const l = all[p.localCandidateId] || {};
        const rm = all[p.remoteCandidateId] || {};
        jsLog('info', `par ${p.state}${p.nominated ? ' (escolhido)' : ''}: ${l.address || '?'}:${l.port} ${l.candidateType} -> ${rm.address || '?'}:${rm.port} ${rm.candidateType}`);
      });
  } catch (e) {
    jsLog('warn', 'getStats: ' + e);
  }
}

async function onViewerSignal(data) {
  const w = S.watch;
  if (!w) return;
  try {
    if (data.kind === 'offer') {
      await w.pc.setRemoteDescription({ type: 'offer', sdp: data.sdp });
      w.remoteSet = true;
      for (const c of w.pendingIce.splice(0)) await w.pc.addIceCandidate(c).catch(() => {});
      const answer = await w.pc.createAnswer();
      await w.pc.setLocalDescription(answer);
      sendSignal(w.peerId, { kind: 'answer', sdp: answer.sdp }, 'v');
    } else if (data.kind === 'ice') {
      jsLog('info', 'assistir: candidato de quem transmite ' + (data.candidate && data.candidate.candidate));
      if (w.remoteSet) await w.pc.addIceCandidate(data.candidate).catch(() => {});
      else w.pendingIce.push(data.candidate);
    }
  } catch (e) {
    $('watch-msg').textContent = 'ERRO: ' + e.message;
  }
}

function stopWatching(reason, switching) {
  const w = S.watch;
  if (!w) return;
  S.watch = null;
  clearInterval(w.statsTimer);
  sendSignal(w.peerId, { kind: 'leave' }, 'v');
  try { w.pc.close(); } catch {}
  $('video').srcObject = null;
  if (!switching) show(S.live ? 'live' : 'home');
  renderCross();
  if (reason) toast(reason);
}

$('btn-leave').addEventListener('click', () => stopWatching());
$('watch-vol').addEventListener('input', (e) => {
  const v = $('video');
  v.volume = Number(e.target.value) / 100;
  v.muted = false;
});
$('btn-mute').addEventListener('click', () => {
  const v = $('video');
  v.muted = !v.muted;
  $('btn-mute').setAttribute('aria-label', v.muted ? 'Ativar som' : 'Mutar');
});
$('btn-full').addEventListener('click', () => {
  const p = $('player');
  if (document.fullscreenElement) document.exitFullscreen();
  else p.requestFullscreen().catch(() => {});
});
$('player').addEventListener('dblclick', () => $('btn-full').click());

window.addEventListener('beforeunload', () => {
  if (S.live) send({ t: 'live', on: false });
  if (S.watch) sendSignal(S.watch.peerId, { kind: 'leave' }, 'v');
});

// ---------------- prévias das telas ----------------

const thumbs = new Map(); // handle -> data URL
function thumbImg(s) {
  const img = el('img', { alt: '' });
  const url = thumbs.get(s.handle);
  if (url) img.src = url;
  else img.hidden = true;
  img.dataset.handle = String(s.handle);
  return img;
}

let thumbsBusy = false;
async function refreshThumbs() {
  if (thumbsBusy || $('screen-home').hidden || !S.sources.length) return;
  thumbsBusy = true;
  try {
    for (const s of S.sources) {
      const url = await invoke('source_thumb', { kind: s.kind, handle: s.handle, w: 320, h: 180 }).catch(() => null);
      if (!url) continue;
      thumbs.set(s.handle, url);
      const img = document.querySelector(`#sources img[data-handle="${s.handle}"]`);
      if (img) {
        img.src = url;
        img.hidden = false;
      }
    }
  } finally {
    thumbsBusy = false;
  }
}

let previewBusy = false;
async function refreshPreview() {
  if (previewBusy || $('screen-home').hidden || !S.cfg.source) return;
  previewBusy = true;
  try {
    const s = S.cfg.source;
    const url = await invoke('source_thumb', { kind: s.kind, handle: s.handle, w: 640, h: 360 }).catch(() => null);
    if (S.cfg.source !== s) return;
    $('preview-img').hidden = !url;
    $('preview-txt').hidden = !!url;
    $('preview-txt').textContent = url ? '' : 'SEM PRÉVIA (janela minimizada?)';
    if (url) $('preview-img').src = url;
  } finally {
    previewBusy = false;
  }
}
setInterval(refreshThumbs, 5000);
setInterval(refreshPreview, 1500);

// ---------------- link do aviso (starstream://assistir/ID) ----------------

let pendingWatchId = null;
let pendingWatchAt = 0;
function onOpenUrl(url) {
  const m = /^starstream:\/\/assistir\/(\d+)/.exec(url || '');
  if (!m) return;
  pendingWatchId = m[1];
  pendingWatchAt = Date.now();
  jsLog('info', 'link: assistir ' + pendingWatchId);
  tryPendingWatch();
  setTimeout(tryPendingWatch, 15500);
}

function tryPendingWatch() {
  if (!pendingWatchId || !S.me) return;
  const peer = S.peers.find((p) => p.live && p.user.id === pendingWatchId && p.peerId !== S.me.peerId);
  if (peer) {
    pendingWatchId = null;
    if (!S.watch || S.watch.peerId !== peer.peerId) startWatching(peer);
    else show('watch');
    return;
  }
  // Ainda não chegou a lista de quem está ao vivo: espera um pouco
  if (Date.now() - pendingWatchAt > 15000) {
    pendingWatchId = null;
    toast('Essa transmissão já terminou.');
  }
}

listen('open-url', ({ payload }) => onOpenUrl(payload));
invoke('take_pending_url').then(onOpenUrl).catch(() => {});

initLogin();
