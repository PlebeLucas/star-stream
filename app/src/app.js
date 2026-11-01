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
    autoQuality: store.get('autoQuality') !== '0',
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
  let screens = [];
  let cams = [];
  try { screens = await invoke('list_sources'); } catch {}
  try { cams = await invoke('webcams'); } catch {}
  // Webcams entram na mesma lista, identificadas pelo nome
  S.sources = [
    ...screens,
    ...cams.map((name) => ({ kind: 'webcam', handle: 'cam:' + name, name, detail: 'Webcam', pid: 0 })),
  ];
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
          el('span', { class: 'kind' }, { monitor: 'TELA', window: 'JANELA', webcam: 'WEBCAM' }[s.kind] || ''),
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
$('auto-quality').checked = S.cfg.autoQuality;
$('auto-quality').addEventListener('change', (e) => {
  S.cfg.autoQuality = e.target.checked;
  store.set('autoQuality', S.cfg.autoQuality ? '1' : '0');
});
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
    source:
      S.cfg.source.kind === 'monitor'
        ? { kind: 'monitor', handle: S.cfg.source.handle }
        : S.cfg.source.kind === 'webcam'
          ? { kind: 'webcam', name: S.cfg.source.name }
          : { kind: 'window', hwnd: S.cfg.source.handle },
    preset: S.cfg.preset,
    audio,
    volume: S.cfg.volume,
    auto_quality: S.cfg.autoQuality,
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
  invoke('overlay_show').catch((e) => jsLog('warn', 'overlay: ' + e));
  resetLiveIx();
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
    case 'interaction':
      onInteraction(ev.peer, ev.msg);
      break;
    case 'quality': {
      const label = PRESETS[ev.preset] || ev.preset;
      $('live-q').textContent = label;
      toast(`Qualidade reduzida para ${label}: ${ev.reason}.`);
      break;
    }
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
  pc.ondatachannel = (ev) => viewerChannel(w, ev.channel);
  resetViewerIx();
  S.watch = w;

  const video = $('video');
  video.volume = Number($('watch-vol').value) / 100;
  const stream = new MediaStream();
  video.srcObject = stream;

  pc.ontrack = (ev) => {
    stream.addTrack(ev.track);
    // Menor atraso possível no amigo
    // Uma folga pequena (150 ms) absorve as oscilações da internet sem atraso perceptível.
    // Com zero, qualquer pacote atrasado virava quadro perdido (as "quedas de FPS").
    try { ev.receiver.jitterBufferTarget = 150; } catch {}
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
        const got = (s.packetsReceived || 0) + (s.packetsLost || 0);
        const lossPct = got ? (100 * (s.packetsLost || 0)) / got : 0;
        $('watch-stats').textContent = `${res} · ${fps} · ${(kbps / 1000).toFixed(1)} Mbps` + (lossPct >= 0.5 ? ` · perda ${lossPct.toFixed(1)}%` : '');
        // A cada 5 s, registra no diagnóstico o que dá pra saber de travadas
        w.tick = (w.tick || 0) + 1;
        if (w.tick % 5 === 0) {
          const d = (k) => (s[k] || 0) - ((w.prev && w.prev[k]) || 0);
          jsLog('info', `assistir: ${res} ${fps} ${(kbps / 1000).toFixed(1)}Mbps | perdidos +${d('packetsLost')} | quadros descartados +${d('framesDropped')} | congelou +${d('freezeCount')} (${d('totalFreezesDuration').toFixed(1)}s) | NACK +${d('nackCount')} | pedidos de imagem +${d('pliCount')} | jitter ${((s.jitter || 0) * 1000).toFixed(0)}ms | decoder ${s.decoderImplementation || '?'}`);
          w.prev = { ...s };
          logPair(pc);
        }
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
  endControl(false);
  resetViewerIx();
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
      if (s.kind === 'webcam') continue;
      const url = await invoke('source_thumb', { kind: s.kind, handle: s.handle, w: 320, h: 180 }).catch(() => null);
      if (!url) continue;
      thumbs.set(s.handle, url);
      const img = document.querySelector(`#sources img[data-handle="${CSS.escape(String(s.handle))}"]`);
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
    let url;
    if (s.kind === 'webcam') {
      // Foto da câmera só quando escolhe (e guarda), pra não ficar ligando a câmera
      url = thumbs.get(s.handle);
      if (!url) {
        $('preview-txt').hidden = false;
        $('preview-img').hidden = true;
        $('preview-txt').textContent = 'LIGANDO A CÂMERA…';
        url = await invoke('webcam_thumb', { name: s.name }).catch(() => null);
        if (url) {
          thumbs.set(s.handle, url);
          const img = document.querySelector(`#sources img[data-handle="${CSS.escape(String(s.handle))}"]`);
          if (img) {
            img.src = url;
            img.hidden = false;
          }
        }
      }
    } else {
      url = await invoke('source_thumb', { kind: s.kind, handle: s.handle, w: 640, h: 360 }).catch(() => null);
    }
    if (S.cfg.source !== s) return;
    $('preview-img').hidden = !url;
    $('preview-txt').hidden = !!url;
    $('preview-txt').textContent = url ? '' : s.kind === 'webcam' ? 'CÂMERA SEM IMAGEM (em uso por outro programa?)' : 'SEM PRÉVIA (janela minimizada?)';
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

// =====================================================================
// INTERAÇÃO: reações, ping, clipa isso, controle remoto
// =====================================================================

const fxEmit = (msg) => {
  try {
    T && T.event.emitTo('overlay', 'fx', msg);
  } catch (e) {
    jsLog('warn', 'fx: ' + e);
  }
};
const nameOf = (peer) => (S.viewers.get(peer) && S.viewers.get(peer).user && S.viewers.get(peer).user.name) || 'amigo';
const myName = () => (S.me && S.me.user && S.me.user.name) || 'eu';

// ---------- quem assiste ----------

const VX = { dc: null, kind: null, canCtl: false, pinging: false, controlling: false, waitingCtl: false, clipCooldown: 0, lastMove: 0, held: new Set() };

function resetViewerIx() {
  VX.dc = null;
  VX.kind = null;
  VX.canCtl = false;
  VX.waitingCtl = false;
  setPinging(false);
  $('ixbar').hidden = true;
  $('ix-hint').hidden = true;
  STARFX.banner($('player'), '');
}

// Botões de reação (montados uma vez)
$('ix-reacts').replaceChildren(
  ...Object.entries(STARFX.REACTIONS).map(([k, r]) => {
    const b = el('button', { class: 'ix-react', type: 'button', title: r.label, 'aria-label': r.label });
    b.style.setProperty('--c', r.c);
    b.innerHTML = r.icon + `<span>${r.label}</span>`;
    b.addEventListener('click', () => sendReact(k));
    return b;
  }),
);

function vsend(msg) {
  if (VX.dc && VX.dc.readyState === 'open') VX.dc.send(JSON.stringify(msg));
}

function viewerChannel(w, dc) {
  VX.dc = dc;
  dc.onopen = () => {
    if (S.watch !== w) return;
    $('ixbar').hidden = false;
    vsend({ t: 'hi' });
  };
  dc.onclose = () => {
    if (S.watch === w) resetViewerIx();
  };
  dc.onmessage = (ev) => {
    let m;
    try { m = JSON.parse(ev.data); } catch { return; }
    onViewerMsg(m);
  };
}

function onViewerMsg(m) {
  const player = $('player');
  const mine = S.me && m.by === S.me.peerId;
  switch (m.t) {
    case 'hello':
      VX.kind = m.kind;
      VX.canCtl = !!m.ctl;
      $('ix-ping').disabled = m.kind === 'webcam';
      $('ix-ctl').disabled = !m.ctl;
      $('ix-ctl').title = m.ctl ? 'Pedir pra controlar o mouse e o teclado de quem transmite' : 'Quem transmite não está aceitando controle remoto';
      break;
    case 'react':
      if (!mine) STARFX.react(player, m.e, m.name);
      break;
    case 'ping':
      if (!mine) STARFX.ping(player, m.x, m.y, m.name, contentBox());
      break;
    case 'clipped':
      STARFX.flash(player, 'CLIP SALVO', `pedido por ${m.name}`);
      break;
    case 'clip-fail':
      toast('Não deu pra clipar: ' + (m.msg || 'erro'));
      break;
    case 'ctl':
      if (m.state === 'granted') startControl(m.minutes);
      else {
        VX.waitingCtl = false;
        endControl(false);
        if (m.state === 'denied') toast('Pedido de controle recusado' + (m.reason ? `: ${m.reason}` : '.'));
        if (m.state === 'revoked') toast('Controle encerrado' + (m.reason ? `: ${m.reason}` : '.'));
      }
      renderCtlBtn();
      break;
  }
}

/** Área do vídeo de verdade dentro do player (sem as faixas pretas). */
function contentBox() {
  const v = $('video');
  const pw = $('player').clientWidth;
  const ph = $('player').clientHeight;
  const vw = v.videoWidth || 16;
  const vh = v.videoHeight || 9;
  const scale = Math.min(pw / vw, ph / vh);
  const width = vw * scale;
  const height = vh * scale;
  return { left: (pw - width) / 2, top: (ph - height) / 2, width, height };
}

/** Posição do ponteiro em 0..1 dentro do vídeo, ou null se estiver nas faixas pretas. */
function videoPoint(e) {
  const r = $('player').getBoundingClientRect();
  const b = contentBox();
  const x = (e.clientX - r.left - b.left) / b.width;
  const y = (e.clientY - r.top - b.top) / b.height;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x: Math.round(x * 10000) / 10000, y: Math.round(y * 10000) / 10000 };
}

let reactTimes = [];
function sendReact(kind) {
  const now = Date.now();
  reactTimes = reactTimes.filter((t) => now - t < 2000);
  if (reactTimes.length >= 6) return; // anti-spam
  reactTimes.push(now);
  STARFX.react($('player'), kind, myName());
  vsend({ t: 'react', e: kind });
}

function setPinging(on) {
  VX.pinging = on && !VX.controlling && VX.kind !== 'webcam';
  $('ix-ping').setAttribute('aria-pressed', VX.pinging ? 'true' : 'false');
  $('player').classList.toggle('pinging', VX.pinging);
  if (VX.pinging) hint('MODO PING: clique no vídeo pra marcar · P ou ESC sai');
  else if (!VX.controlling) $('ix-hint').hidden = true;
}
function hint(t) {
  $('ix-hint').textContent = t;
  $('ix-hint').hidden = false;
}
$('ix-ping').addEventListener('click', () => setPinging(!VX.pinging));

$('player').addEventListener('click', (e) => {
  if (!VX.pinging) return;
  const p = videoPoint(e);
  if (!p) return;
  STARFX.ping($('player'), p.x, p.y, myName(), contentBox());
  vsend({ t: 'ping', x: p.x, y: p.y });
});

$('ix-clip').addEventListener('click', () => {
  if (Date.now() < VX.clipCooldown) return;
  VX.clipCooldown = Date.now() + 10000;
  vsend({ t: 'clip' });
  const b = $('ix-clip');
  b.disabled = true;
  setTimeout(() => (b.disabled = false), 10000);
});

function renderCtlBtn() {
  const b = $('ix-ctl');
  b.classList.toggle('on', VX.controlling);
  $('ix-ctl-txt').textContent = VX.controlling ? 'SOLTAR CONTROLE' : VX.waitingCtl ? 'AGUARDANDO…' : 'PEDIR CONTROLE';
  b.disabled = !VX.canCtl && !VX.controlling;
}
$('ix-ctl').addEventListener('click', () => {
  if (VX.controlling) {
    vsend({ t: 'ctl-release' });
    endControl(false);
  } else if (!VX.waitingCtl) {
    VX.waitingCtl = true;
    vsend({ t: 'ctl-req' });
    setTimeout(() => {
      if (VX.waitingCtl) {
        VX.waitingCtl = false;
        renderCtlBtn();
      }
    }, 25000);
  }
  renderCtlBtn();
});

function startControl(minutes) {
  VX.waitingCtl = false;
  VX.controlling = true;
  setPinging(false);
  $('player').classList.add('controlling');
  STARFX.banner($('player'), 'VOCÊ ESTÁ NO CONTROLE', `${minutes} MIN · CTRL+ALT+X SOLTA`);
  hint('Mouse e teclado vão pro PC de quem transmite enquanto o ponteiro estiver no vídeo.');
  $('player').focus();
  renderCtlBtn();
}

function endControl(notify) {
  if (!VX.controlling) return;
  for (const vk of VX.held) vsend({ t: 'in', k: 'ku', vk });
  VX.held.clear();
  VX.controlling = false;
  $('player').classList.remove('controlling');
  STARFX.banner($('player'), '');
  $('ix-hint').hidden = true;
  if (notify) vsend({ t: 'ctl-release' });
  renderCtlBtn();
}

// Mouse e teclado enquanto controla
$('player').tabIndex = 0;
$('player').addEventListener('pointermove', (e) => {
  if (!VX.controlling) return;
  const now = performance.now();
  if (now - VX.lastMove < 16) return; // ~60 por segundo
  VX.lastMove = now;
  const p = videoPoint(e);
  if (p) vsend({ t: 'in', k: 'm', x: p.x, y: p.y });
});
$('player').addEventListener('pointerdown', (e) => {
  if (!VX.controlling) return;
  e.preventDefault();
  $('player').focus();
  const p = videoPoint(e);
  if (p) vsend({ t: 'in', k: 'd', b: e.button, x: p.x, y: p.y });
});
$('player').addEventListener('pointerup', (e) => {
  if (!VX.controlling) return;
  const p = videoPoint(e) || {};
  vsend({ t: 'in', k: 'u', b: e.button, x: p.x, y: p.y });
});
$('player').addEventListener('wheel', (e) => {
  if (!VX.controlling) return;
  e.preventDefault();
  vsend({ t: 'in', k: 'w', dy: e.deltaY });
}, { passive: false });
$('player').addEventListener('contextmenu', (e) => {
  if (VX.controlling) e.preventDefault();
});
$('player').addEventListener('dblclick', (e) => {
  if (VX.controlling || VX.pinging) e.stopImmediatePropagation();
}, true);

window.addEventListener('keydown', (e) => {
  if ($('screen-watch').hidden) return;
  // Ctrl+Alt+X: solta o controle
  if (VX.controlling && e.ctrlKey && e.altKey && e.code === 'KeyX') {
    e.preventDefault();
    endControl(true);
    return;
  }
  if (VX.controlling) {
    e.preventDefault();
    if (!e.repeat) VX.held.add(e.keyCode);
    vsend({ t: 'in', k: 'kd', vk: e.keyCode });
    return;
  }
  if (e.target && /INPUT|SELECT|TEXTAREA/.test(e.target.tagName)) return;
  if (e.code === 'KeyP' && !$('ixbar').hidden) setPinging(!VX.pinging);
  if (e.code === 'Escape' && VX.pinging) setPinging(false);
}, true);
window.addEventListener('keyup', (e) => {
  if (!VX.controlling) return;
  e.preventDefault();
  VX.held.delete(e.keyCode);
  vsend({ t: 'in', k: 'ku', vk: e.keyCode });
}, true);
window.addEventListener('blur', () => {
  // Saiu da janela com tecla apertada: solta tudo no PC do outro
  for (const vk of VX.held) vsend({ t: 'in', k: 'ku', vk });
  VX.held.clear();
});

// ---------- quem transmite ----------

const LX = { clipBusy: false, lastClip: 0, reactRate: new Map(), pending: null, ctlPeer: null, ctlUntil: 0, timer: null };

const allowCtlInit = store.get('allowCtl') !== '0';
$('allow-ctl').checked = allowCtlInit;
$('allow-ctl').addEventListener('change', (e) => store.set('allowCtl', e.target.checked ? '1' : '0'));

function resetLiveIx() {
  LX.pending = null;
  LX.ctlPeer = null;
  $('ctl-state').hidden = true;
  $('ctl-modal').hidden = true;
  $('ix-feed').replaceChildren(el('li', { class: 'dim' }, 'Reações, pings e clipes da galera aparecem aqui.'));
}

function feed(kind, text) {
  const ul = $('ix-feed');
  const first = ul.firstElementChild;
  if (first && first.classList.contains('dim')) first.remove();
  const now = new Date();
  const hh = String(now.getHours()).padStart(2, '0') + ':' + String(now.getMinutes()).padStart(2, '0');
  ul.prepend(el('li', {}, el('span', { class: 'k' }, kind), el('span', { class: 'grow' }, text), el('span', { class: 'dim' }, hh)));
  while (ul.children.length > 30) ul.lastChild.remove();
}

const bcast = (msg) => invoke('hub_broadcast', { msg }).catch(() => {});
const sendTo = (peer, msg) => invoke('hub_send', { peer, msg }).catch(() => {});

function onInteraction(peer, m) {
  if (!S.live || !m || typeof m.t !== 'string') return;
  const name = nameOf(peer);
  const webcam = S.cfg.source && S.cfg.source.kind === 'webcam';
  switch (m.t) {
    case 'hi':
      sendTo(peer, { t: 'hello', kind: S.cfg.source ? S.cfg.source.kind : 'monitor', ctl: $('allow-ctl').checked && !webcam });
      break;
    case 'react': {
      if (!STARFX.REACTIONS[m.e]) return;
      const now = Date.now();
      const times = (LX.reactRate.get(peer) || []).filter((t) => now - t < 2000);
      if (times.length >= 6) return;
      times.push(now);
      LX.reactRate.set(peer, times);
      STARFX.react(document.querySelector('#screen-live .stage'), m.e, name);
      fxEmit({ t: 'react', e: m.e, name });
      bcast({ t: 'react', e: m.e, by: peer, name });
      feed('REAÇÃO', `${name} · ${STARFX.REACTIONS[m.e].label}`);
      break;
    }
    case 'ping': {
      if (webcam) return;
      const x = Math.min(1, Math.max(0, Number(m.x) || 0));
      const y = Math.min(1, Math.max(0, Number(m.y) || 0));
      fxEmit({ t: 'ping', x, y, name });
      STARFX.ping(document.querySelector('#screen-live .stage'), x, y, name);
      bcast({ t: 'ping', x, y, by: peer, name });
      feed('PING', name);
      break;
    }
    case 'clip':
      doClip(name, peer);
      break;
    case 'ctl-req':
      onCtlRequest(peer, name);
      break;
    case 'ctl-release':
      if (LX.ctlPeer === peer) invoke('remote_revoke', { reason: `${name} soltou o controle` }).catch(() => {});
      break;
  }
}

function stamp() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}_${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}`;
}

async function doClip(name, peer) {
  if (LX.clipBusy || Date.now() - LX.lastClip < 10000) {
    if (peer) sendTo(peer, { t: 'clip-fail', msg: 'acabaram de clipar, espera uns segundos' });
    return;
  }
  LX.clipBusy = true;
  LX.lastClip = Date.now();
  try {
    const slug = String(name).normalize('NFD').replace(/[^\w-]+/g, '').slice(0, 24) || 'amigo';
    const path = await invoke('clip_save', { name: `clip_${stamp()}_${slug}` });
    feed('CLIPE', `${name} clipou · salvo`);
    fxEmit({ t: 'flash', text: 'CLIP SALVO', sub: `pedido por ${name}` });
    STARFX.flash(document.querySelector('#screen-live .stage'), 'CLIP SALVO', `pedido por ${name}`);
    bcast({ t: 'clipped', name });
    jsLog('info', 'clipe: ' + path);
  } catch (e) {
    feed('CLIPE', `falhou: ${e}`);
    if (peer) sendTo(peer, { t: 'clip-fail', msg: String(e) });
    else toast('Não deu pra clipar: ' + e);
  } finally {
    LX.clipBusy = false;
  }
}
$('btn-my-clip').addEventListener('click', () => doClip(myName(), null));
$('btn-clips').addEventListener('click', () => invoke('open_clips_folder').catch((e) => toast('Não abriu: ' + e)));

function onCtlRequest(peer, name) {
  const webcam = S.cfg.source && S.cfg.source.kind === 'webcam';
  const deny = (reason) => sendTo(peer, { t: 'ctl', state: 'denied', reason });
  if (!$('allow-ctl').checked) return deny('controle remoto desligado');
  if (webcam) return deny('transmitindo a webcam');
  if (LX.ctlPeer) return deny('outra pessoa já está no controle');
  if (LX.pending) return deny('já tem um pedido aberto');
  feed('CONTROLE', `${name} pediu o controle`);
  let left = 20;
  LX.pending = { peer, name, timer: null };
  $('ctl-name').textContent = name;
  const u = S.viewers.get(peer) && S.viewers.get(peer).user;
  if (u && u.avatar) $('ctl-avatar').src = u.avatar;
  else $('ctl-avatar').removeAttribute('src');
  $('ctl-count').textContent = left + 's';
  $('ctl-modal').hidden = false;
  $('ctl-5').focus();
  LX.pending.timer = setInterval(() => {
    left -= 1;
    $('ctl-count').textContent = left + 's';
    if (left <= 0) answerCtl(0);
  }, 1000);
}

async function answerCtl(minutes) {
  const p = LX.pending;
  if (!p) return;
  clearInterval(p.timer);
  LX.pending = null;
  $('ctl-modal').hidden = true;
  if (!minutes) {
    sendTo(p.peer, { t: 'ctl', state: 'denied', reason: 'recusado' });
    feed('CONTROLE', `pedido de ${p.name} recusado`);
    return;
  }
  try {
    await invoke('remote_grant', { peer: p.peer, minutes });
  } catch (e) {
    sendTo(p.peer, { t: 'ctl', state: 'denied', reason: String(e) });
    return toast('Não deu pra liberar: ' + e);
  }
  LX.ctlPeer = p.peer;
  LX.ctlUntil = Date.now() + minutes * 60000;
  sendTo(p.peer, { t: 'ctl', state: 'granted', minutes });
  fxEmit({ t: 'banner', text: `${p.name.toUpperCase()} ESTÁ NO CONTROLE`, sub: 'CTRL+ALT+X CORTA' });
  feed('CONTROLE', `${p.name} no controle por ${minutes} min`);
  $('ctl-who').textContent = p.name.toUpperCase();
  $('ctl-state').hidden = false;
  clearInterval(LX.timer);
  LX.timer = setInterval(() => {
    const s = Math.max(0, Math.round((LX.ctlUntil - Date.now()) / 1000));
    $('ctl-left').textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }, 500);
}
$('ctl-5').addEventListener('click', () => answerCtl(5));
$('ctl-15').addEventListener('click', () => answerCtl(15));
$('ctl-no').addEventListener('click', () => answerCtl(0));
$('btn-ctl-cut').addEventListener('click', () => invoke('remote_revoke', { reason: 'quem transmite cortou' }).catch(() => {}));

listen('remote-ended', ({ payload }) => {
  const { peer, reason } = payload || {};
  if (peer) sendTo(peer, { t: 'ctl', state: 'revoked', reason });
  if (LX.ctlPeer === peer || !peer) {
    LX.ctlPeer = null;
    clearInterval(LX.timer);
    $('ctl-state').hidden = true;
    fxEmit({ t: 'banner', text: '' });
    feed('CONTROLE', `encerrado: ${reason || ''}`);
  }
});

initLogin();
