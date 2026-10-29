// Teste da sala: dois "amigos" conectam, um entra ao vivo, trocam sinais.
//   node test/room.test.mjs http://127.0.0.1:8787 segredo-de-teste
import assert from 'node:assert/strict';
import { webcrypto as crypto } from 'node:crypto';

const [base = 'http://127.0.0.1:8787', secret = 'segredo-de-teste'] = process.argv.slice(2);
const enc = new TextEncoder();
const b64url = (b) => Buffer.from(b).toString('base64url');

async function token(payload) {
  const body = b64url(enc.encode(JSON.stringify(payload)));
  const key = await crypto.subtle.importKey('raw', enc.encode(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return body + '.' + b64url(await crypto.subtle.sign('HMAC', key, enc.encode(body)));
}

function client(tok) {
  const ws = new WebSocket(base.replace(/^http/, 'ws') + '/ws?token=' + encodeURIComponent(tok));
  const inbox = [];
  const waiters = [];
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    const i = waiters.findIndex((w) => w.pred(m));
    if (i >= 0) waiters.splice(i, 1)[0].res(m);
    else inbox.push(m);
  };
  const next = (pred, ms = 3000) =>
    new Promise((res, rej) => {
      const i = inbox.findIndex(pred);
      if (i >= 0) return res(inbox.splice(i, 1)[0]);
      waiters.push({ pred, res });
      setTimeout(() => rej(new Error('tempo esgotado esperando mensagem')), ms);
    });
  return { ws, next, send: (o) => ws.send(JSON.stringify(o)) };
}

const exp = Math.floor(Date.now() / 1000) + 3600;
const a = client(await token({ id: '1', name: 'Transmissor', avatar: null, exp }));
const helloA = await a.next((m) => m.t === 'hello');
assert.equal(helloA.you.user.name, 'Transmissor');
assert.ok(Array.isArray(helloA.ice) && helloA.ice.length > 0);

const b = client(await token({ id: '2', name: 'Amigo', avatar: null, exp }));
const helloB = await b.next((m) => m.t === 'hello');
await a.next((m) => m.t === 'presence' && m.peers.length === 2);

a.send({ t: 'live', on: true, info: 'Monitor principal' });
const pres = await b.next((m) => m.t === 'presence' && m.peers.some((p) => p.live));
assert.equal(pres.peers.find((p) => p.live).user.name, 'Transmissor');

b.send({ t: 'signal', to: helloA.you.peerId, data: { kind: 'join' } });
const sig = await a.next((m) => m.t === 'signal');
assert.equal(sig.from, helloB.you.peerId);
assert.equal(sig.fromUser.name, 'Amigo');
assert.deepEqual(sig.data, { kind: 'join' });

a.send({ t: 'signal', to: helloB.you.peerId, data: { kind: 'offer', sdp: 'v=0' } });
assert.equal((await b.next((m) => m.t === 'signal')).data.kind, 'offer');

b.ws.close();
const gone = await a.next((m) => m.t === 'gone');
assert.equal(gone.peerId, helloB.you.peerId);

// Token falsificado é recusado (a conexão nunca abre)
async function refused(tok) {
  return new Promise((res) => {
    const ws = new WebSocket(base.replace(/^http/, 'ws') + '/ws?token=' + encodeURIComponent(tok));
    let opened = false;
    ws.onopen = () => { opened = true; ws.close(); };
    ws.onclose = () => res(!opened);
    ws.onerror = () => res(!opened);
    setTimeout(() => res(!opened), 2000);
  });
}
assert.equal(await refused('xxx.yyy'), true);
assert.equal(await refused((await token({ id: '3', exp })).replace(/.$/, 'A')), true);
assert.equal(await refused(await token({ id: '4', exp: 1 })), true); // expirado

a.ws.close();
console.log('OK: login, sala, ao vivo, troca de sinais e saída funcionando');
process.exit(0);
