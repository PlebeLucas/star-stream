// STAR // STREAM: configuração automática.
//
//   node setup/setup.mjs            configura tudo (primeira vez ou de novo)
//   node setup/setup.mjs --publicar publica uma versão nova (os apps atualizam sozinhos)
//
// O que precisa de você: criar o app no Discord e colar 3 coisas.
// O resto (Cloudflare, GitHub, chaves, build, aviso no Discord) é automático.

import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { createInterface } from 'node:readline/promises';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SIGNAL = join(ROOT, 'signal');
const APP = join(ROOT, 'app');
const CONF_FILE = join(ROOT, 'setup', '.config.json'); // fica só no seu PC (no .gitignore)
const KEY_DIR = join(ROOT, 'setup', '.keys');
const WIN = process.platform === 'win32';
const REPO_NAME = 'star-stream';

// ---------------- aparência ----------------

const R = '\x1b[38;2;255;42;61m', D = '\x1b[38;2;133;140;150m', B = '\x1b[1m', X = '\x1b[0m', G = '\x1b[38;2;61;220;132m';
const step = (n, total, t) => console.log(`\n${R}${B}[${n}/${total}]${X} ${B}${t}${X}`);
const info = (t) => console.log(`${D}    ${t}${X}`);
const ok = (t) => console.log(`${G}    ✓ ${t}${X}`);
const fail = (t) => {
  console.log(`\n${R}${B}    ✗ ${t}${X}\n`);
  process.exit(1);
};

// No terminal, abre e FECHA a leitura a cada pergunta: assim o teclado fica
// livre pros programas que perguntam coisas sozinhos (GitHub, Cloudflare).
// Com entrada redirecionada (testes), usa um leitor único pra não perder linhas.
const TTY = !!process.stdin.isTTY;
let shared = null;
async function readLine(prompt) {
  if (TTY) {
    const r = createInterface({ input: process.stdin, output: process.stdout });
    try {
      return await r.question(prompt);
    } finally {
      r.close();
      process.stdin.pause();
    }
  }
  if (!shared) {
    const r = createInterface({ input: process.stdin, terminal: false });
    shared = { r, it: r[Symbol.asyncIterator]() };
  }
  process.stdout.write(prompt);
  const { value, done } = await shared.it.next();
  if (done) fail('Entrada encerrada.');
  return value;
}
const ask = async (q, def) => {
  const shown = def ? ` ${D}[${def.length > 24 ? def.slice(0, 20) + '…' : def}]` : '';
  const a = (await readLine(`${B}    ${q}${shown}${X} › `)).trim();
  return a || def || '';
};
const pause = (t = 'Aperte ENTER quando terminar') => readLine(`${D}    ${t}…${X}`);

// ---------------- comandos ----------------

const q = (a) => (WIN && /[\s"&|<>^]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a);

/** Roda um comando. `capture` devolve a saída; `input` manda texto pro stdin. */
function run(cmd, args, { cwd = ROOT, capture = false, input, quiet = false, allowFail = false } = {}) {
  return new Promise((res, rej) => {
    const p = spawn(WIN ? [cmd, ...args.map(q)].join(' ') : cmd, WIN ? [] : args, {
      cwd,
      shell: WIN,
      stdio: [input !== undefined ? 'pipe' : 'inherit', capture || quiet ? 'pipe' : 'inherit', quiet ? 'pipe' : 'inherit'],
    });
    let out = '';
    if (p.stdout) p.stdout.on('data', (d) => {
      out += d;
      if (capture && !quiet) process.stdout.write(d);
    });
    if (p.stderr) p.stderr.on('data', (d) => (out += d));
    if (input !== undefined) {
      p.stdin.write(input);
      p.stdin.end();
    }
    p.on('error', (e) => (allowFail ? res({ code: 1, out: String(e) }) : rej(e)));
    p.on('close', (code) => {
      if (code !== 0 && !allowFail) rej(new Error(`${cmd} ${args.join(' ')} saiu com código ${code}\n${out.slice(-800)}`));
      else res({ code, out });
    });
  });
}

const has = async (cmd) => (await run(cmd, ['--version'], { quiet: true, allowFail: true })).code === 0;
const wrangler = (args, opts = {}) => run('npx', ['--yes', 'wrangler', ...args], { cwd: SIGNAL, ...opts });

function openUrl(url) {
  const [c, a] = WIN ? ['explorer.exe', [url]] : process.platform === 'darwin' ? ['open', [url]] : ['xdg-open', [url]];
  try {
    spawn(c, a, { stdio: 'ignore', detached: true }).on('error', () => {}).unref();
  } catch {}
  info(`(se não abriu: ${url})`);
}

async function copy(text) {
  const c = WIN ? 'clip' : process.platform === 'darwin' ? 'pbcopy' : 'xclip';
  const a = WIN || process.platform === 'darwin' ? [] : ['-selection', 'clipboard'];
  return (await run(c, a, { input: text, quiet: true, allowFail: true })).code === 0;
}

const loadConf = () => (existsSync(CONF_FILE) ? JSON.parse(readFileSync(CONF_FILE, 'utf8')) : {});
const saveConf = (c) => writeFileSync(CONF_FILE, JSON.stringify(c, null, 2));

function setTomlVar(name, value) {
  const p = join(SIGNAL, 'wrangler.toml');
  const s = readFileSync(p, 'utf8').replace(new RegExp(`^${name} = ".*"$`, 'm'), `${name} = "${value}"`);
  writeFileSync(p, s);
}

const readJson = (p) => JSON.parse(readFileSync(p, 'utf8'));
const writeJson = (p, o) => writeFileSync(p, JSON.stringify(o, null, 2) + '\n');

async function postWebhook(url, content) {
  if (!url) return;
  await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: 'STAR // STREAM', content, allowed_mentions: { parse: [] } }),
  }).catch(() => {});
}

// ---------------- GitHub: esperar o build ----------------

async function waitForBuild(repo) {
  const sha = (await run('git', ['rev-parse', 'HEAD'], { capture: true, quiet: true })).out.trim();
  info('Esperando o GitHub começar o build…');
  let id;
  for (let i = 0; i < 60 && !id; i++) {
    await new Promise((r) => setTimeout(r, 5000));
    const { out } = await run('gh', ['run', 'list', '-R', repo, '--workflow', 'build.yml', '--limit', '5', '--json', 'databaseId,headSha'], {
      capture: true,
      quiet: true,
      allowFail: true,
    });
    try {
      id = JSON.parse(out).find((r) => r.headSha === sha)?.databaseId;
    } catch {}
  }
  if (!id) fail('O build não começou. Veja a aba Actions do repositório.');
  info('Build rodando (uns 15 minutos na primeira vez; pode deixar a janela aberta e ir fazer outra coisa).');
  const r = await run('gh', ['run', 'watch', String(id), '-R', repo, '--exit-status', '--interval', '20'], { allowFail: true });
  if (r.code !== 0) fail(`O build falhou. Detalhes: https://github.com/${repo}/actions/runs/${id}`);
  ok('Instalador gerado e publicado');
}

// ================= PUBLICAR VERSÃO NOVA =================

async function publish() {
  const conf = loadConf();
  if (!conf.repo) fail('Rode a configuração primeiro (CONFIGURAR.cmd).');
  const tauriConf = join(APP, 'src-tauri', 'tauri.conf.json');
  const c = readJson(tauriConf);
  const [a, b, p] = c.version.split('.').map(Number);
  const next = `${a}.${b}.${p + 1}`;
  console.log(`\n${R}${B}STAR // STREAM${X} — publicando versão ${B}${next}${X} (era ${c.version})`);
  info('Atualizando o servidor de encontro…');
  await run('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: SIGNAL, quiet: true });
  await wrangler(['deploy'], { quiet: true });
  ok('Servidor de encontro atualizado');
  c.version = next;
  // Libera todas as placas de rede (inclui Radmin VPN) pro WebRTC de quem assiste
  c.app.windows[0].additionalBrowserArgs =
    '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,WebRtcHideLocalIpsWithMdns ' +
    '--force-webrtc-ip-handling-policy=default --autoplay-policy=no-user-gesture-required';
  writeJson(tauriConf, c);
  const cargo = join(APP, 'src-tauri', 'Cargo.toml');
  writeFileSync(cargo, readFileSync(cargo, 'utf8').replace(/^version = ".*"$/m, `version = "${next}"`));
  await run('git', ['add', '-A']);
  await run('git', ['commit', '-q', '-m', `Versão ${next}`]);
  await run('git', ['pull', '-q', '--rebase', '--autostash', 'origin', 'main'], { allowFail: true });
  await run('git', ['push', '-q']);
  await waitForBuild(conf.repo);
  await postWebhook(conf.webhook, `🔄 **STAR // STREAM ${next}** saiu. Quem já tem o app recebe sozinho na próxima vez que abrir.`);
  ok(`Versão ${next} no ar. Os apps dos seus amigos atualizam sozinhos.`);
}

// ================= CONFIGURAÇÃO =================

async function setup() {
  const conf = loadConf();
  const TOTAL = 6;
  console.log(`\n${R}${B}  STAR // STREAM${X}${D} · configuração automática${X}`);
  console.log(`${D}  Seus amigos não precisam fazer nada disso: só instalar o app no final.${X}`);

  // ---------- 1. ferramentas ----------
  step(1, TOTAL, 'Conferindo ferramentas');
  for (const [cmd, name] of [['git', 'Git'], ['gh', 'GitHub CLI'], ['npx', 'Node.js']]) {
    if (!(await has(cmd))) fail(`${name} não encontrado. Rode o CONFIGURAR.cmd (ele instala sozinho).`);
  }
  ok('Git, GitHub CLI e Node.js prontos');

  // ---------- 2. Discord ----------
  step(2, TOTAL, 'Discord (a única parte manual)');
  if (!conf.clientId) {
    info('Vou abrir o portal de desenvolvedor do Discord. Lá:');
    info('  1. Clique em "New Application", dê o nome "STAR Stream" e crie.');
    info('  2. No menu da esquerda, abra "OAuth2".');
    openUrl('https://discord.com/developers/applications');
  }
  conf.clientId = await ask('Cole o CLIENT ID:', conf.clientId);
  if (!/^\d{15,22}$/.test(conf.clientId)) fail('Client ID inválido (são só números).');
  if (!conf.clientSecret) info('Na mesma página, clique em "Reset Secret", confirme e copie.');
  conf.clientSecret = await ask('Cole o CLIENT SECRET:', conf.clientSecret);
  if (conf.clientSecret.length < 20) fail('Client Secret parece incompleto.');

  if (!conf.webhook) {
    info('Agora o canal onde o app avisa quando alguém entra ao vivo:');
    info('  No Discord, passe o mouse no canal → engrenagem → Integrações → Webhooks →');
    info('  Novo webhook → Copiar URL do webhook.');
  }
  conf.webhook = await ask('Cole a URL do WEBHOOK:', conf.webhook);
  const wh = await fetch(conf.webhook).then((r) => (r.ok ? r.json() : null)).catch(() => null);
  if (!wh || !wh.guild_id) fail('Não consegui ler esse webhook. Confere se copiou a URL inteira.');
  conf.guildId = wh.guild_id;
  ok(`Servidor do Discord identificado (o acesso fica liberado só pra quem é membro dele)`);
  saveConf(conf);

  // ---------- 3. contas ----------
  step(3, TOTAL, 'Contas gratuitas (Cloudflare e GitHub)');
  if ((await wrangler(['whoami'], { capture: true, quiet: true, allowFail: true })).out.match(/not authenticated|You are not/i)) {
    info('Vai abrir o navegador pra entrar (ou criar conta grátis) na Cloudflare.');
    await wrangler(['login']);
  }
  ok('Cloudflare conectada');
  if ((await run('gh', ['auth', 'status'], { quiet: true, allowFail: true })).code !== 0) {
    info('Agora o GitHub (entre ou crie uma conta grátis). Escolha "Login with a web browser".');
    await run('gh', ['auth', 'login', '--web', '--git-protocol', 'https', '--hostname', 'github.com']);
  }
  const owner = (await run('gh', ['api', 'user', '-q', '.login'], { capture: true, quiet: true })).out.trim();
  conf.repo = `${owner}/${REPO_NAME}`;
  ok(`GitHub conectado como ${owner}`);
  saveConf(conf);

  // ---------- 4. servidor de encontro ----------
  step(4, TOTAL, 'Publicando o servidor de encontro');
  setTomlVar('DISCORD_CLIENT_ID', conf.clientId);
  setTomlVar('GUILD_ID', conf.guildId);
  setTomlVar('GITHUB_REPO', conf.repo);
  await run('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: SIGNAL });
  let dep = await wrangler(['deploy'], { capture: true, allowFail: true });
  if (dep.code !== 0 && /workers\.dev subdomain/i.test(dep.out)) {
    // Conta nova na Cloudflare: precisa escolher o subdomínio uma vez, pelo site
    const onboarding = (dep.out.match(/https:\/\/dash\.cloudflare\.com\/\S+onboarding/) || [])[0] || 'https://dash.cloudflare.com/?to=/:account/workers/onboarding';
    console.log('');
    info('A Cloudflare precisa que você escolha um nome de subdomínio (uma vez só).');
    info('Vou abrir a página: escolha um nome (ex.: seu apelido) e confirme.');
    openUrl(onboarding);
    await pause();
    dep = await wrangler(['deploy'], { capture: true, allowFail: true });
  }
  if (dep.code !== 0) fail('A publicação na Cloudflare falhou (veja a mensagem acima).');
  const url = (dep.out.match(/https:\/\/[a-z0-9.-]+\.workers\.dev/i) || [])[0];
  if (!url) fail('Não achei o endereço do servidor na saída acima.');
  conf.signalUrl = url;
  conf.sessionSecret ||= randomBytes(32).toString('hex'); // mantém o mesmo: ninguém precisa logar de novo
  for (const [k, v] of [
    ['DISCORD_CLIENT_SECRET', conf.clientSecret],
    ['SESSION_SECRET', conf.sessionSecret],
    ['DISCORD_WEBHOOK_URL', conf.webhook],
  ]) {
    await wrangler(['secret', 'put', k], { input: v, quiet: true });
  }
  ok(`Servidor no ar: ${url}`);
  saveConf(conf);

  const redirect = `${url}/callback`;
  if (conf.redirectDone !== redirect) {
    const copied = await copy(redirect);
    info('Último passo no Discord. Vou abrir a página OAuth2 do seu app:');
    info(`  Em "Redirects" clique "Add Redirect", cole ${copied ? '(Ctrl+V, já copiei pra você)' : 'o endereço abaixo'} e clique "Save Changes".`);
    info(`  ${B}${redirect}${X}`);
    openUrl(`https://discord.com/developers/applications/${conf.clientId}/oauth2`);
    await pause();
    conf.redirectDone = redirect;
    saveConf(conf);
  }
  ok('Login com Discord configurado');

  // ---------- 5. chave de atualização ----------
  step(5, TOTAL, 'Preparando atualização automática');
  mkdirSync(KEY_DIR, { recursive: true });
  const keyFile = join(KEY_DIR, 'updater.key');
  if (!existsSync(keyFile)) {
    conf.keyPassword = randomBytes(24).toString('hex');
    await run('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: APP });
    await run('npx', ['tauri', 'signer', 'generate', '-w', keyFile, '-p', conf.keyPassword, '--ci', '-f'], { cwd: APP, quiet: true });
    saveConf(conf);
  }
  const tauriConfPath = join(APP, 'src-tauri', 'tauri.conf.json');
  const tc = readJson(tauriConfPath);
  tc.plugins.updater.pubkey = readFileSync(keyFile + '.pub', 'utf8').trim();
  tc.plugins.updater.endpoints = [`https://github.com/${conf.repo}/releases/latest/download/latest.json`];
  writeJson(tauriConfPath, tc);
  ok('Chave criada (fica só no seu PC, em setup/.keys — não apague)');

  // ---------- 6. GitHub: repositório + build ----------
  step(6, TOTAL, 'Gerando o instalador no GitHub');
  if (!existsSync(join(ROOT, '.git'))) {
    await run('git', ['init', '-q']);
  }
  const name = (await run('git', ['config', 'user.name'], { capture: true, quiet: true, allowFail: true })).out.trim();
  if (!name) {
    await run('git', ['config', 'user.name', owner]);
    await run('git', ['config', 'user.email', `${owner}@users.noreply.github.com`]);
  }
  const hasCommit = (await run('git', ['rev-parse', '--verify', 'HEAD'], { quiet: true, allowFail: true })).code === 0;
  if (hasCommit) await run('git', ['branch', '-M', 'main']);
  else await run('git', ['symbolic-ref', 'HEAD', 'refs/heads/main']);
  await run('git', ['add', '-A']);
  await run('git', ['commit', '-q', '-m', 'STAR // STREAM: configuração'], { allowFail: true });

  const exists = (await run('gh', ['repo', 'view', conf.repo], { quiet: true, allowFail: true })).code === 0;
  if (!exists) {
    // Público pra os amigos conseguirem baixar e o app se atualizar.
    // Não tem nenhum segredo no código: eles ficam na Cloudflare e no GitHub Secrets.
    await run('gh', ['repo', 'create', REPO_NAME, '--public', '--source', '.', '--remote', 'origin', '--description', 'STAR // STREAM']);
  } else if ((await run('git', ['remote', 'get-url', 'origin'], { quiet: true, allowFail: true })).code !== 0) {
    await run('git', ['remote', 'add', 'origin', `https://github.com/${conf.repo}.git`]);
  }
  await run('gh', ['variable', 'set', 'STAR_SIGNAL_URL', '-R', conf.repo, '--body', conf.signalUrl], { quiet: true });
  await run('gh', ['secret', 'set', 'TAURI_SIGNING_PRIVATE_KEY', '-R', conf.repo], { input: readFileSync(keyFile, 'utf8'), quiet: true });
  await run('gh', ['secret', 'set', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD', '-R', conf.repo, '--body', conf.keyPassword], { quiet: true });
  // Se algo foi mudado direto no site do GitHub, traz antes de enviar
  if (exists) await run('git', ['pull', '-q', '--rebase', '--autostash', 'origin', 'main'], { allowFail: true });
  await run('git', ['push', '-q', '-u', 'origin', 'main']);
  ok(`Código enviado pra github.com/${conf.repo}`);
  await waitForBuild(conf.repo);

  const download = `${conf.signalUrl}/baixar`;
  await postWebhook(
    conf.webhook,
    `📥 **STAR // STREAM** chegou!\n1. Baixe: ${download}\n2. Instale e abra\n3. Clique em **ENTRAR COM DISCORD**\n\nSe o Windows avisar "protegeu o computador", clique em *Mais informações → Executar assim mesmo*. As próximas versões chegam sozinhas.`,
  );

  console.log(`\n${R}${B}  PRONTO.${X}`);
  console.log(`  O link de download já foi postado no canal do Discord: ${B}${download}${X}`);
  console.log(`${D}  Pra publicar uma versão nova no futuro: PUBLICAR-ATUALIZACAO.cmd${X}\n`);
}

try {
  if (process.argv.includes('--publicar')) await publish();
  else await setup();
} catch (e) {
  fail(e.message || String(e));
} finally {
  if (shared) shared.r.close();
}
