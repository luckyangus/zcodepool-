import { invoke } from "@tauri-apps/api/core";
import { t, stripErr } from "./i18n.js";
import { ic } from "./icons.js";
import { esc, toast } from "./ui.js";
import { mboxVerify, mboxBatchInfo } from "./mbox.js";

/**
 * 待注册账号。
 *
 * 与「邮箱库」分开存（pending.json / mail.json）：邮箱库是取信源，
 * 待注册账号是准备拿去跑注册的通用邮箱（`email:password` 导入）。
 * 注册引擎仍复用邮箱库那套批量流程，只是从这里发起。
 */
const P = { list: [], sel: new Set(), filter: "all" };
let rerender = () => {};

export function setPendingRerender(fn) {
  rerender = fn;
}

/** 按状态分类，供页面上的 TAB 切换用。 */
function filtered() {
  if (P.filter === "new") return P.list.filter((a) => a.status === "new");
  if (P.filter === "verified") return P.list.filter((a) => a.status === "verified");
  if (P.filter === "failed") {
    return P.list.filter((a) => a.status === "failed" || a.status === "invalid");
  }
  return P.list;
}

export function pendingFilter(f) {
  P.filter = f;
  rerender();
}

export function pendingStats() {
  return {
    total: P.list.length,
    pending: P.list.filter((a) => a.status === "new").length,
    done: P.list.filter((a) => a.status === "verified").length,
    failed: P.list.filter((a) => a.status === "failed" || a.status === "invalid").length,
  };
}

export async function pendingLoad() {
  try {
    const r = await invoke("pending_list");
    P.list = (r && r.accounts) || [];
  } catch (e) {
    P.list = [];
  }
  const alive = new Set(P.list.map((a) => a.email));
  P.sel = new Set([...P.sel].filter((e) => alive.has(e)));
}

function badge(a) {
  const map = {
    new: ["new", "new"],
    verified: ["ok", "verified"],
    failed: ["err", "failed"],
    invalid: ["warn", "invalid"],
  };
  const [cls, key] = map[a.status] || ["new", "new"];
  const note = a.note ? ` title="${esc(a.note)}"` : "";
  return `<span class="chip ${cls}"${note}>${esc(t(`pd.status.${key}`))}</span>`;
}

export function pendingPage() {
  const s = pendingStats();
  const b = mboxBatchInfo();
  const progress = b.running
    ? `<div class="prog">
         <div class="prog-bar"><i style="width:${b.total ? Math.round((b.done / b.total) * 100) : 0}%"></i></div>
         <div class="prog-txt">${esc(t("mb.batch.progress", { done: b.done, total: b.total }))}${b.current ? ` · ${esc(b.current)}` : ""}${b.stage ? ` · ${esc(t(`mb.batch.stage.${b.stage}`))}` : ""}</div>
       </div>`
    : "";
  const rows = filtered();
  const listHtml = rows.length
    ? rows
        .map((a) => {
          // 失败的显示原因，成功的显示注册时间 —— 一行里要能看出「为什么没成」
          const meta =
            a.status === "failed" || a.status === "invalid"
              ? a.note || ""
              : a.verified_at || a.created_at || "";
          return `
      <div class="row" data-email="${esc(a.email)}">
        <input type="checkbox" class="cbx pd-check" aria-label="${esc(a.email)}" data-email="${esc(a.email)}" ${P.sel.has(a.email) ? "checked" : ""}>
        <span class="mail" title="${esc(a.email)}">${esc(a.email)}</span>
        <span class="chip" style="font-size:10px;padding:1px 6px" title="${esc(a.imap_host || t("pd.imapAuto"))}">${esc(a.imap_host || t("pd.imapAuto"))}</span>
        ${badge(a)}
        <span class="when" title="${esc(meta)}">${esc(meta)}</span>
        <button class="iconbtn pd-del" data-del="${esc(a.email)}" title="${esc(t("pd.remove"))}">${ic("x", 14)}</button>
      </div>`;
        })
        .join("")
    : `<div class="empty">
         <div class="glyph">${ic("empty", 34)}</div>
         <div>${esc(t("pd.empty"))}</div>
         <div class="fmt">
           <div class="fmt-hint">${esc(t("pd.formatHint"))}</div>
           <code>email:password</code>
         </div>
       </div>`;

  return `
    <div class="content-h">
      <h1>${t("pd.title")}</h1>
      <span class="sub">${t("m.count", { count: P.list.length })}</span>
      <span class="spacer"></span>
      <div class="seg pd-filter" role="group" aria-label="${t("pd.title")}">
        ${["all", "new", "verified", "failed"]
          .map((f) => `<button class="${P.filter === f ? "on" : ""}" aria-pressed="${P.filter === f}" click="actions.pendingFilter('${f}')">${esc(t(`pd.filter.${f}`))}</button>`)
          .join("")}
      </div>
    </div>
    <dl class="mail-overview">
      <div><dt>${t("pd.overview.total")}</dt><dd>${s.total}</dd></div>
      <div><dt>${t("pd.status.new")}</dt><dd>${s.pending}</dd></div>
      <div class="verified"><dt>${t("pd.status.verified")}</dt><dd>${s.done}</dd></div>
      <div class="attention"><dt>${t("pd.filter.failed")}</dt><dd>${s.failed}</dd></div>
    </dl>
    ${progress}
    <section class="toolbar">
      <button class="btn p" click="actions.pendingImport()">${ic("import", 15)} ${t("pd.import")}</button>
      <button class="btn" click="actions.pasteImport()">${ic("pen", 14)} ${t("pd.paste")}</button>
      <button class="btn" click="actions.blockedOpen()">${ic("alert", 14)} ${t("pd.blocked")}</button>
      <button class="btn" click="actions.pendingStart()" ${P.list.length ? "" : "disabled"}>${ic("play", 14)} ${t("pd.start")}</button>
      <span class="spacer"></span>
      <button class="btn d" click="actions.pendingDelete()" ${P.sel.size ? "" : "disabled"}>${ic("trash", 14)} ${t("pd.delete")}${P.sel.size ? ` (${P.sel.size})` : ""}</button>
    </section>
    <div class="list">${listHtml}</div>`;
}

export function pendingToggle(email, on) {
  if (on) P.sel.add(email);
  else P.sel.delete(email);
}

export function pendingToggleRow(email) {
  pendingToggle(email, !P.sel.has(email));
}

export function pendingSelectAll(on) {
  if (on) P.list.forEach((a) => P.sel.add(a.email));
  else P.sel.clear();
}

export function pendingSelection() {
  return [...P.sel];
}

export async function pendingRemove(email) {
  try {
    await invoke("pending_remove", { email });
    await pendingLoad();
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

export async function pendingDelete(emails) {
  const list = emails && emails.length ? emails : pendingSelection();
  if (!list.length) return;
  try {
    await invoke("pending_remove_many", { emails: list });
    P.sel.clear();
    await pendingLoad();
  } catch (e) {
    toast(String(e).replace(/^[a-z_]+:/, ""), "err");
  }
}

/**
 * 从待注册列表发起批量注册。
 *
 * 不勾选：只跑「新注册」状态的号 —— 失败/无效的号不再自动重跑，
 * 否则每点一次开始，上轮验证码没过的号就白白再烧一遍请求和求解次数。
 * 想重跑失败的：勾选它们再点开始（勾选原样透传）。
 */
export async function pendingStart() {
  const emails = P.sel.size
    ? [...P.sel]
    : P.list.filter((a) => a.status === "new").map((a) => a.email);
  if (!emails.length) {
    toast(t("pd.nothingToRegister"), "warn");
    return;
  }
  await mboxVerify(emails);
}

/**
 * 域名黑名单管理。
 * 命中名单的邮箱在**预检阶段**就被判失败，不会打开登录窗口、不会过验证码。
 */
export async function openBlockedModal() {
  document.querySelector(".ov.blocked")?.remove();
  let data = { builtin: [], custom: [] };
  try {
    data = await invoke("blocked_list");
  } catch (e) {
    toast(stripErr(e), "err");
    return;
  }

  const ov = document.createElement("div");
  ov.className = "ov blocked";
  ov.innerHTML = `
    <div class="modal wide" role="dialog" aria-modal="true">
      <div class="mhead">${ic("alert", 16)} <span>${esc(t("bl.title"))}</span>
        <span class="spacer"></span>
        <button class="iconbtn bl-x" style="color:var(--label-2)">${ic("x", 15)}</button>
      </div>
      <div class="mbody">
        <div class="mnote" style="margin-bottom:12px">${esc(t("bl.hint"))}</div>
        <div class="importrow">
          <input class="inp bl-input" placeholder="${esc(t("bl.placeholder"))}" spellcheck="false" autocomplete="off">
          <button class="btn p bl-add">${ic("check", 14)} ${esc(t("bl.add"))}</button>
        </div>
        <div class="errline"></div>
        <div class="bl-list" style="margin-top:12px"></div>
        ${
          (data.builtin || []).length
            ? `<div class="mnote" style="margin-top:16px">${esc(t("bl.builtinHint", { n: data.builtin.length }))}</div>
               <div class="bl-builtin" style="margin-top:6px;color:var(--ink-3);font-size:11px;line-height:1.9">
                 ${data.builtin.map((d) => `<code style="margin-right:8px">${esc(d)}</code>`).join("")}
               </div>`
            : ""
        }
      </div>
      <div class="mfoot">
        <button class="btn g bl-close">${esc(t("common.close"))}</button>
      </div>
    </div>`;
  document.body.appendChild(ov);

  const err = ov.querySelector(".errline");
  const renderCustom = (list) => {
    const el = ov.querySelector(".bl-list");
    el.innerHTML = (list || []).length
      ? list
          .map(
            (d) => `<div class="row">
              <span class="mail" title="${esc(d)}">${esc(d)}</span>
              <button class="iconbtn bl-del" data-d="${esc(d)}" title="${esc(t("pd.remove"))}">${ic("x", 14)}</button>
            </div>`
          )
          .join("")
      : `<div class="mnote">${esc(t("bl.empty"))}</div>`;
    el.querySelectorAll(".bl-del").forEach((b) => {
      b.addEventListener("click", async () => {
        try {
          const r = await invoke("blocked_remove", { domain: b.dataset.d });
          renderCustom(r.custom);
        } catch (e) {
          err.textContent = stripErr(e);
        }
      });
    });
  };
  renderCustom(data.custom);

  const add = async () => {
    const inp = ov.querySelector(".bl-input");
    const v = inp.value.trim();
    if (!v) return;
    try {
      const r = await invoke("blocked_add", { domain: v });
      inp.value = "";
      err.textContent = "";
      renderCustom(r.custom);
      toast(t("bl.added", { domain: r.domain }), "ok");
    } catch (e) {
      err.textContent = stripErr(e);
    }
  };

  ov.querySelector(".bl-add").addEventListener("click", add);
  ov.querySelector(".bl-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter") add();
  });
  const close = () => ov.remove();
  ov.querySelector(".bl-x").addEventListener("click", close);
  ov.querySelector(".bl-close").addEventListener("click", close);
  ov.addEventListener("click", (e) => {
    if (e.target === ov) close();
  });
  ov.querySelector(".modal").addEventListener("click", (e) => e.stopPropagation());
  setTimeout(() => ov.querySelector(".bl-input").focus(), 30);
}
