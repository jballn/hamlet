// hamlet — 8-bit Village RPG Thin JS Glue Layer (SPEC §12.3; ADR-0044)

(function () {
  'use strict';

  let wasmInstance = null;
  let wasmMemory = null;
  let ws = null;
  let reconnectAttempts = 0;
  let reconnectTimer = null;
  let lastTime = performance.now();
  let avgFrameTime = 16.6;
  let isDegraded = false;
  let spritesLoaded = false;

  let selectedEntity = null;
  let hoveredEntity = null;
  let activePanelKind = null;
  let activePanelTarget = null;

  const canvas = document.getElementById('village-canvas');
  const ctx = canvas.getContext('2d');
  ctx.imageSmoothingEnabled = false;

  // Sprite sheets
  const charSheet = new Image();
  const villageSheet = new Image();
  let loadedCount = 0;
  function onImageLoad() {
    loadedCount++;
    if (loadedCount >= 2) {
      spritesLoaded = true;
    }
  }
  charSheet.onload = onImageLoad;
  villageSheet.onload = onImageLoad;
  charSheet.src = 'assets/sprites/characters.png';
  villageSheet.src = 'assets/sprites/village.png';

  // DOM elements
  const elBoot = document.getElementById('boot-overlay');
  const elBootPrompt = document.getElementById('boot-prompt');
  const elBadge = document.getElementById('connection-badge');
  const elFps = document.getElementById('fps-badge');
  const elInstance = document.getElementById('hud-instance');
  const elSeq = document.getElementById('hud-seq');
  const elAgents = document.getElementById('hud-agents');
  const elCommissions = document.getElementById('hud-commissions');
  const elDecrees = document.getElementById('hud-decrees');
  const elMail = document.getElementById('hud-mail');
  const elCost = document.getElementById('hud-cost');
  const elTicker = document.getElementById('ticker-text');
  const elSelected = document.getElementById('selected-info');
  const elAria = document.getElementById('aria-status');

  // Management Drawer DOM elements
  const elDrawer = document.getElementById('management-drawer');
  const elDrawerTitle = document.getElementById('drawer-title');
  const elDrawerSubtitle = document.getElementById('drawer-subtitle');
  const elDrawerClose = document.getElementById('drawer-close');
  const elDrawerContent = document.getElementById('drawer-content');
  const elMutationBanner = document.getElementById('mutation-banner');

  // Audio: Sting Sound on Console Boot (assets/audio/toBe.wav)
  let stingPlayed = false;
  function playStingSound() {
    if (stingPlayed) return;
    try {
      const audio = new Audio('assets/audio/toBe.wav');
      audio.volume = 0.5;
      const playPromise = audio.play();
      if (playPromise !== undefined) {
        playPromise.then(() => {
          stingPlayed = true;
        }).catch(() => {
          // Autoplay policy fallback: attach listener to first user interaction
          const unlock = function () {
            audio.play().then(() => { stingPlayed = true; }).catch(() => {});
            window.removeEventListener('click', unlock);
            window.removeEventListener('keydown', unlock);
          };
          window.addEventListener('click', unlock, { once: true });
        });
      }
    } catch (_) {
      // Audio optional / non-fatal
    }
  }

  function dismissBootScreen() {
    if (!elBoot) return;
    playStingSound();
    elBoot.classList.add('fade-out');
    setTimeout(() => {
      if (elBoot.parentNode) elBoot.parentNode.removeChild(elBoot);
    }, 600);
  }

  // Trigger boot sting audio and display brief logo splash
  setTimeout(dismissBootScreen, 1800);
  window.addEventListener('keydown', dismissBootScreen, { once: true });
  window.addEventListener('click', dismissBootScreen, { once: true });

  // WASM memory helper functions
  function passStringToWasm(str) {
    const encoder = new TextEncoder();
    const bytes = encoder.encode(str);
    const ptr = wasmInstance.exports.hamlet_web_alloc(bytes.length);
    const memView = new Uint8Array(wasmMemory.buffer, ptr, bytes.length);
    memView.set(bytes);
    return { ptr, len: bytes.length };
  }

  function readSceneFromWasm() {
    const lenPtr = wasmInstance.exports.hamlet_web_alloc(4);
    const scenePtr = wasmInstance.exports.hamlet_web_render_scene_json(lenPtr);
    const lenView = new Uint32Array(wasmMemory.buffer, lenPtr, 1);
    const len = lenView[0];
    wasmInstance.exports.hamlet_web_dealloc(lenPtr, 4);

    if (len === 0 || scenePtr === 0) return null;
    const strBytes = new Uint8Array(wasmMemory.buffer, scenePtr, len);
    const decoder = new TextDecoder();
    const jsonStr = decoder.decode(strBytes);
    return JSON.parse(jsonStr);
  }

  function readPanelFromWasm() {
    if (!wasmInstance || !wasmInstance.exports.hamlet_web_render_panel_json) return null;
    const lenPtr = wasmInstance.exports.hamlet_web_alloc(4);
    const panelPtr = wasmInstance.exports.hamlet_web_render_panel_json(lenPtr);
    const lenView = new Uint32Array(wasmMemory.buffer, lenPtr, 1);
    const len = lenView[0];
    wasmInstance.exports.hamlet_web_dealloc(lenPtr, 4);

    if (len === 0 || panelPtr === 0) return null;
    const strBytes = new Uint8Array(wasmMemory.buffer, panelPtr, len);
    const decoder = new TextDecoder();
    const jsonStr = decoder.decode(strBytes);
    return JSON.parse(jsonStr);
  }

  // WebSocket Connection with exponential backoff
  function connectWs() {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }

    const urlParams = new URLSearchParams(window.location.search);
    const token = urlParams.get('token') || '';
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}/api/v1/events/ws?token=${encodeURIComponent(token)}`;

    ws = new WebSocket(wsUrl);

    ws.onopen = function () {
      reconnectAttempts = 0;
      elBadge.textContent = 'Connected';
      elBadge.className = 'badge connected';
      elAria.textContent = 'Connected to hamlet daemon WebSocket stream.';
      if (elBootPrompt) elBootPrompt.textContent = 'READY. CLICK OR PRESS ANY KEY.';
    };

    ws.onmessage = function (event) {
      if (!wasmInstance) return;
      const { ptr, len } = passStringToWasm(event.data);
      wasmInstance.exports.hamlet_web_process_frame(ptr, len);
      wasmInstance.exports.hamlet_web_dealloc(ptr, len);

      // Inspect frame for CommandAck
      try {
        const parsed = JSON.parse(event.data);
        if (parsed.type === 'command_ack') {
          handleCommandAck(parsed);
        }
      } catch (_) {}

      // Check if view-model flagged a sequence gap or resync requirement (QA09-03)
      if (typeof wasmInstance.exports.hamlet_web_resync_needed === 'function' &&
          wasmInstance.exports.hamlet_web_resync_needed() !== 0) {
        if (typeof wasmInstance.exports.hamlet_web_clear_resync_needed === 'function') {
          wasmInstance.exports.hamlet_web_clear_resync_needed();
        }
        if (ws && ws.readyState === WebSocket.OPEN) {
          const resyncFrame = {
            type: 'resync',
            last_seen_seq: elSeq ? parseInt(elSeq.textContent, 10) || null : null,
            daemon_instance_id: elInstance ? elInstance.textContent || null : null
          };
          ws.send(JSON.stringify(resyncFrame));
        }
      }

      // Refresh management drawer if open
      if (activePanelKind) {
        refreshActivePanel();
      }
    };

    ws.onclose = function () {
      elBadge.textContent = 'Disconnected';
      elBadge.className = 'badge disconnected';

      // Exponential backoff with jitter: 500ms, 1000ms, 2000ms, 4000ms, max 8000ms
      const delay = Math.min(8000, 500 * Math.pow(2, reconnectAttempts)) + Math.floor(Math.random() * 200);
      reconnectAttempts++;
      elAria.textContent = `Disconnected from daemon. Reconnecting in ${(delay / 1000).toFixed(1)}s...`;
      reconnectTimer = setTimeout(connectWs, delay);
    };

    ws.onerror = function () {
      ws.close();
    };
  }

  // Command Channel Dispatch
  function dispatchCommand(actionName, payload, description, target) {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      showMutationBanner('error', 'Cannot send command: WebSocket disconnected.');
      return;
    }

    const cid = `cmd-${Date.now()}-${Math.floor(Math.random() * 10000)}`;

    // 1. Record optimistic mutation in WASM view-model
    if (wasmInstance && wasmInstance.exports.hamlet_web_record_mutation) {
      const pCid = passStringToWasm(cid);
      const pDesc = passStringToWasm(description || actionName);
      const pTgt = passStringToWasm(target || '');
      wasmInstance.exports.hamlet_web_record_mutation(
        pCid.ptr, pCid.len,
        pDesc.ptr, pDesc.len,
        pTgt.ptr, pTgt.len
      );
      wasmInstance.exports.hamlet_web_dealloc(pCid.ptr, pCid.len);
      wasmInstance.exports.hamlet_web_dealloc(pDesc.ptr, pDesc.len);
      wasmInstance.exports.hamlet_web_dealloc(pTgt.ptr, pTgt.len);
    }

    // 2. Transmit typed WsClientFrame::Command frame
    const frame = {
      type: 'command',
      correlation_id: cid,
      action: {
        action: actionName,
        payload: payload
      }
    };

    showMutationBanner('pending', `[Pending] ${description}...`);
    ws.send(JSON.stringify(frame));
  }

  function handleCommandAck(ack) {
    if (ack.success) {
      showMutationBanner('confirmed', `✓ Confirmed: ${ack.command_endpoint} (200 OK)`);
      setTimeout(() => {
        if (elMutationBanner.classList.contains('confirmed')) {
          elMutationBanner.classList.add('hidden');
        }
      }, 4000);
    } else {
      const err = ack.error || 'Command rejected by daemon';
      showMutationBanner('error', `✗ Rejected (${ack.status_code}): ${err}. Optimistic state rolled back.`);
    }
  }

  function showMutationBanner(status, text) {
    elMutationBanner.className = `mutation-banner ${status}`;
    elMutationBanner.textContent = text;
    elMutationBanner.classList.remove('hidden');
    elAria.textContent = text;
  }

  // Render loop with Frame-Budget Guard
  function loop(currentTime) {
    const dt = Math.max(1, currentTime - lastTime);
    lastTime = currentTime;

    // Moving average of frame time
    avgFrameTime = avgFrameTime * 0.95 + dt * 0.05;

    // Frame-budget guard: 60fps target (16.6ms). If avgFrameTime > 20ms (> 50fps drop), degrade bubbles first
    if (wasmInstance && wasmInstance.exports.hamlet_web_set_degraded) {
      if (!isDegraded && avgFrameTime > 20.0) {
        isDegraded = true;
        wasmInstance.exports.hamlet_web_set_degraded(1);
        elFps.textContent = 'DEGRADED';
        elFps.className = 'badge fps-degraded';
      } else if (isDegraded && avgFrameTime <= 16.7) {
        isDegraded = false;
        wasmInstance.exports.hamlet_web_set_degraded(0);
        elFps.textContent = '60 FPS';
        elFps.className = 'badge fps-good';
      }
    }

    if (wasmInstance) {
      wasmInstance.exports.hamlet_web_tick(Math.min(dt, 100));
      const scene = readSceneFromWasm();
      if (scene) {
        updateHud(scene.hud);
        renderScene(scene, currentTime);
      }
    }

    requestAnimationFrame(loop);
  }

  function updateHud(hud) {
    if (elInstance.textContent !== hud.daemon_instance_id && hud.daemon_instance_id) {
      elInstance.textContent = hud.daemon_instance_id;
    }
    elSeq.textContent = hud.seq;
    elAgents.textContent = hud.active_agents;
    elCommissions.textContent = hud.active_commissions;
    elDecrees.textContent = hud.pending_decrees;
    elMail.textContent = hud.unread_mail;
    const dollars = (hud.total_cost_micro_usd / 1000000).toFixed(2);
    elCost.textContent = `$${dollars}`;

    // Update village-wide ticker tape
    if (hud.ticker && hud.ticker.length > 0) {
      const latest = hud.ticker[0];
      if (elTicker.textContent !== latest) {
        elTicker.textContent = latest;
      }
    }
  }

  function renderScene(scene, currentTime) {
    const tileSize = scene.tile_size || 16;
    ctx.clearRect(0, 0, canvas.width, canvas.height);

    // 1. Terrain Grid & Cobblestone Path Network
    ctx.fillStyle = '#141d18';
    ctx.fillRect(0, 0, canvas.width, canvas.height);

    ctx.fillStyle = '#1c2922';
    for (let x = 8; x < canvas.width; x += 32) {
      for (let y = 8; y < canvas.height; y += 32) {
        ctx.fillRect(x, y, 2, 2);
      }
    }

    // Cobblestone Main Street / Path Network
    ctx.fillStyle = '#2d3830';
    ctx.fillRect(0, 11 * tileSize, canvas.width, 2 * tileSize);
    ctx.fillRect(9 * tileSize, 0, 2 * tileSize, canvas.height);
    ctx.fillRect(21 * tileSize, 0, 2 * tileSize, canvas.height);

    // 2. SPEC §2 Landmarks
    for (const lm of scene.landmarks) {
      const px = lm.x * tileSize;
      const py = lm.y * tileSize;
      const pw = lm.w * tileSize;
      const ph = lm.h * tileSize;

      const isSelected = selectedEntity && selectedEntity.type === 'landmark' && selectedEntity.id === lm.id;
      const isHovered = hoveredEntity && hoveredEntity.type === 'landmark' && hoveredEntity.id === lm.id;

      ctx.fillStyle = lm.color || '#333';
      ctx.fillRect(px, py, pw, ph);

      ctx.strokeStyle = isSelected ? '#58a6ff' : (isHovered ? '#ffecb3' : '#ffffff');
      ctx.lineWidth = isSelected ? 2 : 1;
      ctx.strokeRect(px + 1, py + 1, pw - 2, ph - 2);

      // Door indicator
      ctx.fillStyle = '#111';
      ctx.fillRect(px + (pw / 2) - 8, py + ph - 10, 16, 10);
      ctx.strokeStyle = '#555';
      ctx.strokeRect(px + (pw / 2) - 8, py + ph - 10, 16, 10);

      // Landmark title & status
      ctx.fillStyle = '#ffffff';
      ctx.font = 'bold 9px monospace';
      ctx.textAlign = 'center';
      ctx.fillText(lm.name, px + (pw / 2), py + 14);

      if (lm.status_label) {
        ctx.fillStyle = '#ffecb3';
        ctx.font = '8px monospace';
        ctx.fillText(lm.status_label, px + (pw / 2), py + 26);
      }
    }

    // 3. Census Workshop Parcels
    if (scene.workshops) {
      for (const ws of scene.workshops) {
        const wx = ws.x * tileSize;
        const wy = ws.y * tileSize;
        const ww = ws.w * tileSize;
        const wh = ws.h * tileSize;

        ctx.fillStyle = 'rgba(74, 90, 80, 0.4)';
        ctx.fillRect(wx, wy, ww, wh);
        ctx.strokeStyle = '#3fb950';
        ctx.lineWidth = 1;
        ctx.setLineDash([2, 2]);
        ctx.strokeRect(wx, wy, ww, wh);
        ctx.setLineDash([]);

        ctx.fillStyle = '#5c4033';
        ctx.fillRect(wx + 4, wy + wh - 8, ww - 8, 6);

        ctx.fillStyle = '#8b949e';
        ctx.font = '7px monospace';
        ctx.textAlign = 'center';
        ctx.fillText(`p-${ws.uid}`, wx + (ww / 2), wy + 8);
      }
    }

    // 4. Sprites
    for (const sp of scene.sprites) {
      let sx = sp.x * tileSize;
      let sy = sp.y * tileSize;

      const isSelected = selectedEntity && selectedEntity.type === 'sprite' && selectedEntity.uid === sp.uid;
      const isHovered = hoveredEntity && hoveredEntity.type === 'sprite' && hoveredEntity.uid === sp.uid;

      // Loiter animation bobbing
      if (sp.loiter && !sp.zzz && !sp.fallen) {
        const bob = Math.sin(currentTime / 250 + sp.uid) * 1.5;
        sy += bob;
      }

      // Draw character sprite
      if (spritesLoaded) {
        const srcX = sp.char_col * 17;
        const srcY = sp.char_row * 17;

        if (sp.fallen) {
          ctx.save();
          ctx.translate(sx + tileSize / 2, sy + tileSize / 2);
          ctx.rotate(Math.PI / 2);
          ctx.drawImage(charSheet, srcX, srcY, 16, 16, -tileSize / 2, -tileSize / 2, tileSize, tileSize);
          ctx.restore();
        } else {
          ctx.drawImage(charSheet, srcX, srcY, 16, 16, sx, sy, tileSize, tileSize);
        }
      } else {
        ctx.fillStyle = sp.is_mayor ? '#e63946' : (sp.fallen ? '#7f1d1d' : '#457b9d');
        ctx.fillRect(sx, sy, tileSize, tileSize);
      }

      // Tool-swing animation
      if (sp.tool_swing) {
        const swingAngle = Math.sin(currentTime / 120 + sp.uid) * 0.6;
        ctx.save();
        ctx.translate(sx + tileSize + 2, sy + 6);
        ctx.rotate(swingAngle);
        ctx.fillStyle = '#d29922';
        ctx.fillRect(-1, 0, 2, 7);
        ctx.fillStyle = '#f0f6fc';
        ctx.fillRect(-3, -2, 6, 3);
        ctx.restore();
      }

      // Sleeping "zzz"
      if (sp.zzz) {
        const zPhase = (currentTime / 400 + sp.uid) % 3;
        const zOffY = (zPhase * 4);
        ctx.fillStyle = '#79c0ff';
        ctx.font = 'bold 8px monospace';
        ctx.textAlign = 'left';
        ctx.fillText('z', sx + tileSize - 2, sy - 2 - zOffY);
        ctx.fillText('Z', sx + tileSize + 2, sy - 6 - zOffY);
      }

      // Attention mark (!)
      if (sp.attention_mark) {
        ctx.fillStyle = '#f85149';
        ctx.beginPath();
        ctx.arc(sx + tileSize / 2, sy - 6, 5, 0, 2 * Math.PI);
        ctx.fill();
        ctx.fillStyle = '#ffffff';
        ctx.font = 'bold 8px monospace';
        ctx.textAlign = 'center';
        ctx.fillText('!', sx + tileSize / 2, sy - 3);
      }

      // Selection reticle
      if (isSelected || isHovered) {
        ctx.strokeStyle = isSelected ? '#58a6ff' : '#ffecb3';
        ctx.lineWidth = 1;
        ctx.strokeRect(sx - 2, sy - 2, tileSize + 4, tileSize + 4);
      }

      // Name tag
      ctx.fillStyle = '#e6edf3';
      ctx.font = '8px monospace';
      ctx.textAlign = 'center';
      ctx.fillText(sp.name, sx + (tileSize / 2), sy - 2);

      // Sparkline
      if (sp.sparkline && sp.sparkline.length > 1) {
        const slWidth = 16;
        const slHeight = 4;
        const slX = sx;
        const slY = sy + tileSize + 2;

        const maxVal = Math.max(1, ...sp.sparkline);
        ctx.strokeStyle = '#3fb950';
        ctx.lineWidth = 1;
        ctx.beginPath();
        for (let i = 0; i < sp.sparkline.length; i++) {
          const px = slX + (i / (sp.sparkline.length - 1)) * slWidth;
          const py = slY + slHeight - (sp.sparkline[i] / maxVal) * slHeight;
          if (i === 0) ctx.moveTo(px, py);
          else ctx.lineTo(px, py);
        }
        ctx.stroke();
      }
    }

    // 5. Chat Bubbles
    for (const b of scene.bubbles) {
      const bx = b.x * tileSize;
      const by = b.y * tileSize;

      ctx.font = '9px monospace';
      const textWidth = ctx.measureText(b.text).width;
      const bw = Math.min(textWidth + 8, 220);
      const bh = 16;
      const bLeft = bx - (bw / 2);

      let borderColor = '#000000';
      let bgColor = '#ffffff';
      if (b.is_mail || b.kind === 'mail') {
        borderColor = '#d29922';
      } else if (b.kind === 'steer') {
        borderColor = '#58a6ff';
      } else if (b.kind === 'escalation') {
        borderColor = '#f85149';
        bgColor = '#fff5f5';
      } else if (b.kind === 'decree_ack') {
        borderColor = '#3fb950';
      }

      ctx.fillStyle = bgColor;
      ctx.strokeStyle = borderColor;
      ctx.lineWidth = 1.5;
      ctx.fillRect(bLeft, by - bh, bw, bh);
      ctx.strokeRect(bLeft, by - bh, bw, bh);

      ctx.beginPath();
      ctx.moveTo(bx - 3, by);
      ctx.lineTo(bx + 3, by);
      ctx.lineTo(bx, by + 4);
      ctx.fillStyle = bgColor;
      ctx.fill();
      ctx.strokeStyle = borderColor;
      ctx.lineWidth = 1;
      ctx.stroke();

      ctx.fillStyle = '#000000';
      ctx.textAlign = 'center';
      ctx.fillText(b.text, bx, by - 4, bw - 4);
    }
  }

  // ---------------------------------------------------------------------------
  // Work Management Pixel UI Panels (DOM-Hybrid Overlay)
  // ---------------------------------------------------------------------------

  function openPanel(kind, target) {
    activePanelKind = kind;
    activePanelTarget = target;

    if (!wasmInstance || !wasmInstance.exports.hamlet_web_open_panel) return;
    const pKind = passStringToWasm(kind);
    const pTgt = passStringToWasm(target || '');
    wasmInstance.exports.hamlet_web_open_panel(pKind.ptr, pKind.len, pTgt.ptr, pTgt.len);
    wasmInstance.exports.hamlet_web_dealloc(pKind.ptr, pKind.len);
    wasmInstance.exports.hamlet_web_dealloc(pTgt.ptr, pTgt.len);

    refreshActivePanel();
    elDrawer.classList.remove('hidden');
    elDrawerClose.focus();
  }

  function closePanel() {
    activePanelKind = null;
    activePanelTarget = null;
    if (wasmInstance && wasmInstance.exports.hamlet_web_close_panel) {
      wasmInstance.exports.hamlet_web_close_panel();
    }
    elDrawer.classList.add('hidden');
    elDrawerContent.innerHTML = '';
    elMutationBanner.classList.add('hidden');
    canvas.focus();
  }

  elDrawerClose.addEventListener('click', closePanel);

  function refreshActivePanel() {
    const panel = readPanelFromWasm();
    if (!panel || panel.type === 'none') {
      return;
    }

    switch (panel.type) {
      case 'agent':
        renderAgentPanel(panel.data);
        break;
      case 'guild':
        renderGuildPanel(panel.data);
        break;
      case 'vault':
        renderVaultPanel(panel.data);
        break;
      case 'decree_desk':
        renderDecreeDeskPanel(panel.data);
        break;
      case 'courier':
        renderCourierPanel(panel.data);
        break;
      case 'tavern':
        renderTavernPanel(panel.data);
        break;
      default:
        break;
    }
  }

  // 1. Agent Panel
  function renderAgentPanel(data) {
    elDrawerTitle.textContent = `Agent UID ${data.uid}: ${data.name}`;
    elDrawerSubtitle.textContent = `State: ${data.state} | Parcel: keep/census/parcel${data.uid}/`;

    const costStr = (data.cost_micro_usd / 1000000).toFixed(4);
    const memMb = (data.memory_bytes / (1024 * 1024)).toFixed(1);
    const cpuPct = (data.cpu_percent_x10 / 10).toFixed(1);

    let actionsHtml = '';
    if (data.can_steer) {
      actionsHtml += `
        <div class="panel-section">
          <h3>Inject Steer Directive</h3>
          <form id="form-steer" class="pixel-form">
            <input type="text" id="steer-input" class="pixel-input" placeholder="e.g. Recheck compiler tests" required />
            <button type="submit" class="pixel-btn">Send Steer Directive</button>
          </form>
        </div>
      `;
    }

    if (data.can_hibernate) {
      actionsHtml += `
        <div class="panel-section">
          <button id="btn-hibernate" class="pixel-btn secondary danger">Hibernate Agent to Tavern</button>
        </div>
      `;
    }

    if (data.can_wake) {
      actionsHtml += `
        <div class="panel-section">
          <button id="btn-wake" class="pixel-btn">Wake Agent from Stasis</button>
        </div>
      `;
    }

    const lineageList = data.lineage.map(l => `<div class="lineage-entry">${l}</div>`).join('');

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Live Telemetry</h3>
        <div class="telemetry-grid">
          <div class="telemetry-metric"><span class="metric-label">IN TOKENS:</span><span class="metric-val">${data.tokens_in}</span></div>
          <div class="telemetry-metric"><span class="metric-label">OUT TOKENS:</span><span class="metric-val">${data.tokens_out}</span></div>
          <div class="telemetry-metric"><span class="metric-label">COST:</span><span class="metric-val">$${costStr}</span></div>
          <div class="telemetry-metric"><span class="metric-label">LATENCY:</span><span class="metric-val">${data.latency_ms} ms</span></div>
          <div class="telemetry-metric"><span class="metric-label">MEMORY:</span><span class="metric-val">${memMb} MB</span></div>
          <div class="telemetry-metric"><span class="metric-label">CPU:</span><span class="metric-val">${cpuPct}%</span></div>
        </div>
      </div>

      <div class="panel-section">
        <h3>TITLE Lineage</h3>
        <div class="lineage-log">${lineageList}</div>
      </div>

      <div class="panel-section">
        <h3>DEED & PROPRIETOR Records</h3>
        <div class="lineage-entry"><strong>DEED:</strong> ${data.deed_summary}</div>
        <div class="lineage-entry"><strong>PROPRIETOR:</strong> ${data.proprietor_summary}</div>
      </div>

      ${actionsHtml}
    `;

    // Wire actions
    const formSteer = document.getElementById('form-steer');
    if (formSteer) {
      formSteer.addEventListener('submit', function (e) {
        e.preventDefault();
        const directive = document.getElementById('steer-input').value.trim();
        if (directive) {
          dispatchCommand(
            'agent_steer',
            { uid: data.uid, request: { directive: directive } },
            `Steer directive to ${data.name} (UID ${data.uid})`,
            `agent:${data.uid}`
          );
        }
      });
    }

    const btnHibernate = document.getElementById('btn-hibernate');
    if (btnHibernate) {
      btnHibernate.addEventListener('click', function () {
        dispatchCommand(
          'agent_hibernate',
          { uid: data.uid, request: { trigger: 'operator:ui' } },
          `Hibernate agent ${data.name} (UID ${data.uid})`,
          `agent:${data.uid}`
        );
      });
    }

    const btnWake = document.getElementById('btn-wake');
    if (btnWake) {
      btnWake.addEventListener('click', function () {
        dispatchCommand(
          'agent_wake',
          { patron: data.name, commission: data.commission_slug || null },
          `Wake patron ${data.name} into parcel`,
          `agent:${data.uid}`
        );
      });
    }
  }

  // 2. Guild Board Panel
  function renderGuildPanel(data) {
    elDrawerTitle.textContent = 'Guild Hall — Commissions Ledger';
    elDrawerSubtitle.textContent = `Active: ${data.active_count} | Draft: ${data.draft_count} | Closed: ${data.closed_count}`;

    const commList = data.commissions.map(c => {
      const stateClass = c.state.toLowerCase();
      return `
        <li class="pixel-list-item">
          <div>
            <strong>#${c.commission_id} ${c.slug}</strong>
            <span class="status-tag ${stateClass}">${c.state}</span>
          </div>
          <div style="font-size: 9px; color: var(--text-muted);">
            Roster: [${c.roster.join(', ')}] | Deliverables: ${c.deliverables_count}
          </div>
        </li>
      `;
    }).join('');

    const indentureList = data.indenture_entries.map(ie => `
      <li class="pixel-list-item">
        <span>UID ${ie.uid} (${ie.agent_name}): <em>${ie.goal}</em></span>
        <span class="status-tag ${ie.status === 'working' ? 'active' : 'draft'}">${ie.status}</span>
      </li>
    `).join('');

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Commissions Board</h3>
        <ul class="pixel-list">${commList || '<li class="pixel-list-item">No commissions registered.</li>'}</ul>
      </div>

      <div class="panel-section">
        <h3>Indenture Task Breakdown</h3>
        <ul class="pixel-list">${indentureList || '<li class="pixel-list-item">No active indentures.</li>'}</ul>
      </div>

      <div class="panel-section">
        <h3>Charter New Commission</h3>
        <form id="form-new-comm" class="pixel-form">
          <input type="text" id="comm-slug" class="pixel-input" placeholder="Slug name (e.g. compiler-v2)" required />
          <textarea id="comm-brief" class="pixel-textarea" placeholder="Commission brief requirements..." required></textarea>
          <button type="submit" class="pixel-btn">Charter Commission</button>
        </form>
      </div>

      ${data.can_close ? `
      <div class="panel-section">
        <h3>Close Commission</h3>
        <form id="form-close-comm" class="pixel-form">
          <select id="close-slug" class="pixel-select">
            ${data.commissions.filter(c => c.state === 'Active').map(c => `<option value="${c.slug}">${c.slug}</option>`).join('')}
          </select>
          <button type="submit" class="pixel-btn secondary danger">Close Commission</button>
        </form>
      </div>` : ''}
    `;

    const formNew = document.getElementById('form-new-comm');
    if (formNew) {
      formNew.addEventListener('submit', function (e) {
        e.preventDefault();
        const slug = document.getElementById('comm-slug').value.trim();
        const brief = document.getElementById('comm-brief').value.trim();
        if (slug && brief) {
          dispatchCommand(
            'commission_new',
            { slug: slug, brief: brief, seed_dir: null },
            `Charter commission ${slug}`,
            'guild'
          );
        }
      });
    }

    const formClose = document.getElementById('form-close-comm');
    if (formClose) {
      formClose.addEventListener('submit', function (e) {
        e.preventDefault();
        const slug = document.getElementById('close-slug').value;
        if (slug) {
          dispatchCommand(
            'commission_close',
            { slug: slug, request: { slug: slug } },
            `Close commission ${slug}`,
            'guild'
          );
        }
      });
    }
  }

  // 3. Vault Panel
  function renderVaultPanel(data) {
    elDrawerTitle.textContent = 'Keep & Vault — Deliverables';
    elDrawerSubtitle.textContent = `${data.total_count} promoted artifacts (${data.total_bytes} bytes)`;

    const artList = data.artifacts.map(a => `
      <li class="pixel-list-item" style="flex-direction: column; align-items: flex-start; gap: 2px;">
        <div style="font-weight: bold; color: var(--accent-gold);">${a.name}</div>
        <div style="font-size: 8px; color: var(--text-muted); word-break: break-all;">SHA256: ${a.sha256}</div>
        <div style="font-size: 9px; color: var(--text-main);">Size: ${a.size_bytes} bytes | Commission: ${a.commission || 'Trunk'}</div>
      </li>
    `).join('');

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Promoted Artifacts</h3>
        <ul class="pixel-list">${artList || '<li class="pixel-list-item">Vault is currently empty.</li>'}</ul>
      </div>

      ${data.can_export ? `
      <div class="panel-section">
        <h3>Export Artifact</h3>
        <form id="form-vault-export" class="pixel-form">
          <select id="export-artifact" class="pixel-select">
            ${data.artifacts.map(a => `<option value="${a.name}">${a.name}</option>`).join('')}
          </select>
          <input type="text" id="export-dest" class="pixel-input" placeholder="Destination path (e.g. /tmp/out.tar)" required />
          <button type="submit" class="pixel-btn">Export Artifact</button>
        </form>
      </div>` : ''}
    `;

    const formExport = document.getElementById('form-vault-export');
    if (formExport) {
      formExport.addEventListener('submit', function (e) {
        e.preventDefault();
        const artifact = document.getElementById('export-artifact').value;
        const dest = document.getElementById('export-dest').value.trim();
        if (artifact && dest) {
          dispatchCommand(
            'vault_export',
            { artifact: artifact, destination: dest, commission: null },
            `Export vault artifact ${artifact}`,
            'vault'
          );
        }
      });
    }
  }

  // 4. Decree Desk Panel
  function renderDecreeDeskPanel(data) {
    elDrawerTitle.textContent = "Mayor's Manor — Decree Desk";
    elDrawerSubtitle.textContent = `${data.pending_count} pending decrees | ${data.escalations.length} escalations`;

    const decreeList = data.decrees.map(d => {
      const stateClass = d.status.toLowerCase();
      return `
        <li class="pixel-list-item">
          <div>
            <strong>${d.decree_id}</strong>: ${d.action}
            <span class="status-tag ${stateClass}">${d.status}</span>
          </div>
          ${d.status === 'rejected' ? `<button class="pixel-btn secondary replay-decree-btn" data-id="${d.decree_id}" style="padding: 2px 4px; font-size: 8px;">Replay</button>` : ''}
        </li>
      `;
    }).join('');

    const escList = data.escalations.map(e => `
      <li class="pixel-list-item" style="flex-direction: column; align-items: flex-start; gap: 4px;">
        <div style="color: var(--accent-red); font-weight: bold;">! ${e.id} (${e.decree_id})</div>
        <div style="font-size: 9px;">${e.summary}</div>
        ${!e.resolved ? `
          <form class="resolve-esc-form pixel-form" data-id="${e.id}" style="width: 100%; margin-top: 4px;">
            <input type="text" class="pixel-input esc-res-input" placeholder="Operator resolution directive..." required />
            <button type="submit" class="pixel-btn" style="padding: 3px 6px;">Resolve Escalation</button>
          </form>
        ` : `<span class="status-tag active">RESOLVED</span>`}
      </li>
    `).join('');

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Decree Queue</h3>
        <ul class="pixel-list">${decreeList || '<li class="pixel-list-item">No decrees filed.</li>'}</ul>
      </div>

      <div class="panel-section">
        <h3>Escalation Inbox</h3>
        <ul class="pixel-list">${escList || '<li class="pixel-list-item">Zero open escalations.</li>'}</ul>
      </div>
    `;

    document.querySelectorAll('.resolve-esc-form').forEach(form => {
      form.addEventListener('submit', function (e) {
        e.preventDefault();
        const escId = this.getAttribute('data-id');
        const resInput = this.querySelector('.esc-res-input');
        const resText = resInput ? resInput.value.trim() : '';
        if (escId && resText) {
          dispatchCommand(
            'escalation_resolve',
            { id: escId, request: { resolution: resText } },
            `Resolve escalation ${escId}`,
            'manor'
          );
        }
      });
    });
  }

  // 5. Courier Panel
  function renderCourierPanel(data) {
    elDrawerTitle.textContent = 'Courier Post — Dispatch & Mail';
    elDrawerSubtitle.textContent = `${data.total_unread} unread mail spools`;

    let spoolsHtml = '';
    for (const [uid, count] of Object.entries(data.mail_counts)) {
      spoolsHtml += `<li class="pixel-list-item"><span>Slot UID ${uid}:</span> <span class="metric-val">${count} unread</span></li>`;
    }

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Resident Mail Spools</h3>
        <ul class="pixel-list">${spoolsHtml || '<li class="pixel-list-item">All spools empty.</li>'}</ul>
      </div>

      <div class="panel-section">
        <h3>Freight & Dispatch Telemetry</h3>
        <div class="lineage-entry">Freight sandboxes: ${data.freight_sandboxes_count}</div>
        <div class="lineage-entry">Dispatch outbox: ${data.dispatch_outbox_count} | Inbox: ${data.dispatch_inbox_count}</div>
      </div>

      <div class="panel-section">
        <h3>Compose Courier Mail</h3>
        <form id="form-send-mail" class="pixel-form">
          <input type="number" id="mail-to" class="pixel-input" placeholder="Recipient UID (e.g. 1000)" required />
          <input type="text" id="mail-subject" class="pixel-input" placeholder="Subject line" required />
          <textarea id="mail-body" class="pixel-textarea" placeholder="Message content..." required></textarea>
          <button type="submit" class="pixel-btn">Dispatch Mail</button>
        </form>
      </div>
    `;

    const formMail = document.getElementById('form-send-mail');
    if (formMail) {
      formMail.addEventListener('submit', function (e) {
        e.preventDefault();
        const toUid = parseInt(document.getElementById('mail-to').value, 10);
        const subject = document.getElementById('mail-subject').value.trim();
        const body = document.getElementById('mail-body').value.trim();
        if (!isNaN(toUid) && subject && body) {
          dispatchCommand(
            'mail_send',
            { to: toUid, from: 'operator', subject: subject, body: body },
            `Send mail to UID ${toUid}`,
            'courier'
          );
        }
      });
    }
  }

  // 6. Tavern Panel
  function renderTavernPanel(data) {
    elDrawerTitle.textContent = "Boar's Tusk Tavern — Patrons in Stasis";
    elDrawerSubtitle.textContent = `${data.stasis_count} patrons resting`;

    const patronList = data.patrons.map(p => `
      <li class="pixel-list-item">
        <div>
          <strong>${p.identity}</strong> (UID ${p.uid})
          <span style="font-size: 8px; color: var(--text-muted); display: block;">Last commission: ${p.last_commission || 'none'}</span>
        </div>
        <button class="pixel-btn awaken-patron-btn" data-name="${p.identity}" style="padding: 3px 6px;">Awaken</button>
      </li>
    `).join('');

    const gossipList = data.gossip_files.map(g => `<div class="lineage-entry">${g}</div>`).join('');

    elDrawerContent.innerHTML = `
      <div class="panel-section">
        <h3>Patrons Resting in Stasis</h3>
        <ul class="pixel-list">${patronList || '<li class="pixel-list-item">Zero patrons resting in stasis.</li>'}</ul>
      </div>

      <div class="panel-section">
        <h3>Gossip Files (/var/context/)</h3>
        <div class="lineage-log">${gossipList || '<div class="lineage-entry">No gossip files staged.</div>'}</div>
      </div>
    `;

    document.querySelectorAll('.awaken-patron-btn').forEach(btn => {
      btn.addEventListener('click', function () {
        const patronName = this.getAttribute('data-name');
        if (patronName) {
          dispatchCommand(
            'agent_wake',
            { patron: patronName, commission: null },
            `Awaken patron ${patronName} into village parcel`,
            'tavern'
          );
        }
      });
    });
  }

  // Find entity at tile coordinates
  function findEntityAt(scene, tx, ty) {
    if (!scene) return null;

    // Check sprites first (higher priority for small hitbox)
    const sp = scene.sprites.find(s => Math.floor(s.x) === tx && Math.floor(s.y) === ty);
    if (sp) {
      return { type: 'sprite', ...sp };
    }

    // Check workshops
    if (scene.workshops) {
      const ws = scene.workshops.find(w => tx >= w.x && tx < w.x + w.w && ty >= w.y && ty < w.y + w.h);
      if (ws) {
        return { type: 'workshop', ...ws };
      }
    }

    // Check landmarks
    const lm = scene.landmarks.find(l => tx >= l.x && tx < l.x + l.w && ty >= l.y && ty < l.y + l.h);
    if (lm) {
      return { type: 'landmark', ...lm };
    }

    return null;
  }

  function selectEntity(entity) {
    selectedEntity = entity;

    // Update active state on landmark buttons
    document.querySelectorAll('.nav-btn').forEach(btn => {
      const lId = btn.getAttribute('data-landmark');
      if (entity && entity.type === 'landmark' && entity.id === lId) {
        btn.classList.add('active');
      } else {
        btn.classList.remove('active');
      }
    });

    if (!entity) {
      elSelected.textContent = 'Select a landmark building or workshop sprite to inspect. Hotkeys 1–6 navigate landmarks.';
      closePanel();
      return;
    }

    if (entity.type === 'landmark') {
      elSelected.textContent = `[${entity.name}] (${entity.tree_path}) — ${entity.status_label}`;
      elAria.textContent = `Selected ${entity.name}. ${entity.status_label}.`;

      // Open corresponding panel
      switch (entity.id) {
        case 'guild':
          openPanel('guild', 'guild');
          break;
        case 'courier':
          openPanel('courier', 'courier');
          break;
        case 'keep':
          openPanel('vault', 'keep');
          break;
        case 'tavern':
          openPanel('tavern', 'tavern');
          break;
        case 'market':
          openPanel('guild', 'guild');
          break;
        case 'manor':
          openPanel('decree_desk', 'manor');
          break;
        default:
          break;
      }
    } else if (entity.type === 'sprite') {
      const costStr = (entity.cost_micro_usd / 1000000).toFixed(4);
      elSelected.textContent = `[Agent UID ${entity.uid}] (${entity.name}) State: ${entity.anim_state} | In: ${entity.tokens_in} | Out: ${entity.tokens_out} | Cost: $${costStr}`;
      elAria.textContent = `Selected agent ${entity.name}, status ${entity.anim_state}.`;

      openPanel('agent', String(entity.uid));
    } else if (entity.type === 'workshop') {
      elSelected.textContent = `[Workshop: ${entity.parcel_slug}] (${entity.tree_path}) — Assigned to ${entity.agent_name} (${entity.state})`;
      elAria.textContent = `Selected workshop ${entity.parcel_slug}.`;

      openPanel('agent', String(entity.uid));
    }

    // Dispatch DOM custom event for listeners
    window.dispatchEvent(new CustomEvent('hamlet:selection', { detail: entity }));
  }

  // Mouse canvas interaction
  canvas.addEventListener('mousemove', function (e) {
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / rect.width;
    const scaleY = canvas.height / rect.height;
    const tx = Math.floor(((e.clientX - rect.left) * scaleX) / 16);
    const ty = Math.floor(((e.clientY - rect.top) * scaleY) / 16);

    const scene = readSceneFromWasm();
    hoveredEntity = findEntityAt(scene, tx, ty);
  });

  canvas.addEventListener('mouseleave', function () {
    hoveredEntity = null;
  });

  canvas.addEventListener('click', function (e) {
    const rect = canvas.getBoundingClientRect();
    const scaleX = canvas.width / rect.width;
    const scaleY = canvas.height / rect.height;
    const tx = Math.floor(((e.clientX - rect.left) * scaleX) / 16);
    const ty = Math.floor(((e.clientY - rect.top) * scaleY) / 16);

    const scene = readSceneFromWasm();
    const entity = findEntityAt(scene, tx, ty);
    selectEntity(entity);
  });

  // Landmark navigation buttons click
  document.querySelectorAll('.nav-btn').forEach(btn => {
    btn.addEventListener('click', function () {
      const landmarkId = this.getAttribute('data-landmark');
      const scene = readSceneFromWasm();
      if (!scene) return;
      const lm = scene.landmarks.find(l => l.id === landmarkId);
      if (lm) {
        selectEntity({ type: 'landmark', ...lm });
      }
    });
  });

  // Keyboard Navigation: Hotkeys 1–6 map to landmarks, Esc closes panel
  const landmarkKeys = {
    '1': 'guild',
    '2': 'courier',
    '3': 'keep',
    '4': 'tavern',
    '5': 'market',
    '6': 'manor'
  };

  window.addEventListener('keydown', function (e) {
    // Esc closes panel if open
    if (e.key === 'Escape') {
      if (activePanelKind) {
        closePanel();
        e.preventDefault();
        return;
      }
    }

    // Avoid hijacking hotkeys if user is typing into input or textarea
    if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA' || e.target.tagName === 'SELECT') {
      return;
    }

    if (landmarkKeys[e.key]) {
      const landmarkId = landmarkKeys[e.key];
      const scene = readSceneFromWasm();
      if (!scene) return;
      const lm = scene.landmarks.find(l => l.id === landmarkId);
      if (lm) {
        selectEntity({ type: 'landmark', ...lm });
        const btn = document.querySelector(`.nav-btn[data-landmark="${landmarkId}"]`);
        if (btn) btn.focus();
      }
    }
  });

  // Load WASM Module
  async function initWasm() {
    try {
      const response = await fetch('hamlet_web.wasm');
      const buffer = await response.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(buffer, {});
      wasmInstance = instance;
      wasmMemory = instance.exports.memory;
      wasmInstance.exports.hamlet_web_init();
      elAria.textContent = 'WASM village view-model initialized.';

      connectWs();
      requestAnimationFrame(loop);
    } catch (err) {
      console.error('Failed to load WASM module:', err);
      elAria.textContent = 'Failed to load hamlet_web.wasm.';
    }
  }

  initWasm();
})();
