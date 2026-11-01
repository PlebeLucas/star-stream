// STAR // STREAM: efeitos de interação, iguais na tela de quem transmite e no player.
(function () {
  const svg = (paths) =>
    `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="square" stroke-linejoin="miter" aria-hidden="true">${paths}</svg>`;

  // Reações: ícone desenhado + nome + cor
  const REACTIONS = {
    fogo: { label: 'FOGO', c: '#FF7A1A', icon: svg('<path d="M12 2c1 4 6 6 6 12a6 6 0 0 1-12 0c0-3 2-5 3-7 0 3 1 4 3 5-1-4 0-7 0-10z"/>') },
    kkk: { label: 'KKKKK', c: '#F2E205', icon: svg('<rect x="3" y="3" width="18" height="18"/><path d="M8 9h1M15 9h1M7 14c2 3 8 3 10 0z"/>') },
    gg: { label: 'GG', c: '#3DDC84', icon: svg('<path d="M4 4h16v16H4z"/><path d="M10 9H7v6h3v-3H9M17 9h-3v6h3v-3h-1"/>') },
    susto: { label: 'SUSTO', c: '#B026FF', icon: svg('<path d="M5 10a7 7 0 0 1 14 0v5l-2 2v3h-3v-2h-4v2H7v-3l-2-2z"/><path d="M9 11h1M14 11h1M11 15h2"/>') },
    brabo: { label: 'BRABO', c: '#00E5FF', icon: svg('<path d="M13 2L4 14h7l-1 8 9-12h-7z"/>') },
  };

  const esc = (s) => String(s || '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

  function layer(root) {
    let l = root.querySelector(':scope > .fx-layer');
    if (!l) {
      l = document.createElement('div');
      l.className = 'fx-layer';
      l.innerHTML = '<div class="fx-reacts"></div>';
      root.appendChild(l);
    }
    return l;
  }

  function removeLater(el, ms) {
    setTimeout(() => el.remove(), ms);
  }

  /** Ping em x,y (0..1) dentro de `box` = {left, top, width, height} em px do layer. */
  function ping(root, x, y, name, box) {
    const l = layer(root);
    const b = box || { left: 0, top: 0, width: l.clientWidth, height: l.clientHeight };
    const el = document.createElement('div');
    el.className = 'fx-ping';
    el.style.left = b.left + x * b.width + 'px';
    el.style.top = b.top + y * b.height + 'px';
    el.innerHTML = `<i class="cross h"></i><i class="cross v"></i><i class="ring"></i><i class="ring r2"></i><i class="core"></i>
      <div class="tag">${esc(name)}<small>PING · ${String(Math.round(x * 100)).padStart(2, '0')}/${String(Math.round(y * 100)).padStart(2, '0')}</small></div>`;
    // Nome do lado esquerdo se o ping estiver no canto direito
    if (x > 0.8) el.querySelector('.tag').style.cssText = 'left:auto;right:18px;border-left:0;border-right:3px solid var(--fx-acc)';
    l.appendChild(el);
    removeLater(el, 2800);
  }

  function react(root, kind, name) {
    const r = REACTIONS[kind];
    if (!r) return;
    const box = layer(root).querySelector('.fx-reacts');
    const el = document.createElement('div');
    el.className = 'fx-react';
    el.style.setProperty('--c', r.c);
    el.innerHTML = `${r.icon}<span>${r.label}<small>${esc(name)}</small></span>`;
    box.appendChild(el);
    // No máximo 8 na pilha
    while (box.children.length > 8) box.firstChild.remove();
    removeLater(el, 3300);
  }

  function flash(root, text, sub) {
    const l = layer(root);
    const el = document.createElement('div');
    el.className = 'fx-flash';
    el.innerHTML = `<i class="dot"></i><span>${esc(text)}${sub ? `<br><small style="font-family:'JetBrains Mono',monospace;font-weight:400;font-size:11px;color:#9AA1AB">${esc(sub)}</small>` : ''}</span>`;
    l.appendChild(el);
    const scan = document.createElement('div');
    scan.className = 'fx-scan';
    l.appendChild(scan);
    removeLater(el, 3500);
    removeLater(scan, 700);
  }

  /** Aviso fixo (controle remoto). text vazio = esconde. */
  function banner(root, text, sub) {
    const l = layer(root);
    let el = l.querySelector('.fx-banner');
    if (!text) {
      if (el) el.remove();
      return;
    }
    if (!el) {
      el = document.createElement('div');
      el.className = 'fx-banner';
      l.appendChild(el);
    }
    el.innerHTML = `<i class="dot"></i><span>${esc(text)}</span>${sub ? `<small>${esc(sub)}</small>` : ''}`;
  }

  window.STARFX = { REACTIONS, ping, react, flash, banner };
})();
