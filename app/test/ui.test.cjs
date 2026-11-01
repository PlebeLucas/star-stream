// Teste da lógica da interface (sem Windows): simula o servidor de encontro,
// o núcleo em Rust e o WebRTC. Cenário: eu transmito e assisto um amigo
// ao mesmo tempo, e ELE também me assiste. Os sinais não podem se misturar.
//   node app/test/ui.test.cjs   (precisa do pacote jsdom)
const { JSDOM } = require('jsdom');
const fs = require('fs');
const path = require('path');
const assert = require('assert/strict');

const dir = path.join(__dirname, '..', 'src');
const html = fs.readFileSync(path.join(dir, 'index.html'), 'utf8').replace(/<script src="(app|fx).js"><\/script>/g, '').replace(/<link[^>]+>/g, '');
const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
const w = dom.window;

const invokes = [];
const fxSent = [];
const listeners = {};
w.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      invokes.push({ cmd, args });
      if (cmd === 'default_signal_url') return 'https://sinal.test';
      if (cmd === 'list_sources') return [{ kind: 'monitor', handle: 1, name: 'Monitor principal', detail: '2560×1440', pid: 0 }];
      if (cmd === 'encoder_info') return { id: 'nvenc', label: 'NVIDIA (NVENC)' };
      if (cmd === 'source_thumb') return 'data:image/jpeg;base64,AAAA';
      if (cmd === 'webcams') return ['HD Webcam'];
      if (cmd === 'webcam_thumb') return 'data:image/jpeg;base64,CAM';
      if (cmd === 'clip_save') return 'C:/Videos/STAR Stream/' + args.name + '.mp4';
      return null;
    },
  },
  event: { listen: async (name, fn) => (listeners[name] = fn), emitTo: (target, ev, payload) => fxSent.push({ target, ev, payload }) },
};
const sent = [];
class FakeWS {
  constructor() { FakeWS.last = this; this.readyState = 1; setTimeout(() => this.onopen && this.onopen(), 0); }
  send(s) { sent.push(JSON.parse(s)); }
  close() {}
  recv(o) { this.onmessage({ data: JSON.stringify(o) }); }
}
FakeWS.OPEN = 1;
w.WebSocket = FakeWS;
const pcs = [];
w.RTCPeerConnection = class {
  constructor() { pcs.push(this); this.remote = null; this.cands = []; this.connectionState = 'new'; }
  async setRemoteDescription(d) { this.remote = d; }
  async createAnswer() { return { type: 'answer', sdp: 'ANSWER' }; }
  async setLocalDescription() {}
  async addIceCandidate(c) { this.cands.push(c); }
  async getStats() { return new Map(); }
  close() {}
};
w.MediaStream = class { addTrack() {} };
w.HTMLMediaElement.prototype.play = () => Promise.resolve();
w.CSS = { escape: (x) => String(x).replace(/["\\]/g, '\\$&') };
w.localStorage.setItem('token', 'tok');
w.localStorage.setItem('signalUrl', 'https://sinal.test');

w.eval(fs.readFileSync(path.join(dir, 'fx.js'), 'utf8'));
w.eval(fs.readFileSync(path.join(dir, 'app.js'), 'utf8'));
const $ = (id) => w.document.getElementById(id);
const tick = () => new Promise((r) => setTimeout(r, 10));

(async () => {
  await tick();
  const ws = FakeWS.last;
  const me = { peerId: 'EU', user: { id: '1', name: 'Eu', avatar: null } };
  const amigo = { peerId: 'AMIGO', user: { id: '2', name: 'Amigo', avatar: null } };
  ws.recv({ t: 'hello', you: me, ice: [] });
  ws.recv({ t: 'presence', peers: [{ ...me, live: null }, { ...amigo, live: { info: 'jogo' } }] });
  assert.equal($('live-now').hidden, false, 'live do amigo aparece');

  // 1) Eu entro ao vivo
  $('btn-golive').click();
  await tick();
  assert.ok(invokes.some((i) => i.cmd === 'go_live'), 'chamou go_live');
  assert.equal(invokes.find((i) => i.cmd === 'go_live').args.config.auto_quality, true, 'ajuste automático ligado por padrão');
  listeners.hub({ payload: { type: 'quality', preset: 'leve', reason: 'teste' } });
  assert.equal($('live-q').textContent, '1080p · 30fps · 6 Mbps', 'painel mostra a qualidade nova');
  assert.equal($('screen-live').hidden, false, 'tela ao vivo');
  assert.equal($('live-others').hidden, false, 'painel ao vivo mostra outras lives');

  // 2) Assisto o amigo DURANTE a live (antes era bloqueado)
  $('live-list-2').querySelector('button').click();
  await tick();
  assert.equal($('screen-watch').hidden, false, 'tela de assistir');
  assert.equal($('watch-livebar').hidden, false, 'barra "você está ao vivo" visível');
  const join = sent.find((m) => m.t === 'signal' && m.data.kind === 'join');
  assert.deepEqual(join, { t: 'signal', to: 'AMIGO', data: { kind: 'join', role: 'v' } });

  // 3) O amigo também entra na MINHA live: os sinais dele como espectador vão pro núcleo
  ws.recv({ t: 'signal', from: 'AMIGO', fromUser: amigo.user, data: { kind: 'join', role: 'v' } });
  ws.recv({ t: 'signal', from: 'AMIGO', fromUser: amigo.user, data: { kind: 'ice', candidate: { candidate: 'c-espectador' }, role: 'v' } });
  await tick();
  const toCore = invokes.filter((i) => i.cmd === 'signal_in');
  assert.equal(toCore.length, 2, 'join + ice do amigo-espectador foram pro núcleo');
  assert.equal(toCore[1].args.data.candidate.candidate, 'c-espectador');
  assert.equal(toCore[1].args.data.role, undefined, 'papel removido antes de ir pro Rust');
  assert.equal($('watch-livebar-n').textContent, '1', 'contador de espectadores na barra');

  // 4) Sinais do amigo como TRANSMISSOR vão pro meu player, não pro núcleo
  ws.recv({ t: 'signal', from: 'AMIGO', fromUser: amigo.user, data: { kind: 'offer', sdp: 'OFFER', role: 'h' } });
  await tick();
  ws.recv({ t: 'signal', from: 'AMIGO', fromUser: amigo.user, data: { kind: 'ice', candidate: { candidate: 'c-transmissor' }, role: 'h' } });
  await tick();
  const pc = pcs[pcs.length - 1];
  assert.equal(pc.remote.sdp, 'OFFER');
  assert.deepEqual(pc.cands.map((c) => c.candidate), ['c-transmissor']);
  assert.equal(invokes.filter((i) => i.cmd === 'signal_in').length, 2, 'nada de transmissor vazou pro núcleo');
  const ans = sent.find((m) => m.data && m.data.kind === 'answer');
  assert.equal(ans.data.role, 'v');

  // 5) Sinais que o núcleo gera saem marcados como transmissor
  listeners.hub({ payload: { type: 'signal', to: 'AMIGO', data: { kind: 'offer', sdp: 'MEU-OFFER' } } });
  assert.equal(sent[sent.length - 1].data.role, 'h');

  // ===== INTERAÇÃO =====
  const J = (x) => JSON.parse(JSON.stringify(x));
  // Lado de quem assiste: canal de dados abre, pede "hello"
  const dcSent = [];
  const dc = { readyState: 'open', send: (t) => dcSent.push(JSON.parse(t)) };
  pc.ondatachannel({ channel: dc });
  dc.onopen();
  assert.equal($('ixbar').hidden, false, 'barra de interação aparece');
  assert.equal(dcSent[0].t, 'hi');
  dc.onmessage({ data: JSON.stringify({ t: 'hello', kind: 'monitor', ctl: true }) });
  assert.equal($('ix-ctl').disabled, false);
  // reação
  $('ix-reacts').querySelector('button').click();
  assert.equal(dcSent.pop().t, 'react');
  assert.ok($('player').querySelector('.fx-react'), 'reação aparece no player');
  // ping: player de 1000x562, vídeo 16:9 → clique no centro vira 0.5/0.5
  const player = $('player');
  Object.defineProperty(player, 'clientWidth', { value: 1000 });
  Object.defineProperty(player, 'clientHeight', { value: 562.5 });
  player.getBoundingClientRect = () => ({ left: 0, top: 0, width: 1000, height: 562.5 });
  Object.defineProperty($('video'), 'videoWidth', { value: 1920 });
  Object.defineProperty($('video'), 'videoHeight', { value: 1080 });
  $('ix-ping').click();
  assert.equal($('ix-ping').getAttribute('aria-pressed'), 'true');
  player.dispatchEvent(new w.MouseEvent('click', { clientX: 500, clientY: 281.25, bubbles: true }));
  const pingMsg = dcSent.pop();
  assert.equal(pingMsg.t, 'ping');
  assert.equal(pingMsg.x, 0.5);
  assert.equal(pingMsg.y, 0.5);
  // clipa
  $('ix-clip').click();
  assert.equal(dcSent.pop().t, 'clip');
  assert.equal($('ix-clip').disabled, true, 'botão de clipe espera 10 s');
  // pedir controle → liberado → teclas e mouse vão pelo canal
  $('ix-ctl').click();
  assert.equal(dcSent.pop().t, 'ctl-req');
  assert.equal($('ix-ctl-txt').textContent, 'AGUARDANDO…');
  dc.onmessage({ data: JSON.stringify({ t: 'ctl', state: 'granted', minutes: 5 }) });
  assert.ok(player.classList.contains('controlling'));
  assert.equal($('ix-ping').getAttribute('aria-pressed'), 'false', 'ping desliga ao pegar o controle');
  w.dispatchEvent(new w.KeyboardEvent('keydown', { keyCode: 87, code: 'KeyW', bubbles: true }));
  assert.equal(JSON.stringify(dcSent.pop()), JSON.stringify({ t: 'in', k: 'kd', vk: 87 }));
  player.dispatchEvent(new w.MouseEvent('pointerdown', { clientX: 250, clientY: 140.625, button: 0, bubbles: true }));
  const down = dcSent.pop();
  assert.equal(down.k, 'd');
  assert.equal(down.x, 0.25);
  // Ctrl+Alt+X solta: tecla W segurada é solta no PC do outro
  w.dispatchEvent(new w.KeyboardEvent('keydown', { keyCode: 88, code: 'KeyX', ctrlKey: true, altKey: true, bubbles: true }));
  const tail = dcSent.slice(-2).map((m) => m.t + (m.k || ''));
  assert.deepEqual(J(tail), ['inku', 'ctl-release']);
  assert.ok(!player.classList.contains('controlling'));

  // Lado de quem transmite (eu estou ao vivo; o amigo me assiste)
  const ixStart = invokes.length;
  const iv = () => invokes.slice(ixStart);
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'hi' } } });
  await tick();
  const hello = iv().find((i) => i.cmd === 'hub_send');
  assert.equal(hello.args.msg.t, 'hello');
  assert.equal(hello.args.msg.ctl, true);
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'react', e: 'fogo' } } });
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'ping', x: 0.2, y: 0.9 } } });
  await tick();
  const bc = iv().filter((i) => i.cmd === 'hub_broadcast').map((i) => i.args.msg);
  assert.equal(bc[0].t, 'react');
  assert.equal(bc[0].name, 'Amigo');
  assert.equal(bc[1].t, 'ping');
  assert.equal(bc[1].by, 'AMIGO');
  assert.ok(fxSent.some((f) => f.target === 'overlay' && f.payload.t === 'ping' && f.payload.x === 0.2), 'ping vai pra camada da tela');
  assert.match($('ix-feed').textContent, /PING/);
  // clipe pedido pelo amigo
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'clip' } } });
  await tick();
  assert.ok(iv().some((i) => i.cmd === 'clip_save' && /^clip_\d{4}-\d\d-\d\d_\d\d-\d\d-\d\d_Amigo$/.test(i.args.name)), 'salvou clipe com nome certo');
  assert.ok(iv().some((i) => i.cmd === 'hub_broadcast' && i.args.msg.t === 'clipped'));
  // segundo clipe logo em seguida é recusado
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'clip' } } });
  await tick();
  assert.ok(iv().some((i) => i.cmd === 'hub_send' && i.args.msg.t === 'clip-fail'));
  // pedido de controle → janela de aceite → 5 min
  listeners.hub({ payload: { type: 'interaction', peer: 'AMIGO', msg: { t: 'ctl-req' } } });
  await tick();
  assert.equal($('ctl-modal').hidden, false, 'janela de pedido aparece');
  assert.equal($('ctl-name').textContent, 'Amigo');
  $('ctl-5').click();
  await tick();
  assert.ok(iv().some((i) => i.cmd === 'remote_grant' && i.args.peer === 'AMIGO' && i.args.minutes === 5));
  assert.ok(iv().some((i) => i.cmd === 'hub_send' && i.args.msg.state === 'granted'));
  assert.equal($('ctl-state').hidden, false);
  assert.ok(fxSent.some((f) => f.payload.t === 'banner' && /AMIGO ESTÁ NO CONTROLE/.test(f.payload.text)));
  // outro pedido enquanto alguém controla: recusado na hora
  listeners.hub({ payload: { type: 'interaction', peer: 'OUTRO', msg: { t: 'ctl-req' } } });
  await tick();
  assert.ok(iv().some((i) => i.cmd === 'hub_send' && i.args.peer === 'OUTRO' && i.args.msg.state === 'denied'));
  // Ctrl+Alt+X no Rust → evento → avisa o amigo e some o banner
  listeners['remote-ended']({ payload: { peer: 'AMIGO', reason: 'cortado com Ctrl+Alt+X' } });
  await tick();
  assert.ok(iv().some((i) => i.cmd === 'hub_send' && i.args.msg.state === 'revoked'));
  assert.equal($('ctl-state').hidden, true);
  assert.ok(fxSent.some((f) => f.payload.t === 'banner' && f.payload.text === ''));

  // 6) Encerro minha live: continuo assistindo
  $('btn-stop').click();
  await tick();
  assert.equal($('screen-watch').hidden, false, 'continua no vídeo do amigo');
  assert.equal($('watch-livebar').hidden, true, 'barra some');

  // 7) Saio do vídeo: volta pro início
  $('btn-leave').click();
  await tick();
  assert.equal($('screen-home').hidden, false);

  // 8) Prévias: miniatura no cartão e prévia grande no resumo
  await tick();
  assert.ok(invokes.some((i) => i.cmd === 'source_thumb' && i.args.w === 320), 'pediu miniatura');
  assert.ok(invokes.some((i) => i.cmd === 'source_thumb' && i.args.w === 640), 'pediu prévia grande');
  assert.equal(w.document.querySelector('#sources img').getAttribute('src'), 'data:image/jpeg;base64,AAAA');
  assert.equal($('preview-img').hidden, false);

  // 8b) Webcam aparece na lista; escolher ela tira foto e manda nome no go_live
  const camBtn = [...w.document.querySelectorAll('#sources .src')].find((b) => b.textContent.includes('WEBCAM'));
  assert.ok(camBtn, 'webcam na lista');
  assert.ok(!invokes.some((i) => i.cmd === 'webcam_thumb'), 'não liga a câmera sozinho');
  camBtn.click();
  await tick();
  assert.ok(invokes.some((i) => i.cmd === 'webcam_thumb' && i.args.name === 'HD Webcam'), 'foto da câmera ao escolher');
  assert.equal($('preview-img').getAttribute('src'), 'data:image/jpeg;base64,CAM');

  // 9) Link do aviso no Discord abre direto a live do amigo
  const joinsBefore = sent.filter((m) => m.t === 'signal' && m.data.kind === 'join').length;
  listeners['open-url']({ payload: 'starstream://assistir/2' });
  await tick();
  assert.equal($('screen-watch').hidden, false, 'link abriu a tela de assistir');
  assert.equal(sent.filter((m) => m.t === 'signal' && m.data.kind === 'join').length, joinsBefore + 1);
  assert.equal(sent[sent.length - 1].to, 'AMIGO');
  // 10) Transmitir a webcam: manda o nome da câmera
  $('btn-leave').click();
  await tick();
  const camBtn2 = [...w.document.querySelectorAll('#sources .src')].find((b) => b.textContent.includes('WEBCAM'));
  camBtn2.click();
  await tick();
  $('btn-golive').click();
  await tick();
  const lastLive = invokes.filter((i) => i.cmd === 'go_live').pop();
  assert.equal(JSON.stringify(lastLive.args.config.source), JSON.stringify({ kind: "webcam", name: "HD Webcam" }));
  console.log('OK: transmitir e assistir juntos, prévias das telas e link do aviso abrindo a live');
  process.exit(0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
