/**
 * 指纹注入层：让每个注册窗口在页面 JS 眼里是「另一台机器」。
 *
 * 注入时机是 document-start（早于页面任何脚本），作用域由 bootstrap 提供：
 * `PERSONA` 是 persona.rs 按账号邮箱派生的指纹档案。
 *
 * 三条纪律：
 *  1. **只改原型上的访问器/方法，不往实例上挂东西。**
 *     实测基线 `Object.getOwnPropertyNames(navigator)` 是 `[]`，
 *     往实例上 defineProperty 会被这一行抓出来。
 *  2. **被替换的函数 toString 必须还是 `[native code]`。**
 *     页面一行 `canvas.toDataURL.toString()` 就能识破没做这件事的伪装。
 *  3. **宁可少改，不可改出矛盾。**
 *     没有把握自洽的项（时区、字体、UA 版本）一律保持真实值。
 *
 * 有意保留真实值的项，以及原因：
 *  - `userAgent` / `userAgentData` 的**版本号**：必须与 WebView2 运行时逐位一致，
 *    所以 UA 由 Rust 侧按真实版本生成，这里只把 brands 里的 WebView2 品牌摘掉。
 *  - 时区 / 语言：必须与出口 IP 的地理位置一致。没有 GeoIP 对齐前，
 *    伪造出的「美区时区 + 中国 IP」比不改更显眼。
 *  - 字体列表：改 `document.fonts.check` 会与真实文本排版互相矛盾，收益低于风险。
 */
(function () {
  "use strict";
  var P = (typeof PERSONA === "object" && PERSONA) ? PERSONA : null;
  if (!P) return;

  var applied = [];
  var mark = function (k) { try { applied.push(k); } catch (e) {} };

  // ---- 让被替换的函数看起来仍是原生实现 ----
  var nativeize = (function () {
    var orig = Function.prototype.toString;
    var cache = new WeakMap();
    try {
      Object.defineProperty(Function.prototype, "toString", {
        value: function toString() {
          var v;
          try { v = cache.get(this); } catch (e) { v = undefined; }
          return v === undefined ? orig.call(this) : v;
        },
        writable: true,
        configurable: true,
      });
    } catch (e) {}
    return function (fn, name) {
      try { cache.set(fn, "function " + name + "() { [native code] }"); } catch (e) {}
      return fn;
    };
  })();

  function defGetter(proto, name, fn) {
    try {
      Object.defineProperty(proto, name, {
        get: nativeize(fn, "get " + name),
        configurable: true,
        enumerable: true,
      });
      return true;
    } catch (e) {
      return false;
    }
  }

  function defMethod(proto, name, fn) {
    try {
      Object.defineProperty(proto, name, {
        value: nativeize(fn, name),
        writable: true,
        configurable: true,
        enumerable: false,
      });
      return true;
    } catch (e) {
      return false;
    }
  }

  function defConst(proto, name, value) {
    try {
      Object.defineProperty(proto, name, {
        get: nativeize(function () { return value; }, "get " + name),
        configurable: true,
        enumerable: true,
      });
      return true;
    } catch (e) {
      return false;
    }
  }

  // ============ navigator ============

  if (P.cores) { defConst(Navigator.prototype, "hardwareConcurrency", P.cores) && mark("cores"); }
  if (P.mem) { defConst(Navigator.prototype, "deviceMemory", P.mem) && mark("mem"); }
  if (typeof P.touch === "number") { defConst(Navigator.prototype, "maxTouchPoints", P.touch) && mark("touch"); }
  if (P.platform) { defConst(Navigator.prototype, "platform", P.platform) && mark("platform"); }
  if (P.langs && P.langs.length) {
    defConst(Navigator.prototype, "languages", P.langs.slice()) && mark("langs");
    defConst(Navigator.prototype, "language", P.langs[0]) && mark("lang");
  }

  // ---- userAgentData：摘掉 WebView2 品牌 ----
  //
  // WebView2 的 brands 里第一条就是 `Microsoft Edge WebView2`，这是「这不是
  // 真实浏览器」的直接证据。真实 Edge 只报 `Microsoft Edge` + `Chromium`。
  // 版本号保留真实值，只删品牌项 —— 版本对不上比品牌更显眼。
  (function () {
    var desc = Object.getOwnPropertyDescriptor(Navigator.prototype, "userAgentData");
    if (!desc || !desc.get) return;
    var real;
    try { real = desc.get.call(navigator); } catch (e) { return; }
    if (!real) return;

    var strip = function (list) {
      if (!list || typeof list.length !== "number") return list;
      var out = [];
      for (var i = 0; i < list.length; i++) {
        var b = list[i];
        var brand = b && b.brand ? String(b.brand) : "";
        if (/WebView2/i.test(brand)) continue;
        out.push({ brand: brand, version: String(b.version || "") });
      }
      return out;
    };

    var cleanBrands = strip(real.brands);

    var fake = {
      brands: cleanBrands,
      mobile: false,
      platform: real.platform || "Windows",
      getHighEntropyValues: nativeize(function getHighEntropyValues(hints) {
        var want = hints || [];
        return real.getHighEntropyValues(want).then(function (v) {
          var o = {};
          for (var k in v) { if (Object.prototype.hasOwnProperty.call(v, k)) o[k] = v[k]; }
          if (o.brands) o.brands = strip(o.brands);
          if (o.fullVersionList) o.fullVersionList = strip(o.fullVersionList);
          return o;
        });
      }, "getHighEntropyValues"),
      toJSON: nativeize(function toJSON() {
        return { brands: cleanBrands, mobile: false, platform: fake.platform };
      }, "toJSON"),
    };

    // 真实浏览器里 userAgentData 是同一个对象（引用稳定），
    // 每次访问返回新对象会被 `a === a` 一行测出来。
    defGetter(Navigator.prototype, "userAgentData", function () { return fake; }) && mark("uaData");
  })();

  // ---- chrome.webview：WebView2 专有对象，真实 Edge 没有 ----
  (function () {
    try {
      if (window.chrome && window.chrome.webview) {
        delete window.chrome.webview;
        if (!window.chrome.webview) mark("chromeWebview");
      }
    } catch (e) {}
  })();

  // ============ screen ============

  if (P.screen) {
    var s = P.screen;
    defConst(Screen.prototype, "width", s.w) && mark("screenW");
    defConst(Screen.prototype, "height", s.h);
    defConst(Screen.prototype, "availWidth", s.w);
    defConst(Screen.prototype, "availHeight", s.ah);
    // availLeft/availTop 在多显示器下才非零，单屏恒为 0，保持默认即可。
  }

  // ============ canvas ============

  // 确定性微扰：只动一个像素的一个通道 ±1。
  // 肉眼不可见，但任何基于像素哈希的指纹（canvas / webgl / 字体渲染）都会变。
  (function () {
    var seed = (P.canvasSeed >>> 0) || 1;

    function perturb(canvas) {
      try {
        if (!canvas || !canvas.width || !canvas.height) return;
        var ctx = canvas.getContext && canvas.getContext("2d");
        if (!ctx || !ctx.getImageData || !ctx.putImageData) return;
        var x = seed % canvas.width;
        var y = (seed >>> 8) % canvas.height;
        var img = ctx.getImageData(x, y, 1, 1);
        var d = img.data;
        if (!d || d.length < 4) return;
        // 只碰 RGB，不碰 alpha：改 alpha 会让透明区域出现可见边缘。
        var ch = (seed >>> 16) % 3;
        d[ch] = (d[ch] + 1) & 0xff;
        ctx.putImageData(img, x, y);
      } catch (e) {}
    }

    var origToDataURL = HTMLCanvasElement.prototype.toDataURL;
    defMethod(HTMLCanvasElement.prototype, "toDataURL", function toDataURL() {
      perturb(this);
      return origToDataURL.apply(this, arguments);
    }) && mark("canvas");

    var origToBlob = HTMLCanvasElement.prototype.toBlob;
    if (origToBlob) {
      defMethod(HTMLCanvasElement.prototype, "toBlob", function toBlob() {
        perturb(this);
        return origToBlob.apply(this, arguments);
      });
    }

    var origGetImageData = CanvasRenderingContext2D.prototype.getImageData;
    defMethod(CanvasRenderingContext2D.prototype, "getImageData", function getImageData() {
      var out = origGetImageData.apply(this, arguments);
      try {
        // 同一份种子派生：与 toDataURL 的扰动方向一致，
        // 避免「导出图与读回的像素对不上」这种自相矛盾。
        if (out && out.data && out.data.length >= 4) {
          var ch = (seed >>> 16) % 3;
          out.data[ch] = (out.data[ch] + 1) & 0xff;
        }
      } catch (e) {}
      return out;
    });
  })();

  // ============ webgl ============

  (function () {
    var gpu = P.gpu || {};
    var UNMASKED_VENDOR = 37445;
    var UNMASKED_RENDERER = 37446;

    function patch(proto) {
      if (!proto || !proto.getParameter) return false;
      var orig = proto.getParameter;
      return defMethod(proto, "getParameter", function getParameter(p) {
        if (p === UNMASKED_VENDOR && gpu.vendor) return gpu.vendor;
        if (p === UNMASKED_RENDERER && gpu.renderer) return gpu.renderer;
        return orig.call(this, p);
      });
    }

    var ok = false;
    try { ok = patch(window.WebGLRenderingContext && WebGLRenderingContext.prototype) || ok; } catch (e) {}
    try { ok = patch(window.WebGL2RenderingContext && WebGL2RenderingContext.prototype) || ok; } catch (e) {}
    // getParameter 只改字符串，MAX_TEXTURE_SIZE 之类的真实能力值一律不动：
    // 改了反而会与真实驱动能力对不上。
    if (ok) mark("webgl");
  })();

  // ============ audio ============

  (function () {
    var seed = (P.audioSeed >>> 0) || 1;
    var delta = ((seed % 997) + 1) * 1e-8;

    function patch(proto) {
      if (!proto || !proto.startRendering) return false;
      var orig = proto.startRendering;
      return defMethod(proto, "startRendering", function startRendering() {
        var r = orig.apply(this, arguments);
        if (!r || typeof r.then !== "function") return r;
        return r.then(function (buf) {
          try {
            var d = buf && buf.getChannelData && buf.getChannelData(0);
            if (d && d.length) d[seed % d.length] = d[seed % d.length] + delta;
          } catch (e) {}
          return buf;
        });
      });
    }

    var ok = false;
    try { ok = patch(window.OfflineAudioContext && OfflineAudioContext.prototype) || ok; } catch (e) {}
    try { ok = patch(window.webkitOfflineAudioContext && webkitOfflineAudioContext.prototype) || ok; } catch (e) {}
    if (ok) mark("audio");
  })();

  // ============ webrtc ============

  // `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` 在 WebView2 上
  // **不可靠**：实测该参数确实出现在 msedgewebview2 的命令行里，但页面用一次
  // STUN 仍能拿到本机真实公网 IP（srflx 候选）。多个账号露出同一个真实 IP
  // 等于自报关联，所以这里在 JS 层兜住。
  //
  // 做法是**过滤**而不是禁用：mDNS 主机候选（`xxxx.local`）原样保留 ——
  // 那本来就是浏览器的隐私保护特性，整体看起来就是一个「STUN 打不通的普通用户」。
  // 直接清空 iceServers 更好写，但 `pc.getConfiguration()` 会立刻露馅。
  (function () {
    var Orig = window.RTCPeerConnection || window.webkitRTCPeerConnection;
    if (!Orig || !Orig.prototype) return;

    // 带真实 IP 的候选一律丢掉：srflx / prflx / relay，以及任何明文 IPv4。
    var leaky = function (line) {
      var s = String(line || "");
      if (!s) return false;
      if (/\btyp\s+(srflx|prflx|relay)\b/.test(s)) return true;
      return /(\d{1,3}\.){3}\d{1,3}/.test(s);
    };

    var keepEvent = function (e) {
      // candidate === null 表示收集结束，必须放行，否则页面永远等不到结束。
      if (!e || !e.candidate || !e.candidate.candidate) return true;
      return !leaky(e.candidate.candidate);
    };

    var scrubSdp = function (sdp) {
      var s = String(sdp || "");
      if (!s) return s;
      var nl = s.indexOf("\r\n") >= 0 ? "\r\n" : "\n";
      var parts = s.split(nl);
      var out = [];
      for (var i = 0; i < parts.length; i++) {
        var l = parts[i];
        if (l.indexOf("a=candidate:") === 0 && leaky(l)) continue;
        out.push(l);
      }
      return out.join(nl);
    };

    // ---- 事件路径：onicecandidate 赋值 与 addEventListener("icecandidate") ----
    var origAdd = Orig.prototype.addEventListener;
    if (origAdd) {
      defMethod(Orig.prototype, "addEventListener", function addEventListener(type, fn, opts) {
        if (type === "icecandidate" && typeof fn === "function") {
          var wrapped = nativeize(function (e) {
            if (keepEvent(e)) return fn.call(this, e);
          }, "onicecandidate");
          return origAdd.call(this, type, wrapped, opts);
        }
        return origAdd.call(this, type, fn, opts);
      });
    }

    var iceDesc = Object.getOwnPropertyDescriptor(Orig.prototype, "onicecandidate");
    if (iceDesc && iceDesc.set) {
      // 存原 handler 用 WeakMap，不往实例上挂属性：实例自有属性是干净的。
      var handlers = new WeakMap();
      try {
        Object.defineProperty(Orig.prototype, "onicecandidate", {
          get: nativeize(function () {
            return handlers.get(this) || null;
          }, "get onicecandidate"),
          set: nativeize(function (h) {
            var self = this;
            handlers.set(self, h);
            if (typeof h !== "function") return iceDesc.set.call(self, h);
            return iceDesc.set.call(self, nativeize(function (e) {
              if (keepEvent(e)) return h.call(self, e);
            }, "onicecandidate"));
          }, "set onicecandidate"),
          configurable: true,
        });
      } catch (e) {}
    }

    // ---- SDP 路径：候选同样会出现在 localDescription 里，只堵事件等于没堵 ----
    var sdpProps = ["localDescription", "currentLocalDescription", "pendingLocalDescription"];
    var patched = 0;
    var patchProp = function (name) {
      var d = Object.getOwnPropertyDescriptor(Orig.prototype, name);
      if (!d || !d.get) return;
      try {
        Object.defineProperty(Orig.prototype, name, {
          get: nativeize(function () {
            var desc = d.get.call(this);
            if (!desc || !desc.sdp) return desc;
            var sdp = scrubSdp(desc.sdp);
            // 只在真的改动过才换对象，避免无谓地破坏引用一致性。
            if (sdp === desc.sdp) return desc;
            return { type: desc.type, sdp: sdp };
          }, "get " + name),
          configurable: true,
        });
        patched++;
      } catch (e) {}
    };
    for (var i = 0; i < sdpProps.length; i++) patchProp(sdpProps[i]);
    if (patched) mark("webrtc");
  })();

  // 把生效项交给 driver 上报：注入静默失败时，从 flowlog 能一眼看出是哪一项没落地。
  try { P._applied = applied.join(","); } catch (e) {}
})();
