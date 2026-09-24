/* Token 用量驾驶舱 — 交互与图表（干净室自研，非上游衍生） */
(function () {
  "use strict";

  // ── 状态 ──
  const S = {
    payload: null,
    src: "",
    win: 7,
    customStart: null,
    customEnd: null,
    dark: true,
    openSession: null,
  };

  const MODEL_COLORS = [
    "#5ba3d9",
    "#d4b06a",
    "#6dba8a",
    "#d98b6b",
    "#9b8fd4",
    "#6bb7c4",
    "#c48fb0",
    "#8fa3bf",
  ];

  // ── 工具 ──
  const $ = (id) => document.getElementById(id);
  const fmtInt = (n) => (n || 0).toLocaleString("en-US");
  function fmtTokens(n) {
    n = n || 0;
    if (n >= 1e8) return (n / 1e8).toFixed(2) + " 亿";
    if (n >= 1e4) return (n / 1e4).toFixed(1) + " 万";
    return String(Math.round(n));
  }
  function pad2(n) {
    return String(n).padStart(2, "0");
  }
  function fmtDate(minuteTs) {
    const d = new Date(minuteTs * 60000);
    return d.getMonth() + 1 + "-" + pad2(d.getDate());
  }
  function fmtTime(minuteTs) {
    const d = new Date(minuteTs * 60000);
    return pad2(d.getHours()) + ":" + pad2(d.getMinutes());
  }
  function dayKey(minuteTs) {
    const d = new Date(minuteTs * 60000);
    return d.getFullYear() + "-" + pad2(d.getMonth() + 1) + "-" + pad2(d.getDate());
  }

  /** 记录元组 → 对象：[分钟戳, 模型下标, total, input, output, 缓存命中, 缓存写入] */
  function recView(r, models) {
    return {
      min: r[0],
      model: models[r[1]] || "?",
      total: r[2] || 0,
      input: r[3] || 0,
      output: r[4] || 0,
      cacheHit: r[5] || 0,
      cacheWrite: r[6] || 0,
    };
  }

  function currentSource() {
    const p = S.payload;
    if (!p) return null;
    return p.sources[S.src] || null;
  }

  function cutMs() {
    const p = S.payload;
    const gen = p && p.genMs ? p.genMs : Date.now();
    if (S.win === "custom" && S.customStart != null) {
      const lo = S.customStart;
      const hi = S.customEnd != null ? S.customEnd : Infinity;
      return { lo: lo * 60000, hi: hi * 60000, gen };
    }
    const days = Number(S.win) || 7;
    return { lo: gen - days * 86400e3, hi: Infinity, gen };
  }

  function iterReq(src, cb) {
    if (!src) return;
    const models = src.models || [];
    const sess = src.sess || [];
    for (const s of sess) {
      for (const r of s.r || []) {
        const v = recView(r, models);
        v.ws = (src.ws || [])[s.w] || "";
        v.sessId = s.id;
        v.title = s.t || s.id;
        cb(v, s);
      }
    }
  }

  function filterWindow(src) {
    const { lo, hi } = cutMs();
    const out = [];
    iterReq(src, (v) => {
      const ms = v.min * 60000;
      if (ms >= lo && ms <= hi) out.push(v);
    });
    return out;
  }

  function aggregate(src) {
    const reqs = filterWindow(src);
    let total = 0;
    let input = 0;
    let output = 0;
    let cacheHit = 0;
    let cacheWrite = 0;
    const byModel = new Map();
    const byDay = new Map();
    const byHour = new Array(24).fill(0);
    const byHourModel = Array.from({ length: 24 }, () => new Map());
    const sessAgg = new Map();

    for (const r of reqs) {
      total += r.total;
      input += r.input;
      output += r.output;
      cacheHit += r.cacheHit;
      cacheWrite += r.cacheWrite;

      byModel.set(r.model, (byModel.get(r.model) || 0) + r.total);

      const dk = dayKey(r.min);
      byDay.set(dk, (byDay.get(dk) || 0) + r.total);

      const h = new Date(r.min * 60000).getHours();
      byHour[h] += r.total;
      const hm = byHourModel[h];
      hm.set(r.model, (hm.get(r.model) || 0) + r.total);

      let s = sessAgg.get(r.sessId);
      if (!s) {
        s = {
          id: r.sessId,
          title: r.title,
          ws: r.ws,
          total: 0,
          count: 0,
          first: r.min,
          last: r.min,
          reqs: [],
        };
        sessAgg.set(r.sessId, s);
      }
      s.total += r.total;
      s.count += 1;
      s.first = Math.min(s.first, r.min);
      s.last = Math.max(s.last, r.min);
      s.reqs.push(r);
    }

    const sessions = [...sessAgg.values()].sort((a, b) => b.total - a.total);
    const models = [...byModel.entries()]
      .map(([name, total]) => ({ name, total }))
      .sort((a, b) => b.total - a.total);

    return {
      reqs,
      total,
      input,
      output,
      cacheHit,
      cacheWrite,
      byModel,
      byDay,
      byHour,
      byHourModel,
      sessions,
      models,
      requestCount: reqs.length,
      sessionCount: sessions.length,
    };
  }

  // ── 价格（牌价粗估，元 / 百万 token：输入, 输出, 缓存命中）──
  const PRICE = {
    "claude-opus-5": [150, 750, 15],
    "claude-opus-4-8": [110, 550, 11],
    claude: [110, 550, 11],
    "deepseek-v4-flash": [1, 4, 0.2],
    "deepseek-v4-pro": [2, 8, 0.5],
    deepseek: [2, 8, 0.5],
    "glm-5.3": [2, 8, 0.4],
    "glm-5.2-x": [1.2, 6, 0.24],
    "glm-5.2": [0.8, 3.2, 0.16],
    glm: [1, 4, 0.2],
    kimi: [1.5, 6, 0.3],
    qwen: [2.4, 9.6, 0.48],
    hy3: [4, 16, 0.8],
    hy: [4, 16, 0.8],
    minimax: [1, 5, 0.2],
    m3: [1, 5, 0.2],
  };
  function priceOf(name) {
    const n = (name || "").toLowerCase();
    for (const k in PRICE) if (n.startsWith(k)) return PRICE[k];
    return [2, 8, 0.4];
  }
  function costCny(reqs) {
    let sum = 0;
    for (const r of reqs) {
      const [pin, pout, pch] = priceOf(r.model);
      sum +=
        (r.input / 1e6) * pin +
        (r.output / 1e6) * pout +
        (r.cacheHit / 1e6) * pch +
        (r.cacheWrite / 1e6) * pin * 1.25;
    }
    return sum;
  }

  // ── 渲染 ──
  function renderSourceSeg() {
    const p = S.payload;
    const seg = $("source-seg");
    seg.innerHTML = "";
    (p.order || []).forEach((k) => {
      const src = p.sources[k];
      if (!src) return;
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = src.name || k;
      b.dataset.key = k;
      if (k === S.src) b.classList.add("on");
      b.addEventListener("click", () => {
        S.src = k;
        S.openSession = null;
        closeDrawer();
        try {
          localStorage.setItem("tcd-src", k);
        } catch (_) {}
        renderAll();
      });
      seg.appendChild(b);
    });
  }

  function renderKpis(A) {
    const days = S.win === "custom" ? Math.max(1, Math.round((cutMs().hi - cutMs().lo) / 86400e3)) : Number(S.win) || 7;
    const daily = A.total / days;
    const inOut = A.output > 0 ? A.input / A.output : A.input > 0 ? Infinity : 0;
    const cacheRate =
      A.input + A.cacheWrite > 0 ? A.cacheHit / (A.input + A.cacheWrite + A.cacheHit) : 0;
    const cost = costCny(A.reqs);

    $("kpi-strip").innerHTML = [
      kpi("合计 Token", fmtTokens(A.total), `${fmtInt(A.requestCount)} 次请求`, "accent"),
      kpi("预估金额", "¥ " + cost.toFixed(2), "按公开牌价粗估", "gold"),
      kpi("会话 / 日均", `${A.sessionCount} / ${fmtTokens(daily)}`, `输入:输出 ${fmtRatio(inOut)}`),
      kpi("缓存命中率", (cacheRate * 100).toFixed(1) + "%", `命中 ${fmtTokens(A.cacheHit)}`),
    ].join("");
  }

  function kpi(label, value, sub, cls) {
    return `<div class="kpi ${cls || ""}">
      <div class="k-label">${label}</div>
      <div class="k-value">${value}</div>
      <div class="k-sub">${sub}</div>
    </div>`;
  }

  function fmtRatio(x) {
    if (!isFinite(x)) return "∞";
    if (x >= 10) return x.toFixed(0) + ":1";
    return x.toFixed(1) + ":1";
  }

  function renderCal(A) {
    const cal = $("cal");
    const days = [...A.byDay.entries()].sort();
    if (days.length === 0) {
      cal.innerHTML = `<div class="empty">窗口内没有数据</div>`;
      return;
    }
    const max = Math.max(...days.map((d) => d[1]), 1);
    cal.innerHTML = days
      .map(([, v]) => {
        const t = Math.log1p(v) / Math.log1p(max);
        const alpha = 0.12 + t * 0.88;
        return `<div class="cell has" title="${fmtTokens(v)} tokens" style="background:rgba(91,163,217,${alpha.toFixed(3)})"></div>`;
      })
      .join("");
  }

  function renderHour(A) {
    const cv = $("hour-chart");
    const ctx = cv.getContext("2d");
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    const padL = 8;
    const padB = 18;
    const padT = 8;
    const max = Math.max(...A.byHour, 1);
    const bw = (W - padL * 2) / 24;

    for (let h = 0; h < 24; h++) {
      const x = padL + h * bw;
      const stack = A.byHourModel[h];
      let acc = 0;
      const items = [...stack.entries()].sort((a, b) => b[1] - a[1]);
      for (const [name, v] of items) {
        const idx = modelIndex(name, A);
        const hgt = ((v / max) * (H - padT - padB));
        ctx.fillStyle = MODEL_COLORS[idx % MODEL_COLORS.length];
        ctx.fillRect(x + 1, H - padB - acc - hgt, bw - 2, hgt);
        acc += hgt;
      }
      if (h % 3 === 0) {
        ctx.fillStyle = "rgba(139,152,171,0.9)";
        ctx.font = "10px sans-serif";
        ctx.textAlign = "center";
        ctx.fillText(String(h), x + bw / 2, H - 4);
      }
    }
  }

  function modelIndex(name, A) {
    const i = A.models.findIndex((m) => m.name === name);
    return i >= 0 ? i : 0;
  }

  function renderModels(A) {
    const cv = $("model-chart");
    const ctx = cv.getContext("2d");
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);

    const total = A.models.reduce((s, m) => s + m.total, 0) || 1;
    const cx = W * 0.34;
    const cy = H / 2;
    const r = Math.min(W, H) * 0.36;
    let ang = -Math.PI / 2;

    A.models.forEach((m, i) => {
      const slice = (m.total / total) * Math.PI * 2;
      ctx.beginPath();
      ctx.moveTo(cx, cy);
      ctx.arc(cx, cy, r, ang, ang + slice);
      ctx.closePath();
      ctx.fillStyle = MODEL_COLORS[i % MODEL_COLORS.length];
      ctx.fill();
      ang += slice;
    });

    // 中空 → 环
    ctx.globalCompositeOperation = "destination-out";
    ctx.beginPath();
    ctx.arc(cx, cy, r * 0.62, 0, Math.PI * 2);
    ctx.fill();
    ctx.globalCompositeOperation = "source-over";

    ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue("--ink") || "#e8eef7";
    ctx.font = "600 13px sans-serif";
    ctx.textAlign = "center";
    ctx.fillText(fmtTokens(total), cx, cy + 4);

    $("model-list").innerHTML =
      A.models.length === 0
        ? `<div class="empty">没有模型数据</div>`
        : A.models
            .map((m, i) => {
              const pct = ((m.total / total) * 100).toFixed(1);
              return `<div class="model-row">
                <span class="swatch" style="background:${MODEL_COLORS[i % MODEL_COLORS.length]}"></span>
                <span class="m-name">${esc(m.name)}</span>
                <span class="m-val">${fmtTokens(m.total)} · ${pct}%</span>
              </div>`;
            })
            .join("");
  }

  function renderSessions(A) {
    const rail = $("session-rail");
    if (A.sessions.length === 0) {
      rail.innerHTML = `<div class="empty">窗口内没有会话</div>`;
      return;
    }
    rail.innerHTML = A.sessions
      .slice(0, 80)
      .map(
        (s, i) => `<div class="sess-row" data-i="${i}">
          <div class="s-title">${esc(s.title)}</div>
          <div class="s-meta">${fmtDate(s.first)}</div>
          <div class="s-meta">${s.count} 请求</div>
          <div class="s-meta">${fmtTokens(s.total)}</div>
        </div>`,
      )
      .join("");

    rail.querySelectorAll(".sess-row").forEach((el) => {
      el.addEventListener("click", () => {
        const i = Number(el.dataset.i);
        openDrawer(A.sessions[i], A);
      });
    });
  }

  function esc(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function openDrawer(sess, A) {
    S.openSession = sess;
    $("drawer").classList.remove("hidden");
    $("scrim").classList.remove("hidden");
    $("drawer").setAttribute("aria-hidden", "false");
    $("drawer-title").textContent = sess.title;
    $("drawer-sub").textContent =
      (sess.ws ? sess.ws + " · " : "") +
      sess.count +
      " 次请求 · " +
      fmtTokens(sess.total) +
      " tokens";

    drawScatter(sess.reqs, A);

    const tb = $("req-table").querySelector("tbody");
    tb.innerHTML = sess.reqs
      .slice()
      .reverse()
      .map(
        (r) => `<tr>
          <td>${fmtDate(r.min)} ${fmtTime(r.min)}</td>
          <td>${esc(r.model)}</td>
          <td>${fmtInt(r.input)}</td>
          <td>${fmtInt(r.output)}</td>
          <td>${fmtInt(r.cacheHit)}</td>
          <td>${fmtInt(r.total)}</td>
        </tr>`,
      )
      .join("");
  }

  function closeDrawer() {
    S.openSession = null;
    $("drawer").classList.add("hidden");
    $("scrim").classList.add("hidden");
    $("drawer").setAttribute("aria-hidden", "true");
  }

  function drawScatter(reqs, A) {
    const cv = $("scatter");
    const ctx = cv.getContext("2d");
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    if (reqs.length === 0) return;

    const pad = 12;
    const maxT = Math.max(...reqs.map((r) => r.total), 1);
    const minT = Math.min(...reqs.map((r) => r.min));
    const maxMin = Math.max(...reqs.map((r) => r.min), minT + 1);

    for (const r of reqs) {
      const x = pad + ((r.min - minT) / (maxMin - minT)) * (W - pad * 2);
      const y = H - pad - (r.total / maxT) * (H - pad * 2);
      const idx = modelIndex(r.model, A);
      ctx.beginPath();
      ctx.arc(x, y, 3, 0, Math.PI * 2);
      ctx.fillStyle = MODEL_COLORS[idx % MODEL_COLORS.length];
      ctx.fill();
    }
  }

  function renderAll() {
    const p = S.payload;
    if (!p) return;
    $("gen-stamp").textContent = "生成于 " + (p.gen || "—");
    renderSourceSeg();

    // 窗口按钮态
    document.querySelectorAll("#window-seg button").forEach((b) => {
      const v = b.dataset.win;
      const on =
        (S.win === "custom" && v === "custom") || String(S.win) === v;
      b.classList.toggle("on", on);
    });
    $("custom-range").classList.toggle("hidden", S.win !== "custom");

    const src = currentSource();
    const A = aggregate(src);
    renderKpis(A);
    renderCal(A);
    renderHour(A);
    renderModels(A);
    renderSessions(A);
    if (S.openSession) {
      const again = A.sessions.find((s) => s.id === S.openSession.id);
      if (again) openDrawer(again, A);
      else closeDrawer();
    }
  }

  // ── 事件 ──
  function bind() {
    document.querySelectorAll("#window-seg button").forEach((b) => {
      b.addEventListener("click", () => {
        const v = b.dataset.win;
        if (v === "custom") {
          S.win = "custom";
        } else {
          S.win = Number(v);
        }
        S.openSession = null;
        closeDrawer();
        renderAll();
      });
    });

    $("c-apply")?.addEventListener("click", () => {
      const a = $("c-start").value;
      const b = $("c-end").value;
      if (!a) return;
      const toMin = (s) => {
        const d = new Date(s + "T00:00:00");
        return Math.floor(d.getTime() / 60000);
      };
      S.customStart = toMin(a);
      S.customEnd = b ? toMin(b) + 24 * 60 : null;
      S.win = "custom";
      renderAll();
    });

    $("theme-btn").addEventListener("click", () => {
      S.dark = !S.dark;
      document.documentElement.dataset.theme = S.dark ? "dark" : "light";
      $("theme-btn").textContent = S.dark ? "浅色" : "深色";
      try {
        localStorage.setItem("tcd-theme", S.dark ? "dark" : "light");
      } catch (_) {}
      renderAll();
    });

    $("drawer-close").addEventListener("click", closeDrawer);
    $("scrim").addEventListener("click", closeDrawer);
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") closeDrawer();
    });
  }

  // ── 样例数据（预览用；脚本注入后覆盖）──
  function fillDemoIfEmpty(p) {
    const hasAny = (p.order || []).some((k) => (p.sources[k]?.sess || []).length > 0);
    if (hasAny) return p;
    const now = Date.now();
    p.genMs = now;
    p.gen = p.gen || new Date().toLocaleString();
    const models = ["claude-opus-5", "glm-5.3", "deepseek-v4-pro"];
    const sess = [];
    for (let d = 6; d >= 0; d--) {
      const base = Math.floor((now - d * 86400e3) / 60000);
      const id = "s-demo-" + d;
      const rs = [];
      for (let i = 0; i < 8 + d; i++) {
        const mi = (d + i) % models.length;
        const inp = 800 + ((d * 13 + i * 7) % 400) * 20;
        const out = 200 + ((d * 5 + i * 3) % 200) * 8;
        rs.push([base + i * 17, mi, inp + out, inp, out, Math.floor(inp * 0.3), Math.floor(inp * 0.05)]);
      }
      sess.push({
        w: 0,
        id,
        t: "样例会话 " + (d + 1),
        st: base,
        r: rs,
      });
    }
    p.sources.wb.sess = sess;
    p.sources.wb.models = models;
    p.sources.wb.genMs = now;
    return p;
  }

  function loadPayload() {
    try {
      const raw = $("payload").textContent;
      const p = JSON.parse(raw);
      S.payload = fillDemoIfEmpty(p);
    } catch (e) {
      console.error("payload 解析失败", e);
      S.payload = fillDemoIfEmpty({
        genMs: 0,
        gen: "",
        order: ["wb", "local"],
        sources: {
          wb: { key: "wb", name: "WorkBuddy", ws: ["工作区"], models: [], sess: [] },
          local: { key: "local", name: "小 agent", ws: ["float-agent"], models: [], sess: [] },
        },
      });
    }
    const saved = (() => {
      try {
        return localStorage.getItem("tcd-src");
      } catch (_) {
        return null;
      }
    })();
    S.src = saved && S.payload.sources[saved] ? saved : (S.payload.order || ["wb"])[0];
    try {
      const th = localStorage.getItem("tcd-theme");
      if (th === "light") {
        S.dark = false;
        document.documentElement.dataset.theme = "light";
        $("theme-btn").textContent = "深色";
      } else {
        document.documentElement.dataset.theme = "dark";
        $("theme-btn").textContent = "浅色";
      }
    } catch (_) {}
  }

  // ── 启动 ──
  document.addEventListener("DOMContentLoaded", () => {
    loadPayload();
    bind();
    renderAll();
  });
})();
