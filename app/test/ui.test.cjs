// Teste da lógica da interface (sem Windows): simula o servidor de encontro,
// o núcleo em Rust e o WebRTC. Cenário: eu transmito e assisto um amigo
// ao mesmo tempo, e ELE também me assiste. Os sinais não podem se misturar.
//   node app/test/ui.test.cjs   (precisa do pacote jsdom)
const { JSDOM } = require('jsdom');
const fs = require('fs');
const path = require('path');
const assert = require('assert/strict');

const dir = path.join(__dirname, '..', 'src');
const html = fs.readFileSync(path.join(dir, 'index.html'), 'utf8').replace(/<script src="app.js"><\/script>/, '').replace(/<link[^>]+>/g, '');
const dom = new JSDOM(html, { runScripts: 'outside-only', url: 'http://localhost/' });
const w = dom.window;

const invokes = [];
const listeners = {};
w.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      invokes.push({ cmd, args });
      if (cmd === 'default_signal_url') return 'https://sinal.test';
      if (cmd === 'list_sources') return [{ kind: 'monitor', handle: 1, name: 'Monitor principal', detail: '2560×1440', pid: 0 }];
      if (cmd === 'encoder_info') return { id: 'nvenc', label: 'NVIDIA (NVENC)' };
      return null;
    },
  },
  event: { listen: async (name, fn) => (listeners[name] = fn) },
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
w.localStorage.setItem('token', 'tok');
w.localStorage.setItem('signalUrl', 'https://sinal.test');

w.eval(fs.readFileSync(path.join(dir, 'app.js'), 'utf8'));
const $ = (id) => w.document.getElementById(id);
const tick = () => new Promise((r) => setTimeout(r, 10));

(async () => {
  await tick();
  const ws = FakeWS.last;
  const me = { peerId: 'EU', user: { name: 'Eu', avatar: null } };
  const amigo = { peerId: 'AMIGO', user: { name: 'Amigo', avatar: null } };
  ws.recv({ t: 'hello', you: me, ice: [] });
  ws.recv({ t: 'presence', peers: [{ ...me, live: null }, { ...amigo, live: { info: 'jogo' } }] });
  assert.equal($('live-now').hidden, false, 'live do amigo aparece');

  // 1) Eu entro ao vivo
  $('btn-golive').click();
  await tick();
  assert.ok(invokes.some((i) => i.cmd === 'go_live'), 'chamou go_live');
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

  // 6) Encerro minha live: continuo assistindo
  $('btn-stop').click();
  await tick();
  assert.equal($('screen-watch').hidden, false, 'continua no vídeo do amigo');
  assert.equal($('watch-livebar').hidden, true, 'barra some');

  // 7) Saio do vídeo: volta pro início
  $('btn-leave').click();
  await tick();
  assert.equal($('screen-home').hidden, false);
  console.log('OK: transmitir e assistir ao mesmo tempo, sinais separados por papel');
  process.exit(0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
