# STAR // STREAM

Transmissão de tela privada da STAR Corp. Só entra quem é membro do servidor do Discord de vocês.

- **Vídeo pela placa de vídeo:** captura direto na GPU (sem a borda amarela) e codifica no encoder de hardware (NVIDIA, AMD ou Intel, detectado sozinho). Quase não pesa no PC.
- **Bitrate alto:** até 1440p a 60 fps e 15 Mbps.
- **Som sem o Discord:** captura cada programa com som **menos o Discord e o próprio STAR Stream**, então ninguém se ouve na transmissão. O volume da transmissão é separado do seu fone.
- **Assistir e transmitir ao mesmo tempo.**
- **Sem servidor ligado:** o vídeo vai direto do seu PC pro PC dos amigos.
- **Atualiza sozinho:** ninguém precisa reinstalar quando sai versão nova.

---

## Pros amigos (1 minuto)

1. Clique no link de download que aparece no canal do Discord.
2. Instale e abra o **STAR Stream**.
3. Clique em **ENTRAR COM DISCORD**.

Pronto. Pra assistir alguém, clique em **ASSISTIR**. Pra transmitir, escolha a tela e clique em **INICIAR TRANSMISSÃO**. Dá pra fazer os dois ao mesmo tempo: assistir um amigo enquanto está ao vivo.

**Interação** (barra embaixo do vídeo de quem assiste):
- **Reações:** FOGO, KKKKK, GG, SUSTO, BRABO. Aparecem pra quem transmite e pra todo mundo assistindo.
- **PING** (ou tecla **P**): clique no vídeo e aparece um marcador naquele ponto da tela de quem transmite.
- **CLIPA ISSO:** salva os últimos 30 s no PC de quem transmite, em `Vídeos\STAR Stream`.
- **PEDIR CONTROLE:** quem transmite aceita (5 ou 15 min) e você usa o mouse e o teclado dele dentro da área transmitida. **Ctrl+Alt+X** corta na hora, dos dois lados.

> Dica: se você transmite o **monitor inteiro** e abre a live de um amigo nesse mesmo monitor, seus espectadores veem a live dele dentro da sua. Pra evitar, transmita só a janela do jogo ou deixe o STAR Stream no outro monitor. O **som** da live do amigo nunca vai pra sua: o app exclui o próprio som automaticamente.

> Na primeira instalação o Windows pode mostrar "O Windows protegeu o computador". É porque o app não tem certificado pago. Clique em **Mais informações → Executar assim mesmo**.

---

## Pra você, que organiza (uma vez, ~5 minutos de atenção)

1. Descompacte esta pasta onde quiser.
2. Dê dois cliques em **`CONFIGURAR.cmd`**.

Ele instala o que faltar e faz tudo sozinho. Você só vai:

- **Criar o app no Discord** (ele abre a página): "New Application" → nome "STAR Stream" → colar o **Client ID** e o **Client Secret** da aba OAuth2.
- **Colar a URL de um webhook** do canal onde o app vai avisar (Editar canal → Integrações → Webhooks → Novo webhook → Copiar URL). Ele descobre o servidor do Discord sozinho por ela.
- **Entrar na Cloudflare e no GitHub** pelo navegador (contas grátis; dá pra criar na hora).
- **Colar um endereço no Discord** (ele já copia pra você e abre a página certa).

Depois disso ele publica o servidor, gera o instalador no GitHub (uns 15 minutos, sozinho) e **posta o link de download no canal**. Seus amigos já podem instalar.

**Pra publicar uma versão nova:** dois cliques em **`PUBLICAR-ATUALIZACAO.cmd`**. Os apps de todo mundo se atualizam sozinhos.

**Importante:**
- **Não apague** a pasta `setup/.keys`. É a chave que assina as atualizações; sem ela, os apps instalados param de aceitar versões novas.
- O repositório no GitHub fica **público** pros amigos conseguirem baixar. Não tem segredo nenhum no código: as senhas ficam na Cloudflare e nos Secrets do GitHub, e a pasta `setup/` com a sua configuração nunca sobe.

---

## Como funciona

```
 Seu PC (transmitindo)                                   PC do amigo
 ┌─────────────────────────────┐                        ┌────────────────┐
 │ FFmpeg: captura GPU + NVENC │──vídeo────────────┐    │                │
 │ WASAPI: som do PC − Discord │──áudio (Opus)──┐  │    │  STAR Stream   │
 │ Mini servidor WebRTC ───────┼────────────────┴──┴──► │  (assistindo)  │
 └──────────────┬──────────────┘   direto, PC → PC     └───────┬────────┘
                │                                              │
                └──────── login + "apresentação" ──────────────┘
                     Servidor de encontro (Cloudflare, grátis)
                     · login com Discord, confere se é do servidor
                     · avisa no canal quando alguém entra ao vivo
```

| Pasta | O que é |
| --- | --- |
| `core/` | Núcleo em Rust: captura, encoder, áudio sem Discord, envio WebRTC |
| `app/` | O app (Tauri): a interface STAR e a ponte com o núcleo |
| `signal/` | Servidor de encontro (Cloudflare Worker) |
| `.github/workflows/` | Gera o instalador `.exe` automaticamente |

---

## Detalhes

### Qualidade

| Preset | Saída | Upload por amigo |
| --- | --- | --- |
| Leve | até 1080p · 30 fps | ~6 Mbps |
| Equilibrada | até 1080p · 60 fps | ~10 Mbps |
| Máxima | até 1440p · 60 fps | ~15 Mbps |

O vídeo vai direto do seu PC pra cada amigo, então o **upload multiplica** pelo número de pessoas assistindo. Ex.: Máxima com 4 amigos ≈ 60 Mbps de upload.

### Requisitos

- Windows 10 21H2 (build 20348) ou Windows 11. É a versão que permite capturar o som "menos o Discord".
- Placa de vídeo NVIDIA, AMD ou Intel com encoder H.264 (praticamente qualquer uma dos últimos 8 anos). Sem ela o app usa o processador, mais pesado.

---

## Se algo der errado

**"Não consegui conectar no PC de quem transmite"**: provavelmente CGNAT na operadora de um dos dois. Primeiro confira se o IPv6 está ativo no roteador (a maioria das operadoras de fibra já entrega). Se não resolver, ative o relay reserva (TURN) da Cloudflare: no painel, **Realtime → TURN Server → Create**, e copie o **Turn Token ID** e o **API Token**. Depois, na pasta `signal`:

```bash
npx wrangler secret put TURN_KEY_ID
npx wrangler secret put TURN_KEY_API_TOKEN
npx wrangler deploy
```

O relay só é usado quando a conexão direta falha.

**A janela não aparece na lista:** janelas minimizadas não podem ser capturadas. Restaure a janela e clique em **ATUALIZAR**.

**"Transmitindo sem som"**: o Windows é anterior ao build 20348, ou o programa escolhido em "Só um app" fechou.

---

## Estado desta versão (v0.1)

**Testado aqui:**
- O núcleo envia vídeo H.264 e áudio Opus de ponta a ponta por WebRTC (teste `cargo run --example loopback` no `core/`).
- O servidor de encontro: login, sala, ao vivo, troca de sinais, saída e recusa de token falso (`node signal/test/room.test.mjs`).
- Todo o código de Windows (captura de som, lista de janelas, app, atualização automática) compila pra Windows.
- A interface transmitindo e assistindo ao mesmo tempo, com dois amigos se assistindo um ao outro (`node app/test/ui.test.cjs`, precisa do pacote `jsdom`).
- O mixer de som (testes no `core/`).
- O `CONFIGURAR.cmd`/`PUBLICAR-ATUALIZACAO.cmd`, simulando Cloudflare, GitHub e Discord: preenche tudo certo e não sobe nenhum segredo pro repositório.

**Ainda não testado num PC Windows de verdade** (ponto de partida pro primeiro teste):
- Captura pela GPU com o FFmpeg (`gfxcapture`) e os encoders de cada fabricante
- Captura de som por programa (excluindo Discord e o próprio app)
- Assistir pela janela do app (WebView2)

**Fica pra próximas versões:** miniaturas das telas, ajuste automático de qualidade quando o upload não aguenta, atalhos de teclado.
