import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, has, stripErr } from "./i18n.js";
import { ic } from "./icons.js";
import { esc, toast } from "./ui.js";
import { randName } from "./names.js";

const STEPS = {
  register: [
    "open-signup", "signup-form", "await-email", "open-link",
    "complete-signup", "back-to-authorize", "confirm-authorize", "bind-invite", "done",
  ],
  login: ["confirm-authorize", "bind-invite", "done"],
  observe: [],
};

const PAGE_KEYS = {
  "zai-signup": "signup",
  "zai-auth": "zaiAuth",
  "zai-authorize": "zaiAuthorize",
  "zai-other": "zaiOther",
  "bigmodel-login": "bigmodel",
  "zcode-bridge": "bridge",
  other: "other",
};

const MAX_LOGS = 200;

// 取激活链接：每 5 秒查一次，共 6 次；都没取到就判定该账号注册失败。
const LINK_POLL_MS = 5000;
const LINK_POLL_TRIES = 6;
const LINK_MAX_AUTO = 3;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let S = null;
let drawer = null;
let modal = null;
let modalKind = null;
let linkBusy = false;

let onSkip = null;
export function setRegSkip(fn) {
  onSkip = fn;
}

let onAbort = null;
/** 中断整个注册流程（关掉登录窗口 + 停止批量队列）。 */
export function setRegAbort(fn) {
  onAbort = fn;
}

let onFail = null;
/** 取不到激活链接 → 判定该账号注册失败（由上层标记并收尾）。 */
export function setRegFail(fn) {
  onFail = fn;
}

function tr(group, key, params) {
  const k = `${group}.${key}`;
  return has(k) ? t(k, params) : `${key}`;
}

function nowLabel() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function stepsFor(mode) {
  return STEPS[mode] || STEPS.login;
}

export function regActive() {
  return !!S;
}

export function regStart(mode, provider) {
  S = {
    mode,
    provider,
    phase: "",
    note: "",
    logs: [],
    closed: false,
    failed: false,
    startedAt: Date.now(),
    signupEmail: "",
    batchEmail: "",
    linkAuto: 0,
    submittedAt: 0,
  };
  linkBusy = false;
  captchaBusy = false;
  captchaQueued = false;
  pushLog("info", "connected", provider);
  render();
}

export function regBatchSet(email) {
  if (S) S.batchEmail = email || "";
}

export function regClose() {
  closeModal();
  S = null;
  document.body.classList.remove("reg-open");
  if (drawer) {
    drawer.remove();
    drawer = null;
  }
}

export function regEvent(ev) {
  if (!S || !ev || typeof ev !== "object") return;
  switch (ev.kind) {
    case "ready":
      pushLog("info", "connected", ev.provider || S.provider);
      break;
    case "page":
      pushLog("info", "page", tr("reg.page", PAGE_KEYS[ev.page] || "other"));
      break;
    case "phase":
      S.phase = ev.phase;
      if (ev.phase === "unsupported") S.failed = true;
      break;
    case "note":
      S.note = !ev.code || ev.code === "clear" ? "" : tr("reg.note", ev.code, { a: ev.a });
      break;
    case "ask":
      if (ev.ask === "captcha") {
        solveCaptcha();
      } else if (S.batchEmail) {
        if (ev.ask === "signup") autoSignup();
        else autoFetchLink();
      } else if (ev.ask === "link") {
        autoFetchLink();
      } else {
        openAsk(ev.ask);
      }
      break;
    case "log":
      if (ev.code === "submitted") S.submittedAt = Date.now();
      if (ev.code === "finishBlocked") {
        // 网络环境级限流：换号也没用，整条队列都得停下
        pushLog("error", "finishBlocked", ev.a);
        toast(t("reg.finishBlocked"), "err", ev.a || "");
        if (onFail) onFail(`上游限流：${ev.a || ""}`);
        if (onAbort) onAbort();
        return;
      }
      pushLog(ev.level || "info", ev.code, ev.a, ev.b);
      break;
    case "closed":
      S.closed = true;
      S.note = "";
      pushLog("warn", "windowClosed");
      break;
    default:
      return;
  }
  render();
}

function pushLog(level, code, a, b) {
  if (!S) return;
  const text = tr("reg.log", code, { a: a || "", b: b || "" });
  S.logs.push({ level, text, at: nowLabel() });
  if (S.logs.length > MAX_LOGS) S.logs.splice(0, S.logs.length - MAX_LOGS);
}

async function autoSignup() {
  const email = S && S.batchEmail;
  if (!email) return;
  try {
    const r = await invoke("pool_get", { email });
    S.signupEmail = r.email;
    await invoke("reg_input", {
      ask: "signup",
      value: { name: randName(), email: r.email, password: r.password },
    });
    pushLog("info", "batchSignup", r.email);
  } catch (e) {
    pushLog("warn", "batchSignupFail", stripErr(e));
  }
  render();
}

function pickLink(links) {
  const verify = links.find((u) => /verify|token=|confirm|activ|signup/i.test(u));
  return verify || links[0] || "";
}

function isCredentialError(msg) {
  return /invalid_grant|AADSTS70000|AADSTS70008|AADSTS50173|AADSTS7000215/i.test(String(msg || ""));
}

async function autoFetchLink() {
  if (!S || linkBusy) return;
  const email = S.signupEmail;
  if (!email) return startManualLink("noEmail");
  if (S.linkAuto >= LINK_MAX_AUTO) return startManualLink("loop");

  linkBusy = true;
  pushLog("info", "fetchLinkStart", email);
  render();

  const anchor = S.submittedAt || S.startedAt || Date.now();
  const sinceMs = anchor - 60_000;

  let link = "";
  let lastErr = "";
  for (let i = 1; i <= LINK_POLL_TRIES && !S.closed; i++) {
    try {
      const r = await invoke("reg_fetch_link", { email, limit: 15, since: sinceMs });
      const links = (r && r.links) || [];
      if (links.length) {
        link = pickLink(links);
        break;
      }
      const n = Number(r && r.scanned) || 0;
      const subj = String((r && r.newestSubject) || "").trim();
      lastErr = subj ? t("reg.linkNewest", { n, s: subj }) : t("reg.linkEmptyBox", { n });
    } catch (e) {
      // 单次取信失败（IMAP 探测超时 / 冷却 / 网络抖动）不该放弃这一轮：
      // 以前这里直接 break，一次抖动就退化成手动输入，第二个号经常就栽在这。
      lastErr = stripErr(e);
      pushLog("warn", "fetchLinkErr", lastErr);
    }
    if (i < LINK_POLL_TRIES) {
      pushLog("info", "fetchLinkPolling", String(i), String(LINK_POLL_TRIES));
      render();
      await sleep(LINK_POLL_MS);
    }
  }
  linkBusy = false;

  if (!link) return failLink(lastErr || "timeout");

  S.linkAuto += 1;
  try {
    await invoke("reg_input", { ask: "link", value: link });
    pushLog("ok", "fetchLinkFound", link);
  } catch (e) {
    pushLog("warn", "fetchLinkPushFail", stripErr(e));
    failLink(stripErr(e));
  }
  render();
}

/**
 * 6 次都没取到链接 → 判定该账号注册失败。
 * 不再退化成手动输入：失败会被标记进「注册失败」分类，队列继续跑下一个。
 */
function failLink(reason) {
  if (reason) pushLog("warn", "fetchLinkFailed", reason);
  render();
  if (onFail) onFail(reason);
}

let captchaBusy = false;
let captchaQueued = false;

// 单次求解现在很快（内部轮数少、超时短），所以可以多试几轮、间隔也缩短。
const CAPTCHA_MAX_TRIES = 4;
const CAPTCHA_RETRY_MS = 3000;

/**
 * 自动完成注册表单上的滑块验证码。
 * 求解器通过 CDP 挂到同一个 WebView2 上拖动，成功后页面自己变成「验证通过」，
 * driver 的门控随即放行 —— 所以这里只负责触发与回报，不搬运 token。
 */
async function solveCaptcha(tries = 0) {
  if (captchaBusy) {
    // 求解器正在跑，driver 又发来请求：记下来，本轮结束立刻接上，
    // 直接丢弃会让「重试」看起来毫无反应。
    captchaQueued = true;
    return;
  }
  if (!S || S.closed) return;
  captchaBusy = true;
  // 求解期间把状态写在抽屉上：否则用户只看到「开始自动求解」后长时间没动静，
  // 会以为已经放弃了。
  if (tries === 0) {
    pushLog("info", "captchaSolveStart");
    S.note = t("reg.note.solvingCaptcha");
  } else {
    S.note = t("reg.note.solvingCaptchaRetry", { a: tries + 1, b: CAPTCHA_MAX_TRIES });
  }
  render();

  let ok = false;
  try {
    const r = await invoke("captcha_solve");
    ok = !!(r && r.ok);
    if (ok) {
      S.note = "";
      pushLog("ok", "captchaSolved", String(r.attempts ?? ""), String(r.elapsed ?? ""));
    } else {
      pushLog("warn", "captchaSolveFail", String((r && r.error) || ""));
    }
  } catch (e) {
    pushLog("warn", "captchaSolveErr", stripErr(e));
  }

  captchaBusy = false;
  render();

  const needRetry = captchaQueued || (!ok && tries + 1 < CAPTCHA_MAX_TRIES);
  captchaQueued = false;
  if (needRetry && S && !S.closed) {
    if (!ok) {
      pushLog("info", "captchaRetrying", String(tries + 2), String(CAPTCHA_MAX_TRIES));
    }
    setTimeout(() => solveCaptcha(ok ? 0 : tries + 1), CAPTCHA_RETRY_MS);
  }
}

function startManualLink(reason) {
  if (!S) return;
  if (modalKind === "link") return; 
  if (reason) pushLog("warn", "fetchLinkManual", reason);
  render();
  openAsk("link");
}

function modeLabel(mode) {
  return tr("reg.mode", mode);
}

function ensureDrawer() {
  if (!drawer) {
    drawer = document.createElement("aside");
    drawer.className = "drw";
    document.body.appendChild(drawer);
  }
  return drawer;
}

function statusOf() {
  if (S.closed) return { key: "closed", cls: "off" };
  if (S.failed) return { key: "attention", cls: "warn" };
  if (S.phase === "done") return { key: "done", cls: "ok" };
  return { key: "live", cls: "run" };
}

function stepState(step) {
  const list = stepsFor(S.mode);
  const cur = list.indexOf(S.phase);
  const idx = list.indexOf(step);
  if (cur < 0) return "";
  if (idx < cur) return "done";
  if (idx === cur) return "cur";
  return "";
}

function render() {
  if (!S) return;
  const root = ensureDrawer();
  document.body.classList.add("reg-open");
  const list = stepsFor(S.mode);
  const status = statusOf();

  const stepsHtml = list.length
    ? `<div class="drw-steps">${list.map((s) => `
        <span class="drw-step ${stepState(s)}"><i></i>${esc(tr("reg.step", s))}</span>`).join("")}</div>`
    : "";

  const logsHtml = S.logs.map((l) => `
    <div class="drw-line ${esc(l.level)}">
      <span class="at">${esc(l.at)}</span>
      <span class="tx">${esc(l.text)}</span>
    </div>`).join("");

  root.innerHTML = `
    <div class="drw-h">
      <span class="dot ${status.cls}"></span>
      <span class="drw-title">${esc(t("reg.title"))} · ${esc(modeLabel(S.mode))}</span>
      <span class="chip new">${esc(S.provider || "")}</span>
      <span class="spacer"></span>
      <span class="drw-status ${status.cls}">${esc(tr("reg.status", status.key))}</span>
      <button class="iconbtn" data-act="close" title="${esc(t("reg.btn.close"))}" style="color:var(--ink-2)">${ic("x", 14)}</button>
    </div>
    <div class="bar" style="border-bottom:1px solid var(--line);padding:9px 14px;gap:6px">
      ${S.mode === "register" ? "" : `<button class="btn sm" data-act="to-register">${ic("userPlus", 12)} ${esc(t("reg.btn.toRegister"))}</button>`}
      <button class="btn sm" data-act="reveal">${ic("play", 12)} ${esc(t("reg.btn.reveal"))}</button>
      <button class="btn sm" data-act="retry">${ic("refresh", 12)} ${esc(t("reg.btn.retry"))}</button>
      ${S.batchEmail ? `<button class="btn sm" data-act="skip">${ic("x", 12)} ${esc(t("reg.btn.skip"))}</button>` : ""}
      <button class="btn sm d" data-act="abort">${ic("power", 12)} ${esc(t("reg.btn.abort"))}</button>
    </div>
    ${S.note ? `<div class="drw-note">${ic("alert", 14)}<span>${esc(S.note)}</span></div>` : ""}
    ${stepsHtml}
    <div class="drw-log" id="reg-log">${logsHtml || `<div class="drw-line"><span class="tx" style="color:var(--ink-3)">${esc(t("reg.waiting"))}</span></div>`}</div>
    ${S.closed ? `<div style="padding:14px"><button class="btn g" data-act="close">${esc(t("reg.btn.close"))}</button></div>` : ""}
  `;

  const log = root.querySelector("#reg-log");
  if (log) log.scrollTop = log.scrollHeight;

  root.querySelectorAll("[data-act]").forEach((b) => {
    b.addEventListener("click", () => onAction(b.dataset.act));
  });
}

async function onAction(act) {
  if (act === "close") return regClose();
  try {
    if (act === "reveal") await invoke("reg_action", { action: "reveal" });
    else if (act === "abort") {
      // 中断：先关登录窗口，再让上层停掉批量队列（否则下一个号还会被拉起来）
      await invoke("reg_close_window").catch(() => {});
      if (onAbort) onAbort();
      pushLog("warn", "aborted");
      return;
    } else if (act === "retry") {
      S.failed = false;
      await invoke("reg_action", { action: "retry" });
    } else if (act === "skip") {
      if (onSkip) onSkip();
      return; 
    } else if (act === "to-register") {
      const m = await invoke("reg_set_mode", { mode: "register" });
      S.mode = m;
      S.note = "";
      S.logs.push({ level: "info", text: tr("reg.log", "modeSwitch", { a: modeLabel(m) }), at: nowLabel() });
    }
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
  render();
}

function closeModal() {
  if (modal) {
    modal.remove();
    modal = null;
  }
  modalKind = null;
}

function field(id, label, type, placeholder, extra = "") {
  return `
    <label class="fld">
      <span>${esc(label)}</span>
      <input id="${id}" class="inp" type="${type}" placeholder="${esc(placeholder)}"
        autocomplete="off" spellcheck="false" ${extra}>
    </label>`;
}

async function importAccounts(mask) {
  const err = mask.querySelector(".errline");
  const btn = mask.querySelector(".reg-import-btn");
  const sel = mask.querySelector(".reg-acct");
  btn.disabled = true;
  try {
    const r = await invoke("pool_list");
    const list = ((r && r.accounts) || []).filter((a) => a.status !== "verified");
    if (!list.length) {
      err.textContent = t("mb.nothingToVerify");
      return;
    }
    err.textContent = "";
    sel.innerHTML = list
      .map((a) => `<option value="${esc(a.email)}">${esc(a.email)}</option>`)
      .join("");
    sel.hidden = false;
    sel.onchange = () => fillFromPool(mask, sel.value);
    fillFromPool(mask, list[0].email);
  } catch (e) {
    err.textContent = stripErr(e);
  } finally {
    btn.disabled = false;
  }
}

async function fillFromPool(mask, email) {
  try {
    const r = await invoke("pool_get", { email });
    const em = mask.querySelector("#reg-email");
    const pw = mask.querySelector("#reg-pw");
    if (em) em.value = r.email || "";
    if (pw) pw.value = r.password || "";
  } catch (e) {
    const err = mask.querySelector(".errline");
    if (err) err.textContent = stripErr(e);
  }
}

function openAsk(kind) {
  closeModal();
  modalKind = kind;
  const isSignup = kind === "signup";
  const mask = document.createElement("div");
  mask.className = "ov reg bottom";
  mask.innerHTML = `
    <div class="sheet" role="dialog" aria-modal="true">
      <div class="grabber"></div>
      <div class="mhead">${ic(isSignup ? "userPlus" : "mail", 16)} <span>${esc(tr("reg.ask", `${kind}.label`))}</span></div>
      <div class="mbody">
        <div class="mnote" style="margin-bottom:14px">${esc(tr("reg.ask", `${kind}.hint`))}</div>
        ${isSignup
          ? `<div class="importrow">
               <button type="button" class="btn sm reg-import-btn">${ic("import", 13)} ${esc(t("reg.ask.signup.import"))}</button>
               <select class="inp reg-acct" hidden></select>
             </div>`
            + field("reg-name", t("reg.ask.signup.name"), "text", t("reg.ask.signup.namePh"))
            + field("reg-email", t("reg.ask.signup.email"), "email", t("reg.ask.signup.emailPh"))
            + field("reg-pw", t("reg.ask.signup.password"), "password", t("reg.ask.signup.passwordPh"))
          : `<label class="fld">
               <span>${esc(t("reg.ask.link.ph"))}</span>
               <textarea id="reg-link" class="inp" rows="3"
                 placeholder="${esc(t("reg.ask.link.ph"))}" spellcheck="false"></textarea>
             </label>`}
        <div class="errline"></div>
      </div>
      <div class="mfoot">
        <button class="btn g reg-cancel">${esc(t("common.cancel"))}</button>
        <button class="btn p reg-go">${ic("check", 14)} ${esc(t("reg.ask.submit"))}</button>
      </div>
    </div>`;
  document.body.appendChild(mask);
  modal = mask;

  if (isSignup) {
    const nameEl = mask.querySelector("#reg-name");
    if (nameEl && !nameEl.value) nameEl.value = randName();
    const importBtn = mask.querySelector(".reg-import-btn");
    if (importBtn) importBtn.addEventListener("click", () => importAccounts(mask));
  }

  const err = mask.querySelector(".errline");
  const close = () => { closeModal(); };
  mask.addEventListener("click", (e) => { if (e.target === mask) close(); });
  mask.querySelector(".reg-cancel").addEventListener("click", close);

  const first = mask.querySelector("input, textarea");
  if (first) first.focus();

  const submit = async () => {
    let value;
    if (isSignup) {
      const name = mask.querySelector("#reg-name").value.trim();
      const email = mask.querySelector("#reg-email").value.trim();
      const password = mask.querySelector("#reg-pw").value;
      if (!/^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/.test(email)) return (err.textContent = t("reg.err.email"));
      if (password.length < 6) return (err.textContent = t("reg.err.password"));
      S.signupEmail = email;
      value = { name, email, password };
    } else {
      const link = mask.querySelector("#reg-link").value.trim();
      if (!/^https?:\/\//i.test(link)) return (err.textContent = t("reg.err.link"));
      S.linkAuto = LINK_MAX_AUTO;
      value = link;
    }
    const go = mask.querySelector(".reg-go");
    go.disabled = true;
    try {
      await invoke("reg_input", { ask: kind, value });
      closeModal();
      pushLog("info", isSignup ? "submittedSignup" : "submittedLink");
      render();
      if (drawer) drawer.scrollIntoView({ block: "end" });
    } catch (e) {
      err.textContent = String(e).replace(/^[a-z_]+:/, "");
      go.disabled = false;
    }
  };
  mask.querySelector(".reg-go").addEventListener("click", submit);
  mask.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) submit();
    else if (e.key === "Enter" && !isSignup) submit();
  });
}

listen("reg://event", (ev) => regEvent(ev.payload)).catch(() => {});
