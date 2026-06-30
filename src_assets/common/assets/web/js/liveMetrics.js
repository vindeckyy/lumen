/*
 * liveMetrics.js — poll /api/v1/sessions and render canvas time-series.
 *
 * Vanilla JS, no framework, no chart library. ~140 lines.
 * Polls every 1s; the payload is small. Switch to WebSocket only when
 * 1 Hz becomes a real bottleneck.
 */
(function () {
  'use strict';

  const POLL_MS = 1000;
  const HISTORY_LEN = 60; // 1 minute at 1 Hz
  const auth = btoa(`${document.cookie.match(/lumen-user=([^;]+)/)?.[1] || 'admin'}:${document.cookie.match(/lumen-pass=([^;]+)/)?.[1] || ''}`);
  // The web UI normally stores creds in localStorage and the request handler
  // attaches Basic auth. For now we rely on the browser's existing session
  // cookie. Add a real helper when SSO is wired.

  const charts = {
    fps:      makeChart('chart-fps', 'fps'),
    bitrate:  makeChart('chart-bitrate', 'kbps'),
    rtt:      makeChart('chart-rtt', 'ms'),
  };

  function makeChart(canvasId, unit) {
    const c = document.getElementById(canvasId);
    const ctx = c.getContext('2d');
    return {
      canvas: c, ctx,
      data: [],
      unit,
      push(v) {
        this.data.push(v);
        if (this.data.length > HISTORY_LEN) this.data.shift();
        this.draw();
      },
      draw() {
        const W = this.canvas.width, H = this.canvas.height;
        this.ctx.clearRect(0, 0, W, H);
        if (this.data.length < 2) return;
        const max = Math.max(1, ...this.data);
        const min = Math.min(0, ...this.data);
        const range = Math.max(1, max - min);
        this.ctx.strokeStyle = '#5ac8fa';
        this.ctx.lineWidth = 1.5;
        this.ctx.beginPath();
        this.data.forEach((v, i) => {
          const x = (i / (HISTORY_LEN - 1)) * W;
          const y = H - ((v - min) / range) * (H - 4) - 2;
          if (i === 0) this.ctx.moveTo(x, y); else this.ctx.lineTo(x, y);
        });
        this.ctx.stroke();
        // Axis label.
        this.ctx.fillStyle = '#888';
        this.ctx.font = '10px sans-serif';
        this.ctx.fillText(`max ${max.toFixed(0)} ${this.unit}`, 4, 12);
      },
    };
  }

  async function fetchSessions() {
    try {
      const r = await fetch('/api/v1/sessions', {
        credentials: 'same-origin',
        headers: { 'Accept': 'application/json', 'Authorization': `Basic ${auth}` },
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      const sessions = await r.json();
      document.getElementById('status').textContent =
        `${sessions.length} active session(s) — refreshed every ${POLL_MS}ms`;
      return sessions;
    } catch (e) {
      document.getElementById('status').textContent = `error: ${e.message}`;
      return [];
    }
  }

  function renderTable(sessions) {
    const tbody = document.getElementById('session-rows');
    if (!sessions.length) {
      tbody.innerHTML = '<tr><td colspan="7" class="lumen-empty">no active sessions</td></tr>';
      return;
    }
    tbody.innerHTML = sessions.map(s => `
      <tr>
        <td><code>${(s.id || '').slice(0, 12)}</code></td>
        <td>${esc(s.app_name)}</td>
        <td>${esc(s.client_name)}</td>
        <td>${(s.encoder_fps || 0).toFixed(1)}</td>
        <td>${s.bitrate_kbps || 0}</td>
        <td>${s.rtt_ms || 0}</td>
        <td>${s.frames_dropped || 0}</td>
      </tr>
    `).join('');
  }

  function esc(s) {
    return String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  }

  async function tick() {
    const sessions = await fetchSessions();
    renderTable(sessions);
    // Aggregate across sessions (simple sum/avg for now).
    const fps = sessions.reduce((a, s) => a + (s.encoder_fps || 0), 0);
    const bitrate = sessions.reduce((a, s) => a + (s.bitrate_kbps || 0), 0);
    const rtt = sessions.length
      ? sessions.reduce((a, s) => a + (s.rtt_ms || 0), 0) / sessions.length
      : 0;
    charts.fps.push(fps);
    charts.bitrate.push(bitrate);
    charts.rtt.push(rtt);
    document.getElementById('stat-fps').textContent = fps.toFixed(1);
    document.getElementById('stat-bitrate').textContent = bitrate.toFixed(0);
    document.getElementById('stat-rtt').textContent = rtt.toFixed(0);
  }

  tick();
  setInterval(tick, POLL_MS);
})();