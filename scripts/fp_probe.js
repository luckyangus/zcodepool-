(async () => {
  const out = {};
  const safe = (fn) => { try { return fn(); } catch (e) { return "ERR:" + String(e).slice(0, 80); } };

  // ---- navigator ----
  out.nav = {
    ua: navigator.userAgent,
    appVersion: navigator.appVersion,
    platform: navigator.platform,
    vendor: navigator.vendor,
    product: navigator.product,
    productSub: navigator.productSub,
    language: navigator.language,
    languages: navigator.languages,
    hardwareConcurrency: navigator.hardwareConcurrency,
    deviceMemory: navigator.deviceMemory,
    maxTouchPoints: navigator.maxTouchPoints,
    webdriver: navigator.webdriver,
    cookieEnabled: navigator.cookieEnabled,
    doNotTrack: navigator.doNotTrack,
    pdfViewerEnabled: navigator.pdfViewerEnabled,
    plugins: safe(() => [...navigator.plugins].map((p) => p.name)),
    mimeCount: safe(() => navigator.mimeTypes.length),
  };
  out.uaData = safe(() => navigator.userAgentData
    ? { brands: navigator.userAgentData.brands, mobile: navigator.userAgentData.mobile, platform: navigator.userAgentData.platform }
    : null);

  // ---- screen / window ----
  out.screen = safe(() => ({
    w: screen.width, h: screen.height, aw: screen.availWidth, ah: screen.availHeight,
    colorDepth: screen.colorDepth, pixelDepth: screen.pixelDepth,
    isExtended: screen.isExtended,
    orient: screen.orientation && screen.orientation.type,
  }));
  out.window = safe(() => ({
    iw: innerWidth, ih: innerHeight, ow: outerWidth, oh: outerHeight,
    dpr: devicePixelRatio, screenX, screenY, outerScreenX: screenX,
    hasFocus: document.hasFocus(),
  }));

  // ---- locale / tz ----
  out.locale = safe(() => ({
    tz: Intl.DateTimeFormat().resolvedOptions().timeZone,
    tzOffset: new Date().getTimezoneOffset(),
    locale: Intl.DateTimeFormat().resolvedOptions().locale,
    numberingSystem: Intl.DateTimeFormat().resolvedOptions().numberingSystem,
    calendar: Intl.DateTimeFormat().resolvedOptions().calendar,
    nfLocale: Intl.NumberFormat().resolvedOptions().locale,
    dateStr: new Date().toString(),
  }));

  // ---- canvas 2d ----
  const canvasHash = () => {
    const c = document.createElement("canvas");
    c.width = 220; c.height = 30;
    const ctx = c.getContext("2d");
    ctx.textBaseline = "top";
    ctx.font = "14px 'Arial'";
    ctx.fillStyle = "#f60"; ctx.fillRect(0, 0, 220, 30);
    ctx.fillStyle = "#069"; ctx.fillText("ZpoolFP-\u4e2d\u6587-\u{1F600}", 2, 2);
    ctx.fillStyle = "rgba(102,204,0,0.7)"; ctx.fillText("ZpoolFP-\u4e2d\u6587-\u{1F600}", 4, 8);
    ctx.globalCompositeOperation = "multiply";
    ctx.fillStyle = "rgb(255,0,255)"; ctx.beginPath();
    ctx.arc(50, 15, 10, 0, Math.PI * 2); ctx.fill();
    ctx.fillStyle = "rgba(0,0,255,0.5)"; ctx.fillRect(60, 0, 30, 20);
    const url = c.toDataURL();
    let h = 0;
    for (let i = 0; i < url.length; i++) { h = (h * 31 + url.charCodeAt(i)) | 0; }
    return { len: url.length, hash: h, tail: url.slice(-24) };
  };
  out.canvas2d = safe(canvasHash);

  // ---- webgl ----
  out.webgl = safe(() => {
    const c = document.createElement("canvas");
    const gl = c.getContext("webgl") || c.getContext("experimental-webgl");
    if (!gl) return null;
    const dbg = gl.getExtension("WEBGL_debug_renderer_info");
    const p = (n) => { try { return gl.getParameter(n); } catch (e) { return null; } };
    const str = (n) => { try { return String(gl.getParameter(n)).slice(0, 200); } catch (e) { return null; } };
    const glHash = (() => {
      const c2 = document.createElement("canvas");
      c2.width = 64; c2.height = 64;
      const g2 = c2.getContext("webgl");
      if (!g2) return null;
      g2.clearColor(0.2, 0.4, 0.6, 1.0); g2.clear(g2.COLOR_BUFFER_BIT);
      const buf = new Uint8Array(64 * 64 * 4);
      g2.readPixels(0, 0, 64, 64, g2.RGBA, g2.UNSIGNED_BYTE, buf);
      let h = 0; for (let i = 0; i < buf.length; i++) { h = (h * 31 + buf[i]) | 0; }
      return h;
    })();
    return {
      vendor: str(gl.VENDOR), renderer: str(gl.RENDERER),
      unmaskedVendor: dbg ? str(dbg.UNMASKED_VENDOR_WEBGL) : null,
      unmaskedRenderer: dbg ? str(dbg.UNMASKED_RENDERER_WEBGL) : null,
      version: str(gl.VERSION), shading: str(gl.SHADING_LANGUAGE_VERSION),
      maxTexture: p(gl.MAX_TEXTURE_SIZE), maxViewport: p(gl.MAX_VIEWPORT_DIMS),
      maxRenderbuffer: p(gl.MAX_RENDERBUFFER_SIZE), aliasedLine: p(gl.ALIASED_LINE_WIDTH_RANGE),
      extensions: (gl.getSupportedExtensions() || []).length,
      extList: (gl.getSupportedExtensions() || []).slice(0, 12),
      pixelHash: glHash,
    };
  });

  // ---- audio ----
  out.audio = await (async () => {
    try {
      const AC = window.OfflineAudioContext || window.webkitOfflineAudioContext;
      if (!AC) return null;
      const ctx = new AC(1, 44100, 44100);
      const osc = ctx.createOscillator();
      osc.type = "triangle"; osc.frequency.value = 10000;
      const comp = ctx.createDynamicsCompressor();
      comp.threshold.value = -50; comp.knee.value = 40; comp.ratio.value = 12;
      comp.attack.value = 0; comp.release.value = 0.25;
      osc.connect(comp); comp.connect(ctx.destination); osc.start(0);
      const buf = await ctx.startRendering();
      const d = buf.getChannelData(0);
      // 全量累加 + 哈希：注入的微扰只有一个采样点，只采一段区间会漏掉它。
      let sum = 0;
      let h = 0;
      for (let i = 0; i < d.length; i++) {
        sum += Math.abs(d[i]);
        h = (h * 31 + Math.round(d[i] * 1e7)) | 0;
      }
      return { fullSum: sum, hash: h, sampleRate: buf.sampleRate, len: d.length };
    } catch (e) { return "ERR:" + String(e).slice(0, 80); }
  })();

  // ---- fonts ----
  out.fonts = safe(() => {
    const probe = ["Arial", "Calibri", "Cambria", "Consolas", "Courier New", "Georgia",
      "Microsoft YaHei", "\u5b8b\u4f53", "\u5fae\u8f6f\u96c5\u9ed1", "SimSun", "Segoe UI",
      "Tahoma", "Times New Roman", "Verdana", "Wingdings", "Comic Sans MS", "Impact",
      "Trebuchet MS", "Palatino Linotype", "MS Gothic", "Malgun Gothic", "Nirmala UI"];
    const base = ["monospace", "sans-serif", "serif"];
    const span = document.createElement("span");
    span.style.cssText = "position:absolute;left:-9999px;font-size:72px;white-space:nowrap;";
    span.textContent = "mmmmmmmmmmlli";
    document.body.appendChild(span);
    const def = {};
    for (const b of base) { span.style.fontFamily = b; def[b] = span.offsetWidth; }
    const found = [];
    for (const f of probe) {
      for (const b of base) {
        span.style.fontFamily = `'${f}',${b}`;
        if (span.offsetWidth !== def[b]) { found.push(f); break; }
      }
    }
    span.remove();
    return found;
  });

  // ---- 自动化痕迹 ----
  out.traces = safe(() => {
    const keys = Object.keys(window).filter((k) => /tauri|zpool|TAURI|ZPOOL|cdc_|webdriver|selenium|puppeteer|playwright/i.test(k));
    return {
      tauriInternals: typeof window.__TAURI_INTERNALS__,
      tauriInternalsKeys: window.__TAURI_INTERNALS__ ? Object.keys(window.__TAURI_INTERNALS__) : null,
      tauriInternalsProps: window.__TAURI_INTERNALS__ ? Object.getOwnPropertyNames(window.__TAURI_INTERNALS__) : null,
      tauriGlobal: typeof window.__TAURI__,
      zpoolCfg: window.__ZPOOL_CFG ? Object.keys(window.__ZPOOL_CFG) : null,
      zpool: typeof window.__zpool,
      zpoolKeys: window.__zpool ? Object.keys(window.__zpool) : null,
      chromeObj: typeof window.chrome === "object" ? Object.keys(window.chrome) : String(typeof window.chrome),
      suspiciousWindowKeys: keys,
      iframes: document.querySelectorAll("iframe").length,
      navProtoKeys: Object.getOwnPropertyNames(Object.getPrototypeOf(navigator)).length,
      permQuery: typeof navigator.permissions,
    };
  });

  // ---- 子 frame 指纹（验证注入是否覆盖 iframe；风控常在 iframe 里采样） ----
  out.iframeFp = await new Promise((res) => {
    let done = false;
    const finish = (v) => { if (!done) { done = true; res(v); } };
    try {
      const f = document.createElement("iframe");
      f.style.cssText = "position:absolute;left:-9999px;width:10px;height:10px";
      f.onload = () => {
        try {
          const w = f.contentWindow;
          finish({
            cores: w.navigator.hardwareConcurrency,
            mem: w.navigator.deviceMemory,
            touch: w.navigator.maxTouchPoints,
            screenW: w.screen.width,
            screenH: w.screen.height,
            ua: w.navigator.userAgent.slice(-30),
            uaBrands: w.navigator.userAgentData ? w.navigator.userAgentData.brands.map((b) => b.brand) : null,
            hasTauri: typeof w.__TAURI_INTERNALS__,
            isTauri: w.isTauri,
          });
        } catch (e) { finish("ERR:" + String(e).slice(0, 80)); }
      };
      document.body.appendChild(f);
      setTimeout(() => finish("timeout"), 2500);
    } catch (e) { finish("ERR:" + String(e).slice(0, 80)); }
  });

  // ---- CDP / 调试端口可探测性 ----
  out.cdpProbe = await (async () => {
    const res = {};
    for (const port of [9222, 9223]) {
      try {
        const r = await fetch(`http://127.0.0.1:${port}/json/version`, { mode: "no-cors" });
        res[port] = "reachable:" + r.type;
      } catch (e) { res[port] = "blocked:" + String(e).slice(0, 60); }
    }
    return res;
  })();

  // ---- WebRTC 泄露 ----
  out.webrtc = await (async () => {
    try {
      const PC = window.RTCPeerConnection || window.webkitRTCPeerConnection;
      if (!PC) return null;
      const pc = new PC({ iceServers: [{ urls: "stun:stun.l.google.com:19302" }] });
      const cands = [];
      pc.createDataChannel("x");
      const done = new Promise((res) => {
        pc.onicecandidate = (e) => {
          if (!e.candidate) return res(cands);
          cands.push(e.candidate.candidate);
          setTimeout(() => res(cands), 2500);
        };
        setTimeout(() => res(cands), 4000);
      });
      await pc.setLocalDescription(await pc.createOffer());
      const list = await done;
      pc.close();
      return list;
    } catch (e) { return "ERR:" + String(e).slice(0, 100); }
  })();

  // ---- 硬件/媒体设备 ----
  out.media = await (async () => {
    try {
      if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return null;
      const ds = await navigator.mediaDevices.enumerateDevices();
      return ds.map((d) => d.kind + ":" + (d.label ? d.label.slice(0, 40) : "(no-label)"));
    } catch (e) { return "ERR:" + String(e).slice(0, 80); }
  })();

  // ---- 电池 / 存储 / 其他 ----
  out.misc = safe(() => ({
    storageQuota: typeof navigator.storage,
    connection: navigator.connection ? {
      effectiveType: navigator.connection.effectiveType, rtt: navigator.connection.rtt,
      downlink: navigator.connection.downlink, saveData: navigator.connection.saveData,
    } : null,
    getBattery: typeof navigator.getBattery,
    gpu: typeof navigator.gpu,
    geolocation: typeof navigator.geolocation,
    userAgentDataHighEntropy: typeof (navigator.userAgentData && navigator.userAgentData.getHighEntropyValues),
    timeOrigin: Math.round(performance.timeOrigin),
    url: location.href,
    title: document.title,
  }));

  return JSON.stringify(out, null, 1);
})()
