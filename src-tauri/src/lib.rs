pub mod cli;
pub mod i18n;
mod captcha;
mod claim;
mod driver;
mod flowlog;
mod gateway;
mod graph;
mod imap;
mod oauth;
mod persona;
mod pool;
mod prompt;
mod quota;
mod quota_cache;
mod store;
mod usage;
mod zcrypto;

use serde_json::{json, Value};
use std::sync::Mutex;
use store::*;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::AutoLaunchManager;
use tauri_plugin_dialog::DialogExt;

const TRAY_ID: &str = "main";

/// 登录窗口的 WebView2 附加参数。
///
/// 登录窗口有独立的 `data_directory`，因此会另起一个 WebView2 环境；
/// 调试端口是按环境绑的，所以它**不能**沿用主窗口的端口，必须用自己的，
/// 否则第二个环境绑不上，验证码求解器就看不到注册窗口（踩过）。
/// 这里也刻意不带 `--no-proxy-server`：注册请求要能走用户配置的代理。
///
/// **调试端口不写在这里**：固定端口本身就是可探测的自动化特征 ——
/// 实测在注册页里 `fetch('http://127.0.0.1:9222/json/version')` 能打通
/// （页面 CSP 不拦 localhost），风控扫一眼就知道这台机器开着 CDP。
/// 端口改成每次注册现取一个高位空闲端口，见 [`pick_cdp_port`]。
///
/// `--force-webrtc-ip-handling-policy=disable_non_proxied_udp`：
/// 实测未加之前，页面用一次 STUN 就能拿到本机**真实公网 IP**（srflx 候选），
/// 配了代理也照样泄露 —— 多个账号露出同一个真实 IP 等于自报关联。
const LOGIN_WEBVIEW_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows --force-webrtc-ip-handling-policy=disable_non_proxied_udp";

/// 当前登录窗口的 CDP 端口。每次注册重新随机，求解器从这里取。
static LOGIN_CDP_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// 取一个可用的高位 CDP 端口。
///
/// 避开 9222/9223 这类被风控写进黑名单的 Chromium 默认端口。绑定一次再释放
/// 是「探测可用性」的常规做法，释放到 WebView2 真正绑定之间的窗口极窄，
/// 抢不到就退回 0，由调用方按无端口处理。
fn pick_cdp_port() -> u16 {
    let seed = uuid::Uuid::new_v4().as_u128();
    for i in 0..64u128 {
        let port = (21000 + (seed.wrapping_add(i.wrapping_mul(9779)) % 20000)) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    0
}

static STORE_LOCK: Mutex<()> = Mutex::new(());

struct PendingClaim {
    account_id: String,
    account_name: String,
    plan_id: String,
    plan_name: String,
    credentials: Value,
    config: Option<Value>,
    device_mid: String,
}

static PENDING_CLAIM: Mutex<Option<PendingClaim>> = Mutex::new(None);

fn pending_guard() -> std::sync::MutexGuard<'static, Option<PendingClaim>> {
    match PENDING_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    match STORE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn tray_menu_inner(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let paths = Paths::detect();
    let state = match store::get_state(&paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tray state error: {e}");
            return MenuBuilder::new(app)
                .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
                .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?)
                .build();
        }
    };

    let b = MenuBuilder::new(app)
        .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
        .item(&MenuItem::with_id(app, "capture", &i18n::tr("tray.capture"), state.live_logged_in, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "launch", &i18n::tr("tray.launch"), state.zcode_path_ok && !state.zcode_running, None::<&str>)?)
        .item(&MenuItem::with_id(app, "kill", &i18n::tr("tray.kill"), state.zcode_running, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?);
    b.build()
}

pub fn rebuild_tray(app: &AppHandle) {
    let app2 = app.clone();
    let res = app.run_on_main_thread(move || {
        if let Some(tray) = app2.tray_by_id(TRAY_ID) {
            let paths = Paths::detect();
            let tip = match store::get_state(&paths) {
                Ok(s) => {
                    let cur = s
                        .accounts
                        .iter()
                        .find(|a| a.is_active)
                        .map(|a| a.name.clone())
                        .or_else(|| s.live_identity.as_ref().and_then(|i| i.label()))
                        .unwrap_or_else(|| if s.live_logged_in { i18n::tr("tray.unsaved") } else { i18n::tr("tray.logged_out") });
                    format!("Z·POOL · {cur}")
                }
                Err(_) => "Z·POOL".into(),
            };
            let _ = tray.set_tooltip(Some(&tip));
            if let Ok(menu) = tray_menu_inner(&app2) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
    if let Err(e) = res {
        eprintln!("rebuild_tray: {e}");
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn run_tray_action(app: AppHandle, action: String) {
    std::thread::spawn(move || {
        let paths = Paths::detect();
        let mut emit_payload = json!({ "action": action });
        let _guard = store_guard();
        let result: Result<serde_json::Value, String> = match action.as_str() {
            "capture" => store::capture_current(&paths, None).map(|a| json!({ "name": a.name })),
            "launch" => {
                let (p, ok) = effective_zcode_path(&paths);
                if ok { store::launch_zcode(&p).map(|_| json!({})) } else { Err(i18n::trf("err.zcode.path_invalid", &[("p", &p)])) }
            }
            "kill" => match store::kill_zcode() {
                Ok(true) => Ok(json!({})),
                Ok(false) => Err(i18n::tr("err.zcode.kill_timeout")),
                Err(e) => Err(e),
            },
            _ => Ok(json!({})),
        };
        match result {
            Ok(v) => {
                emit_payload["ok"] = json!(true);
                emit_payload["result"] = v;
            }
            Err(e) => {
                emit_payload["ok"] = json!(false);
                emit_payload["error"] = json!(e);
            }
        }
        let _ = app.emit("tray-action", &emit_payload);
        rebuild_tray(&app);
    });
}

#[tauri::command]
async fn get_state() -> Result<AppState, String> {
    store::get_state(&Paths::detect())
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn capture_current(app: AppHandle, name: Option<String>) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::capture_current(&Paths::detect(), name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn rename_account(app: AppHandle, id: String, name: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::rename_account(&Paths::detect(), &id, &name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn delete_account(app: AppHandle, id: String) -> Result<(), String> {
    let _guard = store_guard();
    let r = store::delete_account(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn update_account_from_live(app: AppHandle, id: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::update_account_from_live(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn switch_to(app: AppHandle, id: String, force: bool, restart: bool) -> Result<SwitchResult, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let r = store::switch_to(&paths, &id, force, restart);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn get_live_quota(force: Option<bool>) -> Result<quota::QuotaOverview, String> {
    store::live_quota(&Paths::detect(), force.unwrap_or(false))
}

/// 账号额度。`force=true` 跳过盘上快照（手动刷新、领取套餐后必须走这条）。
#[tauri::command]
async fn get_account_quota(id: String, force: Option<bool>) -> Result<quota::QuotaOverview, String> {
    store::account_quota(&Paths::detect(), &id, force.unwrap_or(false))
}

#[tauri::command]
async fn claim_preview(id: String) -> Result<Vec<claim::ClaimPlan>, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRefreshResult {
    plans: Vec<claim::ClaimPlan>,
    activated: bool,
    activation_error: Option<String>,
}

#[tauri::command]
async fn claim_refresh(id: String) -> Result<ClaimRefreshResult, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    let (activated, activation_error) =
        match claim::telemetry_user_id(&paths.home, &acc.credentials) {
            Some(uid) => match claim::report_activation_events(&uid, &mid) {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e)),
            },
            None => (false, None),
        };
    let plans = claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid))?;
    Ok(ClaimRefreshResult { plans, activated, activation_error })
}

#[tauri::command]
async fn claim_start(
    app: AppHandle,
    id: String,
    plan_id: String,
    auto: Option<bool>,
) -> Result<serde_json::Value, String> {
    claim_start_inner(&app, id, plan_id, auto.unwrap_or(false))
}

pub fn proxy_claim_start(
    app: &AppHandle,
    id: String,
    plan_id: String,
    auto: bool,
) -> Result<serde_json::Value, String> {
    claim_start_inner(app, id, plan_id, auto)
}

fn claim_start_inner(
    app: &AppHandle,
    id: String,
    plan_id: String,
    auto: bool,
) -> Result<serde_json::Value, String> {
    let paths = Paths::detect();
    let mid = store::account_mid(&paths, &id)?;
    let acc = load_account(&paths, &id)?;
    let plans = claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid.clone()))?;
    let plan = plans
        .iter()
        .find(|p| p.plan_id == plan_id)
        .ok_or_else(|| i18n::tr("err.claim.gone"))?;
    let display = if plan.name.is_empty() { plan.plan_id.clone() } else { plan.name.clone() };

    *pending_guard() = Some(PendingClaim {
        account_id: acc.id.clone(),
        account_name: acc.name.clone(),
        plan_id: plan.plan_id.clone(),
        plan_name: display.clone(),
        credentials: acc.credentials,
        config: acc.config,
        device_mid: mid,
    });
    open_captcha_window(app, auto)?;
    Ok(json!({ "account": acc.name, "plan": display, "captcha": true }))
}

static LAST_CLAIM: Mutex<Option<Value>> = Mutex::new(None);

fn last_claim_guard() -> std::sync::MutexGuard<'static, Option<Value>> {
    match LAST_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub fn claim_last_result() -> Value {
    last_claim_guard().clone().unwrap_or(Value::Null)
}

fn record_claim_result(payload: &Value) {
    // ⚠ `at` 是给人看的字符串；前端要**比时间先后**，字符串比不了 ——
    //   它和 unix 秒数相减会得到 NaN，判断恒为 false，成功分支就永远进不去
    //   （症状：领取成功了却一直显示「正在领取中」然后超时）。所以另给一个数值 `atMs`。
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    *last_claim_guard() = Some(json!({ "at": store::now_ts(), "atMs": ms, "result": payload }));
}

pub fn emit_state_changed(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
}

#[tauri::command]
async fn claim_captcha_config() -> Result<claim::CaptchaConfig, String> {
    claim::fetch_captcha_config()
}

#[tauri::command]
async fn claim_captcha_submit(
    app: AppHandle,
    param: String,
    region: Option<String>,
) -> Result<serde_json::Value, String> {
    let pending = pending_guard().take().ok_or_else(|| i18n::tr("err.claim.none_pending"))?;
    let paths = Paths::detect();
    let res = claim::submit_claim(
        &paths.home,
        &pending.credentials,
        pending.config.as_ref(),
        &pending.plan_id,
        &param,
        region.as_deref(),
        Some(pending.device_mid),
    );
    close_captcha_window(&app);
    let payload = match res {
        Ok(v) => {
            let ms = |k: &str| -> Option<i64> {
                v.pointer(&format!("/data/plan/{k}"))
                    .and_then(|x| x.as_i64())
                    .map(|s| s * 1000)
            };
            let server_time = v
                .pointer("/data/server_time")
                .and_then(|x| x.as_i64())
                .map(|s| s * 1000);
            let outcome = claim::ClaimOutcome {
                account_id: pending.account_id.clone(),
                account_name: pending.account_name.clone(),
                plan_name: pending.plan_name.clone(),
                starts_at: ms("starts_at"),
                ends_at: ms("ends_at"),
                server_time,
            };
            let p = serde_json::to_value(&outcome).unwrap_or(Value::Null);
            record_claim_result(&p);
            let _ = app.emit("claim://result", &p);
            p
        }
        Err(e) => {
            let p = claim::failure_payload(
                &pending.account_id,
                &pending.account_name,
                &pending.plan_name,
                &e,
            );
            record_claim_result(&p);
            let _ = app.emit("claim://result", &p);
            return Ok(p);
        }
    };
    Ok(payload)
}

#[tauri::command]
async fn claim_cancel(app: AppHandle) -> Result<(), String> {
    *pending_guard() = None;
    close_captcha_window(&app);
    Ok(())
}

fn close_captcha_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("captcha") {
        let _ = w.close();
    }
}

struct PendingOAuth {
    provider: String,
    state: String,
    flow: String,
}

#[derive(Clone)]
struct PollCfg {
    url: String,
    token: String,
    expires_at_ms: u128,
    interval_ms: u64,
}

static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);

fn pending_oauth_guard() -> std::sync::MutexGuard<'static, Option<PendingOAuth>> {
    match PENDING_OAUTH.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[tauri::command]
async fn oauth_providers() -> Result<Vec<oauth::OAuthProvider>, String> {
    Ok(oauth::OAUTH_PROVIDERS.to_vec())
}

#[tauri::command]
async fn oauth_begin(
    app: AppHandle,
    provider: String,
    mode: Option<String>,
    batch: Option<bool>,
) -> Result<serde_json::Value, String> {
    if !oauth::OAUTH_PROVIDERS.iter().any(|p| p.id == provider) {
        return Err(i18n::trf("err.oauth.unknown_provider", &[("provider", &provider)]));
    }
    let assist_mode = driver::normalize_mode(mode.as_deref());
    let auto = batch.unwrap_or(false);
    let mut proxy_str = String::new();
    let proxy_url: Option<tauri::Url> = match load_settings(&Paths::detect()).auth_proxy() {
        Some(p) => {
            let norm = oauth::parse_proxy_url(p)?;
            proxy_str = norm.clone();
            Some(norm.parse::<tauri::Url>().map_err(|e| i18n::trf("err.proxy.invalid", &[("e", &e.to_string())]))?)
        }
        None => None,
    };
    *pending_oauth_guard() = None;
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.close();
    }
    let flow = uuid::Uuid::new_v4().to_string();
    flowlog::log(&flow, "begin", &format!("provider={provider} auto={auto} proxy={} client_ver={}", if proxy_url.is_some() { "on" } else { "off" }, quota::zcode_app_version()));
    let mid = uuid::Uuid::new_v4().to_string();
    // 指纹档案的种子用 mid：它与最终写进账号的 `virtual_device_mid` 是同一个值，
    // 「账号 ↔ 设备指纹」因此天然绑定，补注册 / 重登不会拿到另一套指纹。
    let salt = Paths::detect().home.to_string_lossy().to_string();
    let persona = persona::derive(&mid, &salt);
    let (p_init, m_init) = (provider.clone(), mid.clone());
    let init = match tauri::async_runtime::spawn_blocking(move || oauth::init_flow(&p_init, &m_init))
        .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            flowlog::log(&flow, "init-fail", &e);
            return Err(e);
        }
        Err(e) => {
            let m = i18n::trf("err.oauth.flow", &[("e", &e.to_string())]);
            flowlog::log(&flow, "init-fail", &m);
            return Err(m);
        }
    };
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let srv_flow = init.poll_url.rsplit('/').next().unwrap_or("");
        let expires_in = init.expires_at_ms.saturating_sub(now) / 1000;
        flowlog::log(
            &flow,
            "init-ok",
            &format!("server_flow={srv_flow} expires_in={expires_in}s interval={}ms", init.poll_interval_ms),
        );
    }
    let url = init.authorize_url.clone();
    let state = init.state.clone();
    let poll_cfg = PollCfg {
        url: init.poll_url.clone(),
        token: init.poll_token.clone(),
        expires_at_ms: init.expires_at_ms,
        interval_ms: init.poll_interval_ms,
    };
    *pending_oauth_guard() = Some(PendingOAuth {
        provider: provider.clone(),
        state: state.clone(),
        flow: flow.clone(),
    });

    let login_root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| i18n::trf("err.oauth.appdata", &[("e", &e.to_string())]))?
        .join("login-webview");
    sweep_login_profiles(&login_root);
    let profile_dir = login_root.join(&flow);

    let app2 = app.clone();
    let (provider2, state2, flow2, mid2) = (provider.clone(), state.clone(), flow.clone(), mid.clone());
    let flow_close = flow.clone();
    let driver_script = driver::bootstrap_script(
        &json!({
            "mode": assist_mode,
            "provider": provider,
            "return_url": url,
            "auto": auto,
        }),
        &persona,
    );
    let flow_driver = flow.clone();
    // 登录窗口的 WebView2 参数：显式带 --proxy-server。
    //
    // Tauri 的 `builder.proxy_url()` 在「带独立 data_directory 的窗口」上实测**没生效**
    // （实测：登录窗口出口 IP 与直连一致，而代理本身是好的）。
    // 所以这里再走一遍命令行参数做双保险。
    let cdp_port = pick_cdp_port();
    LOGIN_CDP_PORT.store(cdp_port, std::sync::atomic::Ordering::Relaxed);
    // 端口每次注册都不一样，不记下来就没法从外部挂上去排查（求解器是宿主自己传的，不受影响）。
    flowlog::log(&flow, "cdp", &format!("port={cdp_port}"));
    let cdp_arg = if cdp_port == 0 {
        String::new()
    } else {
        format!(" --remote-debugging-port={cdp_port}")
    };
    let login_args = if proxy_str.is_empty() {
        format!("{LOGIN_WEBVIEW_ARGS}{cdp_arg}")
    } else {
        format!("{LOGIN_WEBVIEW_ARGS}{cdp_arg} --proxy-server={proxy_str}")
    };
    // 窗口尺寸与位置由 persona 决定，不能固定：innerWidth/innerHeight/screenX/screenY
    // 必须与伪 screen 尺寸自洽，否则「窗口比屏幕还大」一眼就是伪造。
    let win_w = persona["win"]["w"].as_f64().unwrap_or(1120.0);
    let win_h = persona["win"]["h"].as_f64().unwrap_or(820.0);
    let win_x = persona["win"]["x"].as_f64().unwrap_or(0.0);
    let win_y = persona["win"]["y"].as_f64().unwrap_or(0.0);
    let mut builder = tauri::WebviewWindowBuilder::new(
        &app,
        "login",
        tauri::WebviewUrl::External(url.parse::<tauri::Url>().map_err(|e| i18n::trf("err.oauth.bad_authorize_url", &[("e", &e.to_string())]))?),
    )
    .title(i18n::tr("title.login"))
    .theme(Some(tauri::Theme::Dark))
    .inner_size(win_w, win_h)
    .position(win_x, win_y)
    .min_inner_size(840.0, 560.0)
    .resizable(true)
    // 刻意不设 `.user_agent(...)`。
    //
    // 之前硬编码的是 Chrome/131 + Edg/131，而运行时 WebView2 已经是 155 ——
    // `navigator.userAgent` 说 131、`navigator.userAgentData.brands` 说 155，
    // 自相矛盾，比不伪装更容易被识别。UA 与 UA-CH 必须同源，
    // 唯一稳的做法是交给运行时自己给。
    .initialization_script(&driver_script)
    .additional_browser_args(&login_args)
    .data_directory(profile_dir);
    if let Some(u) = proxy_url {
        builder = builder.proxy_url(u);
    }
    builder
    .on_navigation(move |url| {
        if url.scheme() == driver::SENTINEL_SCHEME {
            match driver::parse_message(&url.to_string()) {
                Some(msg) => {
                    flowlog::log(&flow_driver, "drv", &driver::summarize(&msg));
                    let _ = app2.emit("reg://event", msg);
                }
                None => flowlog::log(&flow_driver, "driver-bad-msg", ""),
            }
            return false;
        }
        if url.scheme() == "http" || url.scheme() == "https" {
            let full = url.to_string();
            let short: String = full.chars().take(170).collect();
            flowlog::log(&flow_driver, "nav", &short);
        }
        if url.scheme() != "zcode" {
            return true;
        }
        let full = url.to_string();
        let (app3, p3, s3, f3, m3) = (app2.clone(), provider2.clone(), state2.clone(), flow2.clone(), mid2.clone());
        tauri::async_runtime::spawn(async move {
            finish_oauth(&app3, p3, s3, f3, m3, &full).await;
        });
        false
    })
    .build()
    .map_err(|e| {
        let mut pending = pending_oauth_guard();
        if pending.as_ref().map(|p| p.flow == flow).unwrap_or(false) {
            *pending = None;
        }
        let m = i18n::trf("err.oauth.window", &[("e", &e.to_string())]);
        flowlog::log(&flow, "window-fail", &m);
        m
    })?;
    if let Some(w) = app.get_webview_window("login") {
        let app_close = app.clone();
        w.on_window_event(move |e| {
            if let tauri::WindowEvent::CloseRequested { .. } = e {
                let mut pending = pending_oauth_guard();
                if pending.as_ref().map(|p| p.flow == flow_close).unwrap_or(false) {
                    flowlog::log(&flow_close, "cancelled", "");
                    *pending = None;
                    let _ = app_close.emit("reg://event", driver::closed_event());
                }
            }
        });
    }
    spawn_poll_loop(app.clone(), provider.clone(), flow.clone(), mid, poll_cfg);
    Ok(json!({ "opened": true, "provider": provider, "mode": assist_mode }))
}

#[tauri::command]
async fn reg_input(app: AppHandle, ask: String, value: Value) -> Result<(), String> {
    if ask != "signup" && ask != "link" {
        return Err(i18n::trf("err.reg.bad_ask", &[("ask", &ask)]));
    }
    let win = app
        .get_webview_window("login")
        .ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    let js = driver::host_dispatch(&json!({ "t": "input", "ask": ask, "value": value }));
    win.eval(&js)
        .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))
}

#[tauri::command]
async fn reg_action(app: AppHandle, action: String) -> Result<(), String> {
    let win = app
        .get_webview_window("login")
        .ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    match action.as_str() {
        "reveal" => {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            Ok(())
        }
        "retry" | "confirm-authorize" | "open-signup" => {
            let js = driver::host_dispatch(&json!({ "t": "action", "action": action }));
            win.eval(&js)
                .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))
        }
        other => Err(i18n::trf("err.reg.bad_action", &[("a", other)])),
    }
}

/// IMAP 自动寻址失败后的静默期：前端每 3 秒轮询一次，没有这个闸门会把整轮
/// 探测（最坏几十秒）反复重跑。
const IMAP_DISCOVER_COOLDOWN_MS: i64 = 60_000;
static IMAP_FAIL_AT: Mutex<Vec<(String, i64)>> = Mutex::new(Vec::new());

fn imap_cooldown_left(email: &str) -> Option<i64> {
    let key = email.trim().to_ascii_lowercase();
    let now = chrono::Local::now().timestamp_millis();
    let list = IMAP_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner());
    list.iter().find(|(e, _)| *e == key).and_then(|(_, at)| {
        let left = IMAP_DISCOVER_COOLDOWN_MS - (now - at);
        (left > 0).then_some(left)
    })
}

fn mark_imap_fail(email: &str) {
    let key = email.trim().to_ascii_lowercase();
    let now = chrono::Local::now().timestamp_millis();
    let mut list = IMAP_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(e, _)| *e != key);
    list.push((key, now));
}

fn clear_imap_fail(email: &str) {
    let key = email.trim().to_ascii_lowercase();
    let mut list = IMAP_FAIL_AT.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(e, _)| *e != key);
}

#[tauri::command]
async fn reg_fetch_link(
    email: String,
    limit: Option<usize>,
    since: Option<i64>,
) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let (acc, acc_zone) = pool::find_any(&root, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;
    let cur_flow = pending_oauth_guard()
        .as_ref()
        .map(|p| p.flow.clone())
        .unwrap_or_default();
    let take = limit.unwrap_or(12);

    let (found, new_rt, new_ep) = if acc.is_outlook() {
        match graph::fetch_links(&acc.client_id, &acc.refresh_token, take, since) {
            Ok((f, rt)) => (f, rt, None),
            Err(e) => {
                if !cur_flow.is_empty() {
                    flowlog::log(&cur_flow, "link-fetch-fail", &e.chars().take(200).collect::<String>());
                }
                if graph::is_credential_error(&e) {
                    let _ = pool::set_status(
                        &root,
                        &email,
                        pool::STATUS_INVALID,
                        Some(i18n::tr("err.graph.reauth")),
                    );
                }
                return Err(e);
            }
        }
    } else {
        if acc.password.trim().is_empty() {
            return Err(i18n::trf("err.imap.no_password", &[("email", acc.email.trim())]));
        }
        if let Some(left) = imap_cooldown_left(&acc.email) {
            return Err(i18n::trf("err.imap.cooling", &[("sec", &((left / 1000) + 1).to_string())]));
        }
        let (mail_addr, password, cached) = (acc.email.clone(), acc.password.clone(), acc.endpoint());
        let res = tauri::async_runtime::spawn_blocking(
            move || -> Result<(graph::Found, Option<imap::Endpoint>), String> {
                let (ep, fresh) = match cached {
                    Some(ep) => (ep, None),
                    None => {
                        let ep = imap::discover(&mail_addr, &password)?;
                        (ep.clone(), Some(ep))
                    }
                };
                let found = imap::fetch_links(&ep, &password, take, since)?;
                Ok((found, fresh))
            },
        )
        .await
        .map_err(|e| i18n::trf("err.imap.spawn", &[("e", &e.to_string())]))?;
        match res {
            Ok((f, fresh)) => {
                clear_imap_fail(&acc.email);
                (f, None, fresh)
            }
            Err(e) => {
                if !cur_flow.is_empty() {
                    flowlog::log(&cur_flow, "imap-fetch-fail", &e.chars().take(200).collect::<String>());
                }
                mark_imap_fail(&acc.email);
                return Err(e);
            }
        }
    };

    if !cur_flow.is_empty() {
        let newest: String = found
            .newest_subject
            .clone()
            .unwrap_or_default()
            .chars()
            .take(40)
            .collect();
        flowlog::log(
            &cur_flow,
            "link-fetch",
            &format!(
                "found={} scanned={} newest={newest}",
                found.links.len(),
                found.scanned
            ),
        );
    }

    // 回写：Graph 会轮换 refresh_token；IMAP 首次探测到的端点就地缓存，后续注册不再重探。
    if new_rt.is_some() || new_ep.is_some() {
        let mut accounts = pool::load_zone(&root, acc_zone);
        if let Some(a) = pool::find_mut(&mut accounts, &email) {
            if let Some(rt) = new_rt {
                a.refresh_token = rt;
            }
            if let Some(ep) = new_ep.as_ref() {
                a.set_endpoint(ep);
            }
            let _ = pool::save_zone(&root, acc_zone, &accounts);
        }
    }

    Ok(json!({
        "links": found.links,
        "scanned": found.scanned,
        "newestSubject": found.newest_subject,
        "newestAt": found.newest_at,
    }))
}

/// 自动完成注册表单上的滑块验证码。
///
/// 由前端在 driver 报告「验证码挡着」时调用；求解成功后页面自身会显示
/// 「验证通过」，driver 的门控随即放行并提交，不需要回传 token。
#[tauri::command]
async fn captcha_solve() -> Result<Value, String> {
    let store = Paths::detect().store_dir();
    let port = LOGIN_CDP_PORT.load(std::sync::atomic::Ordering::Relaxed);
    let r = tauri::async_runtime::spawn_blocking(move || captcha::solve(&store, port))
        .await
        .map_err(|e| i18n::trf("err.captcha.spawn", &[("e", &e.to_string())]))?;
    match &r {
        Ok(v) => {
            let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
            flowlog::log(
                "captcha",
                if ok { "solved" } else { "unsolved" },
                &v.to_string().chars().take(300).collect::<String>(),
            );
        }
        Err(e) => flowlog::log("captcha", "error", &e.chars().take(300).collect::<String>()),
    }
    r
}

/// 注册前的邮箱预检：连不上或读不了收件箱就提前判失败。
///
/// 顺序上放在注册之前——不然白开一次登录窗口、白过一次验证码，最后才发现邮箱是死的。
/// 顺带把探测到的 IMAP 端点缓存回账号，后面取激活链接不用再探一遍。
#[tauri::command]
async fn mail_precheck(email: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let (acc, zone) = pool::find_any(&root, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;

    if acc.is_outlook() {
        // Outlook 走 Graph，凭据问题会在取信那步报出来，这里不额外校验
        return Ok(json!({ "ok": true, "kind": "outlook" }));
    }
    // 域名被上游拒收的（一次性邮箱等）不用再往下走
    if let Some(dom) = pool::domain_blocked(&root, &acc.email) {
        flowlog::log("precheck", "blocked", &format!("{} 域名 {dom} 不可注册", acc.email));
        return Ok(json!({
            "ok": false,
            "blocked": true,
            "domain": dom,
            "error": i18n::trf("err.mail.domain_blocked", &[("domain", &dom)]),
        }));
    }
    if acc.password.trim().is_empty() {
        return Err(i18n::trf("err.imap.no_password", &[("email", acc.email.trim())]));
    }

    let (addr, password, cached) = (acc.email.clone(), acc.password.clone(), acc.endpoint());
    let res = tauri::async_runtime::spawn_blocking(
        move || -> Result<(imap::Endpoint, usize), String> {
            imap::probe(&addr, &password, cached.as_ref())
        },
    )
    .await
    .map_err(|e| i18n::trf("err.imap.spawn", &[("e", &e.to_string())]))?;

    match res {
        Ok((ep, count)) => {
            let mut accounts = pool::load_zone(&root, zone);
            if let Some(a) = pool::find_mut(&mut accounts, &email) {
                a.set_endpoint(&ep);
                let _ = pool::save_zone(&root, zone, &accounts);
            }
            flowlog::log(
                "precheck",
                "ok",
                &format!("{email} -> {}:{} inbox={count}", ep.host, ep.port),
            );
            Ok(json!({
                "ok": true,
                "kind": "imap",
                "host": ep.host,
                "port": ep.port,
                "messages": count,
            }))
        }
        Err(e) => {
            flowlog::log(
                "precheck",
                "fail",
                &format!("{email}: {}", e.chars().take(200).collect::<String>()),
            );
            Ok(json!({ "ok": false, "error": e }))
        }
    }
}

/// 域名黑名单：内置 + 用户自定义，预检阶段直接拦掉。
#[tauri::command]
async fn blocked_list() -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    Ok(json!({
        "builtin": pool::blocked_builtin(),
        "custom": pool::blocked_custom(&root),
    }))
}

#[tauri::command]
async fn blocked_add(domain: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let d = pool::blocked_add(&root, &domain)?;
    Ok(json!({ "ok": true, "domain": d, "custom": pool::blocked_custom(&root) }))
}

#[tauri::command]
async fn blocked_remove(domain: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    pool::blocked_remove(&root, &domain)?;
    Ok(json!({ "ok": true, "custom": pool::blocked_custom(&root) }))
}

#[tauri::command]
async fn pool_list() -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let items: Vec<_> = pool::load(&root).iter().map(|a| a.to_summary()).collect();
    Ok(json!({ "accounts": items }))
}

/// 待注册账号：独立于邮箱库的一份列表，注册流程从这里取号。
#[tauri::command]
async fn pending_list() -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let items: Vec<_> = pool::load_zone(&root, pool::ZONE_PENDING)
        .iter()
        .map(|a| a.to_summary())
        .collect();
    Ok(json!({ "accounts": items }))
}

#[tauri::command]
async fn pending_remove(app: AppHandle, email: String) -> Result<Value, String> {
    let removed = {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        let mut accounts = pool::load_zone(&root, pool::ZONE_PENDING);
        let before = accounts.len();
        accounts.retain(|a| !a.email.eq_ignore_ascii_case(email.trim()));
        let removed = before - accounts.len();
        pool::save_zone(&root, pool::ZONE_PENDING, &accounts)?;
        removed
    };
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "removed": removed }))
}

#[tauri::command]
async fn pending_remove_many(app: AppHandle, emails: Vec<String>) -> Result<Value, String> {
    let removed = {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        let mut accounts = pool::load_zone(&root, pool::ZONE_PENDING);
        let before = accounts.len();
        let targets: Vec<String> = emails.iter().map(|e| e.trim().to_ascii_lowercase()).collect();
        accounts.retain(|a| !targets.contains(&a.email.trim().to_ascii_lowercase()));
        let removed = before - accounts.len();
        pool::save_zone(&root, pool::ZONE_PENDING, &accounts)?;
        removed
    };
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "removed": removed }))
}

/// 粘贴导入：多行文本，按与文件导入完全相同的规则自动分流。
#[tauri::command]
async fn pool_import_text(app: AppHandle, text: String) -> Result<Value, String> {
    let r = {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        pool::import_split(&root, &text, &store::now_ts())?
    };
    let _ = app.emit("pool-changed", ());
    Ok(json!({
        "added": r.added,
        "skipped": r.skipped,
        "parsed": r.total_parsed,
        "pendingAdded": r.pending_added,
        "mailAdded": r.mail_added,
    }))
}

#[tauri::command]
async fn pool_import_pick(app: AppHandle) -> Result<Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.txt"), &["txt"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp
        .into_path()
        .map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| i18n::trf("err.pool.read", &[("e", &e.to_string())]))?;

    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let r = pool::import_split(&root, &raw, &store::now_ts())?;
    drop(_guard);

    let items: Vec<_> = pool::load(&root).iter().map(|a| a.to_summary()).collect();
    let pending: Vec<_> = pool::load_zone(&root, pool::ZONE_PENDING)
        .iter()
        .map(|a| a.to_summary())
        .collect();
    Ok(json!({
        "picked": true,
        "added": r.added,
        "skipped": r.skipped,
        "parsed": r.total_parsed,
        "pendingAdded": r.pending_added,
        "mailAdded": r.mail_added,
        "accounts": items,
        "pendingAccounts": pending,
    }))
}

#[tauri::command]
async fn pool_remove(email: String) -> Result<Value, String> {
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let mut accounts = pool::load(&root);
    let before = accounts.len();
    accounts.retain(|a| !a.email.eq_ignore_ascii_case(email.trim()));
    pool::save(&root, &accounts)?;
    drop(_guard);
    Ok(json!({ "removed": before - accounts.len() }))
}

#[tauri::command]
async fn pool_remove_many(emails: Vec<String>) -> Result<Value, String> {
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let mut accounts = pool::load(&root);
    let before = accounts.len();
    let targets: Vec<String> = emails.iter().map(|e| e.trim().to_ascii_lowercase()).collect();
    accounts.retain(|a| !targets.contains(&a.email.trim().to_ascii_lowercase()));
    pool::save(&root, &accounts)?;
    drop(_guard);
    Ok(json!({ "removed": before - accounts.len() }))
}

#[tauri::command]
async fn pool_get(email: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let (a, zone) = pool::find_any(&root, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;
    Ok(json!({ "email": a.email, "password": a.password, "zone": zone }))
}

#[tauri::command]
async fn pool_mark_verified(
    app: AppHandle,
    email: String,
    ok: bool,
    note: Option<String>,
) -> Result<(), String> {
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        if ok {
            pool::set_status_any(&root, &email, pool::STATUS_VERIFIED, None)?;
        } else {
            pool::set_status_any(&root, &email, pool::STATUS_FAILED, note)?;
        }
    }
    let _ = app.emit("pool-changed", ());
    Ok(())
}

#[tauri::command]
async fn reg_close_window(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.close();
    }
    Ok(())
}

#[tauri::command]
async fn pool_reset(app: AppHandle, email: String) -> Result<(), String> {
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        pool::set_status_any(&root, &email, pool::STATUS_NEW, None)?;
    }
    let _ = app.emit("pool-changed", ());
    Ok(())
}

#[tauri::command]
async fn pool_upsert(app: AppHandle, line: String) -> Result<Value, String> {
    let Some(parsed) = pool::parse_line(&line, &store::now_ts()) else {
        return Err(i18n::tr("err.pool.bad_line"));
    };
    // 与导入同样的分流规则：通用邮箱进待注册区，Outlook 进邮箱库。
    let zone = if parsed.is_outlook() {
        pool::ZONE_MAIL
    } else {
        pool::ZONE_PENDING
    };
    let _guard = store_guard();
    let root = Paths::detect().store_dir();
    let mut accounts = pool::load_zone(&root, zone);
    let updated = if let Some(a) = pool::find_mut(&mut accounts, &parsed.email) {
        a.password = parsed.password.clone();
        a.client_id = parsed.client_id.clone();
        a.refresh_token = parsed.refresh_token.clone();
        a.imap_host = parsed.imap_host.clone();
        a.imap_port = parsed.imap_port;
        a.imap_user = None;
        a.status = pool::STATUS_NEW.to_string();
        a.note = None;
        true
    } else {
        accounts.push(parsed.clone());
        false
    };
    pool::save_zone(&root, zone, &accounts)?;
    drop(_guard);
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "email": parsed.email, "updated": updated, "zone": zone }))
}

#[tauri::command]
async fn outlook_reauth_begin(email: String) -> Result<Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    let acc = pool::find(&accounts, &email)
        .ok_or_else(|| i18n::trf("err.pool.not_found", &[("email", email.trim())]))?;
    let cid = acc.client_id.clone();
    let d = graph::device_code_begin(&cid)?;
    Ok(json!({
        "client_id": cid,
        "device_code": d.device_code,
        "user_code": d.user_code,
        "verification_uri": d.verification_uri,
        "interval": d.interval,
        "expires_in": d.expires_in,
    }))
}

#[tauri::command]
async fn outlook_reauth_poll(
    app: AppHandle,
    email: String,
    client_id: String,
    device_code: String,
) -> Result<Value, String> {
    let p = graph::device_code_poll(&client_id, &device_code)?;
    if p.pending {
        return Ok(json!({ "pending": true }));
    }
    let Some(rt) = p.refresh_token else {
        return Ok(json!({ "pending": true }));
    };
    {
        let _guard = store_guard();
        let root = Paths::detect().store_dir();
        let mut accounts = pool::load(&root);
        if let Some(a) = pool::find_mut(&mut accounts, &email) {
            a.refresh_token = rt;
            if a.client_id.trim().is_empty() {
                a.client_id = client_id;
            }
            a.status = pool::STATUS_NEW.to_string();
            a.note = None;
            pool::save(&root, &accounts)?;
        }
    }
    let _ = app.emit("pool-changed", ());
    Ok(json!({ "pending": false, "ok": true }))
}

#[tauri::command]
async fn reg_set_mode(app: AppHandle, mode: String) -> Result<String, String> {
    let m = mode.trim().to_ascii_lowercase();
    if !driver::is_valid_mode(&m) {
        return Err(i18n::trf("err.reg.bad_mode", &[("m", &mode)]));
    }
    let win = app
        .get_webview_window("login")
        .ok_or_else(|| i18n::tr("err.reg.no_window"))?;
    let js = driver::host_dispatch(&json!({ "t": "setMode", "mode": m }));
    win.eval(&js)
        .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))?;
    Ok(m)
}

fn sweep_login_profiles(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() && meta.modified().map(|m| m < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[tauri::command]
async fn set_auth_proxy(app: AppHandle, on: bool, url: Option<String>) -> Result<(), String> {
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let trimmed = url.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let normalized = match trimmed {
            Some(s) => Some(oauth::parse_proxy_url(s)?),
            None => None,
        };
        if on && normalized.is_none() {
            return Err(i18n::tr("err.proxy.need_url"));
        }
        let mut s = load_settings(&paths);
        s.auth_proxy_on = Some(on);
        s.auth_proxy_url = normalized;
        save_settings(&paths, &s)?;
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

async fn finish_oauth(app: &AppHandle, provider: String, state: String, flow: String, mid: String, callback_url: &str) {
    let result = {
        let provider = provider.clone();
        let state = state.clone();
        let flow = flow.clone();
        let mid = mid.clone();
        let callback_url = callback_url.to_string();
        // 绑定邀请码要在注册窗口还活着的时候做，所以连窗口句柄一起带进闭包。
        let app_invite = app.clone();
        tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
            {
                let pending = pending_oauth_guard();
                match pending.as_ref() {
                    Some(p) if p.provider == provider && p.state == state && p.flow == flow => {}
                    Some(_) | None => {
                        flowlog::log(&flow, "deeplink-superseded", "");
                        return Err("__superseded__".into());
                    }
                }
            }
            let (code, cb_state) = match oauth::parse_callback(&callback_url) {
                Ok(oauth::CallbackKind::Code { code, state }) => (code, state),
                Ok(oauth::CallbackKind::Attribution) => {
                    flowlog::log(&flow, "deeplink-attribution", "");
                    return Err("__attribution__".into());
                }
                Err(e) => {
                    flowlog::log(&flow, "deeplink-parse-fail", &e);
                    return Err(e);
                }
            };
            if cb_state != state {
                flowlog::log(&flow, "deeplink-state-mismatch", "");
                return Err(i18n::tr("err.oauth.state"));
            }
            let exchanged = match oauth::exchange_token(&provider, &code, &state, &mid) {
                Ok(v) => v,
                Err(e) => {
                    flowlog::log(&flow, "exchange-fail", &e);
                    return Err(e);
                }
            };
            match persist_oauth_account(&Paths::detect(), &provider, &exchanged["raw"], &flow, &mid, false) {
                Ok(v) => {
                    flowlog::log(
                        &flow,
                        "persist-ok",
                        &format!("channel=deeplink duplicate={}", v.get("duplicate").is_some()),
                    );
                    // 账号已入库 → 顺手把邀请码绑上（不是必须项，失败只留痕）。
                    finish_invite(&app_invite, &exchanged["raw"], &flow);
                    Ok(v)
                }
                Err(e) => {
                    if e != "__superseded__" {
                        flowlog::log(&flow, "persist-fail", &format!("channel=deeplink {e}"));
                    }
                    Err(e)
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(i18n::trf("err.oauth.flow", &[("e", &e.to_string())])))
    };
    if let Err(e) = &result {
        if deeplink_err_soft(e) {
            let ours = pending_oauth_guard().as_ref().map(|p| p.flow == flow).unwrap_or(false);
            if ours {
                flowlog::log(&flow, "soft-fail", e);
                let _ = app.emit("oauth://done", &json!({ "ok": false, "soft": true, "error": e }));
            }
            return;
        }
    }
    finalize_oauth_result(app, result);
}

fn deeplink_err_soft(e: &str) -> bool {
    e != "__superseded__" && e != "__attribution__"
}

/// 内置邀请码。settings 里填了就用 settings 的。
const DEFAULT_INVITE_CODE: &str = "MK45MPKN";
/// 邀请页所在域。注册窗口收尾时正好停在这里（`/api/v1/oauth/cli/callback/...`）。
const INVITE_HOST: &str = "zcode.z.ai";
/// 等窗口自己落到 zcode.z.ai 的时限。poll 可能比桥接页先 ready，
/// 这时窗口还在跳转中间，必须等而不是抢着导航。
const INVITE_HOST_WAIT_MS: u64 = 15_000;
/// 等邀请页跳成功页的时限。实测整条自动绑定（无感验证码 + bind-code）约 3~6 秒；
/// 留够 SDK 加载与重试的余量即可 —— 批量注册时它是每个号唯一会等的一步。
const INVITE_BOUND_WAIT_MS: u64 = 40_000;

/// 同一时刻只允许一个绑定在跑：poll 与 deeplink 两条完成通道都可能走到这里。
static INVITE_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn invite_code() -> String {
    load_settings(&Paths::detect())
        .invite_code
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| DEFAULT_INVITE_CODE.to_string())
}

/// 邀请码是 8 位、去掉了 0/1/I/O 这类易混字符的大写串（与前端 `mz()` 的校验一致）。
fn valid_invite_code(c: &str) -> bool {
    c.chars().count() == 8 && c.chars().all(|ch| "23456789ABCDEFGHJKLMNPQRSTUVWXYZ".contains(ch))
}

/// 注册/登录收尾时顺手把邀请码绑上。
///
/// 宿主只做三件事：把 zcode 网页 token 写进注册窗口的 localStorage、写一份
/// 10 分钟内有效的一次性意图（`acceptIntent` 的 nonce 必须对得上）、导航到邀请页。
/// 剩下的——无感验证码、`bind-code`、跳成功页——全由 zcode 前端自己跑完，
/// 这条链路是实测走通的，这里不重造一遍。
///
/// **失败不算注册失败**：调用方只记日志、发事件；账号这时早就入库了。
fn bind_invite_code(app: &AppHandle, jwt: &str, flow: &str) -> Result<String, String> {
    let code = invite_code();
    if !valid_invite_code(&code) {
        return Err(i18n::trf("err.invite.bad_code", &[("code", &code)]));
    }
    let jwt = jwt.trim();
    if jwt.is_empty() {
        return Err(i18n::tr("err.invite.no_token"));
    }
    if INVITE_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(i18n::tr("err.invite.busy"));
    }
    let _busy = BusyFlag;

    let win = app
        .get_webview_window("login")
        .ok_or_else(|| i18n::tr("err.invite.no_window"))?;
    let host_now = || -> String {
        win.url()
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default()
    };
    let wait_host = || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(INVITE_HOST_WAIT_MS);
        while std::time::Instant::now() < deadline && host_now() != INVITE_HOST {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    };

    if host_now() != INVITE_HOST {
        wait_host();
    }
    if host_now() != INVITE_HOST {
        // 窗口没自己过来（或还停在 chat.z.ai）：直接导到邀请页。
        // 邀请页只认 origin，从哪个入口进来都行；token 是下一步才写的，
        // 所以这一趟先把页面落地，避免带着 intent 空跑一次。
        let js = format!("location.href = 'https://{INVITE_HOST}/cn/invite/{code}';");
        win.eval(&js)
            .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))?;
        wait_host();
    }
    if host_now() != INVITE_HOST {
        return Err(i18n::tr("err.invite.no_origin"));
    }

    let nonce = uuid::Uuid::new_v4().to_string();
    let j = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    let js = format!(
        "(function(){{try{{\
         localStorage.setItem(\"zcodejwttoken\", {jwt});\
         sessionStorage.setItem(\"incentive:accept-intent\", JSON.stringify({{code:{code}, nonce:{nonce}, createdAt:Date.now()}}));\
         location.href = \"/cn/invite/\" + encodeURIComponent({code}) + \"?acceptIntent=\" + encodeURIComponent({nonce});\
         }}catch(e){{}}}})();",
        jwt = j(jwt),
        code = j(&code),
        nonce = j(&nonce),
    );
    win.eval(&js)
        .map_err(|e| i18n::trf("err.reg.eval", &[("e", &e.to_string())]))?;
    flowlog::log(flow, "invite-nav", &format!("code={code}"));

    // 成功后是 Next.js 客户端路由（不刷新页面），所以盯 URL 就够。
    //
    // 但 URL 是**弱信号**：重复登录同一个号时，邀请页走的是「你已接受邀请」
    // 分支，永远不跳成功页。所以隔几秒补一次服务端复核，别把「已绑定」
    // 误报成失败 —— 那会让批量日志看起来像功能坏了。
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(INVITE_BOUND_WAIT_MS);
    let mut last_probe = std::time::Instant::now();
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        let hit = win
            .url()
            .map(|u| u.path().contains("/invite/success"))
            .unwrap_or(false);
        if hit {
            return Ok(code);
        }
        if last_probe.elapsed() >= std::time::Duration::from_secs(6) {
            last_probe = std::time::Instant::now();
            if invite_bound_confirmed(jwt) {
                return Ok(code);
            }
        }
    }
    if invite_bound_confirmed(jwt) {
        return Ok(code);
    }
    Err(i18n::tr("err.invite.timeout"))
}

/// 服务端复核：`referral/summary` 里那条 `referral_bind_code` 新手任务。
///
/// 这是绑定的权威状态，页面跳不跳成功页都不影响它。请求失败一律当「没绑上」，
/// 继续走原来的超时逻辑 —— 复核只是加分项，不该让判定更糟。
fn invite_bound_confirmed(jwt: &str) -> bool {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let resp = match agent
        .get("https://zcode.z.ai/api/v1/incentive/referral/summary")
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {jwt}"))
        .call()
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    let body = resp.into_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    v.pointer("/data/invitee/tasks")
        .and_then(|t| t.as_array())
        .map(|tasks| {
            tasks.iter().any(|t| {
                t.get("key").and_then(|k| k.as_str()) == Some("referral_bind_code")
                    && t.get("completed").and_then(|c| c.as_bool()) == Some(true)
            })
        })
        .unwrap_or(false)
}

/// `INVITE_BUSY` 的 RAII 释放：函数里有多条提前 return 的分支。
struct BusyFlag;

impl Drop for BusyFlag {
    fn drop(&mut self) {
        INVITE_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 绑定收尾：拿 jwt → 绑 → 上报。**永远不返回 Err** —— 它不该影响注册结果。
fn finish_invite(app: &AppHandle, raw: &Value, flow: &str) {
    let jwt = raw
        .pointer("/data/token")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let _ = app.emit("reg://event", json!({ "kind": "phase", "phase": "bind-invite" }));
    let _ = app.emit(
        "reg://event",
        json!({ "kind": "log", "level": "info", "code": "inviteStart", "a": invite_code() }),
    );
    match bind_invite_code(app, &jwt, flow) {
        Ok(code) => {
            flowlog::log(flow, "invite-bound", &format!("code={code}"));
            let _ = app.emit(
                "reg://event",
                json!({ "kind": "log", "level": "ok", "code": "inviteBound", "a": code }),
            );
        }
        Err(e) => {
            // 绑定不是必须项：注册已经成功，这里只留痕。
            flowlog::log(flow, "invite-skip", &e);
            let _ = app.emit(
                "reg://event",
                json!({ "kind": "log", "level": "warn", "code": "inviteFail", "a": e }),
            );
        }
    }
    // 绑定阶段结束（无论成败）才把流程标成完成 —— driver 早前发过一次
    // `done`，那次是「授权完成」，随后就被 `bind-invite` 覆盖了。
    let _ = app.emit("reg://event", json!({ "kind": "phase", "phase": "done" }));
}

fn persist_oauth_account(
    paths: &Paths,
    provider: &str,
    raw: &serde_json::Value,
    flow: &str,
    mid: &str,
    poll_ready: bool,
) -> Result<serde_json::Value, String> {
    let flow_still_ours = || {
        pending_oauth_guard()
            .as_ref()
            .map(|p| p.flow == flow)
            .unwrap_or(false)
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let jwt = raw
        .pointer("/data/token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| i18n::tr("err.oauth.no_token"))?
        .to_string();
    let t_all = std::time::Instant::now();
    let t_biz = std::time::Instant::now();
    let raw_access = oauth::extract_access_token(provider, raw).unwrap_or_default();
    let access_token = if provider == "zai" && !raw_access.is_empty() {
        match oauth::resolve_zai_business_token_diag(&raw_access) {
            (Some(t), _) => t,
            (None, why) => {
                flowlog::log(flow, "zai-business-fail", &why);
                return Err(i18n::tr("err.oauth.zai_business"));
            }
        }
    } else {
        raw_access
    };
    let ms_biz = t_biz.elapsed().as_millis();
    let t_user = std::time::Instant::now();
    let userinfo = if poll_ready {
        oauth::extract_poll_user_profile(raw)
    } else {
        oauth::extract_user_profile(provider, raw)
    }
    .or_else(|| {
        (!access_token.is_empty())
            .then(|| oauth::fetch_userinfo(provider, &access_token))
            .flatten()
    });
    let refresh_token = oauth::extract_refresh_token(provider, raw);
    let ms_user = t_user.elapsed().as_millis();
    let t_cfg = std::time::Instant::now();
    let credentials = oauth::assemble_credentials_with_token(
        provider,
        &jwt,
        userinfo.as_ref(),
        (!access_token.is_empty()).then_some(access_token.as_str()),
        refresh_token.as_deref(),
    );
    let config = oauth::assemble_config(provider, &jwt, &access_token);
    flowlog::log(
        flow,
        "persist-t",
        &format!(
            "biz={ms_biz}ms user={ms_user}ms cfg={}ms net-total={}ms",
            t_cfg.elapsed().as_millis(),
            t_all.elapsed().as_millis()
        ),
    );

    let _lock = store_guard();
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let accounts = list_accounts(paths)?;
    let hash = canonical_hash(&credentials);
    if let Some(i) = store::find_same_login(&credentials, &hash, &accounts, &paths.home) {
        let mut dup = accounts[i].clone();
        dup.hash = hash.clone();
        dup.credentials = credentials;
        dup.config = Some(config);
        dup.updated_at = now_ts();
        if dup.virtual_device_mid.as_deref().map_or(true, |m| m.trim().is_empty()) {
            dup.virtual_device_mid = Some(mid.to_string());
        }
        if !flow_still_ours() {
            return Err("__superseded__".into());
        }
        save_account(paths, &dup)?;
        *pending_oauth_guard() = None;
        return Ok(json!({ "id": dup.id, "name": dup.name, "provider": provider, "duplicate": true }));
    }
    let base = credentials
        .get(format!("oauth:{provider}:user_info"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|u| u.get("username").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| match provider {
            "zai" => "z.ai".to_string(),
            _ => "BigModel".to_string(),
        });
    let name = unique_name(&accounts, &base);
    let ts = now_ts();
    let acc = Account {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        created_at: ts.clone(),
        updated_at: ts,
        hash,
        credentials,
        config: Some(config),
        virtual_device_mid: Some(mid.to_string()),
        virtual_arms_uid: Some(store::new_arms_uid()),
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    save_account(paths, &acc)?;
    *pending_oauth_guard() = None;
    Ok(json!({ "id": acc.id, "name": acc.name, "provider": provider }))
}

fn finalize_oauth_result(app: &AppHandle, result: Result<serde_json::Value, String>) {
    if let Err(e) = &result {
        if e == "__superseded__" || e == "__attribution__" {
            return;
        }
    }
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.close();
    }
    let payload = match result {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e }),
    };
    let _ = app.emit("oauth://done", &payload);
}

fn spawn_poll_loop(app: AppHandle, provider: String, flow: String, mid: String, cfg: PollCfg) {
    std::thread::spawn(move || {
        let ours = || pending_oauth_guard().as_ref().map(|p| p.flow == flow).unwrap_or(false);
        let deadline = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            std::cmp::min(cfg.expires_at_ms, now + u128::from(oauth::FLOW_TIMEOUT_MS))
        };
        loop {
            if !ours() {
                flowlog::log(&flow, "poll-exit", "flow-done-or-replaced");
                return;
            }
            match oauth::poll_flow_once(&cfg.url, &cfg.token, &mid) {
                Ok(oauth::PollOutcome::Pending) => {}
                Ok(oauth::PollOutcome::Ready(data)) => {
                    flowlog::log(&flow, "poll-ready", "");
                    let raw = json!({ "code": 0, "data": data });
                    let result = persist_oauth_account(&Paths::detect(), &provider, &raw, &flow, &mid, true);
                    match &result {
                        Ok(v) => flowlog::log(
                            &flow,
                            "persist-ok",
                            &format!("channel=poll duplicate={}", v.get("duplicate").is_some()),
                        ),
                        Err(e) if e != "__superseded__" => {
                            flowlog::log(&flow, "persist-fail", &format!("channel=poll {e}"));
                        }
                        Err(_) => {}
                    }
                    // 账号已入库 → 顺手把邀请码绑上（不是必须项，失败只留痕）。
                    // 必须赶在 finalize 关窗之前，绑定要用这个窗口。
                    if result.is_ok() {
                        finish_invite(&app, &raw, &flow);
                    }
                    finalize_oauth_result(&app, result);
                    return;
                }
                Err(e) => {
                    if !ours() {
                        flowlog::log(&flow, "poll-exit", "superseded");
                        return;
                    }
                    flowlog::log(&flow, "poll-fail", &e);
                    *pending_oauth_guard() = None;
                    finalize_oauth_result(&app, Err(e));
                    return;
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now >= deadline {
                if !ours() {
                    return;
                }
                flowlog::log(&flow, "poll-timeout", "");
                *pending_oauth_guard() = None;
                finalize_oauth_result(&app, Err(i18n::tr("err.oauth.expired")));
                return;
            }
            let sleep = (deadline - now).min(cfg.interval_ms as u128) as u64;
            std::thread::sleep(std::time::Duration::from_millis(sleep));
        }
    });
}

fn open_captcha_window(app: &AppHandle, auto: bool) -> Result<(), String> {
    let (w, h) = (380.0, 320.0);
    if let Some(win) = app.get_webview_window("captcha") {
        let _ = win.eval("location.reload()");
        center_over_main(app, &win, w, h);
        if auto {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
        return Ok(());
    }

    const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-proxy-server --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows";
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "captcha",
        tauri::WebviewUrl::App("captcha.html".into()),
    )
    .title(i18n::tr("title.captcha"))
    .theme(Some(tauri::Theme::Dark))
    .background_color(tauri::window::Color(0, 0, 0, 255))
    .inner_size(w, h)
    .min_inner_size(340.0, 280.0)
    .maximizable(false)
    .resizable(false)
    .additional_browser_args(BROWSER_ARGS)
    .visible(false)
    .build()
    .map_err(|e| {
        flowlog::log("captcha", "build-err", &e.to_string());
        e.to_string()
    })?;
    center_over_main(app, &win, w, h);
    if !auto {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn captcha_show(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("captcha") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn kill_zcode(app: AppHandle) -> Result<(), String> {
    let _guard = store_guard();
    let r = if store::kill_zcode()? { Ok(()) } else { Err(i18n::tr("err.zcode.kill_timeout")) };
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn set_behavior(
    app: AppHandle,
    launch_after_switch: Option<bool>,
    close_to_tray: Option<bool>,
    auto_claim: Option<bool>,
) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    if let Some(v) = launch_after_switch {
        s.launch_after_switch = Some(v);
    }
    if let Some(v) = close_to_tray {
        s.close_to_tray = Some(v);
    }
    if let Some(v) = auto_claim {
        s.auto_claim = Some(v);
    }
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn set_language(app: AppHandle, lang: String) -> Result<(), String> {
    let l = i18n::Lang::parse(&lang)
        .ok_or_else(|| i18n::trf("err.lang.unknown", &[("lang", &lang)]))?;
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let mut s = load_settings(&paths);
        s.language = Some(l.as_str().to_string());
        save_settings(&paths, &s)?;
    }
    i18n::set(l);
    rebuild_tray(&app);
    for (label, key) in [("captcha", "title.captcha"), ("login", "title.login")] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_title(&i18n::tr(key));
        }
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

#[tauri::command]
async fn reveal_main(app: AppHandle) -> Result<(), String> {
    let win = app.get_webview_window("main").ok_or_else(|| i18n::tr("err.main.missing"))?;
    win.show().map_err(|e| e.to_string())?;
    let _ = win.set_focus();
    Ok(())
}

fn center_over_main(app: &AppHandle, win: &tauri::WebviewWindow, w: f64, h: f64) {
    let Some(m) = app.get_webview_window("main") else { return };
    let (Ok(p), Ok(s), Ok(scale)) = (m.outer_position(), m.outer_size(), m.scale_factor()) else { return };
    let (mx, my) = (p.x as f64 / scale, p.y as f64 / scale);
    let (mw, mh) = (s.width as f64 / scale, s.height as f64 / scale);
    let x = mx + (mw - w).max(0.0) / 2.0;
    let y = my + (mh - h).max(0.0) / 2.0;
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

#[tauri::command]
async fn autostart_status(app: AppHandle) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn autostart_set(app: AppHandle, enable: bool) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    if enable {
        al.enable().map_err(|e| e.to_string())?;
    } else {
        al.disable().map_err(|e| e.to_string())?;
    }
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn export_one(app: AppHandle, id: String) -> Result<serde_json::Value, String> {
    let acc = load_account(&Paths::detect(), &id)?;
    let default_name = format!("{}.json", sanitize_filename(&acc.name));
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let payload = store::export_bundle_value(std::slice::from_ref(&acc));
    let body = serde_json::to_string_pretty(&payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "name": acc.name }))
}

#[tauri::command]
async fn export_all(app: AppHandle) -> Result<serde_json::Value, String> {
    let accounts = list_accounts(&Paths::detect())?;
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty"));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .set_file_name("zcode-accounts.json")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let payload = store::export_bundle_value(&accounts);
    let body = serde_json::to_string_pretty(&payload).unwrap() + "\n";
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
}

#[tauri::command]
async fn import_files(app: AppHandle) -> Result<ImportReport, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.json"), &["json"])
        .blocking_pick_files();
    let Some(files) = picked else {
        return Ok(ImportReport { picked: false, ..Default::default() });
    };
    let _guard = store_guard();
    let mut bundles = vec![];
    let mut errors = vec![];
    for fp in files {
        let path = match fp.into_path() {
            Ok(p) => p,
            Err(e) => {
                errors.push(i18n::trf("err.path.conv", &[("e", &e.to_string())]));
                continue;
            }
        };
        let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(v) => bundles.push((fname, v)),
                Err(e) => errors.push(i18n::trf("err.import.json", &[("fname", fname.as_str()), ("e", &e.to_string())])),
            },
            Err(e) => errors.push(i18n::trf("err.import.read", &[("fname", fname.as_str()), ("e", &e.to_string())])),
        }
    }
    let mut report = if bundles.is_empty() {
        ImportReport { picked: true, ..Default::default() }
    } else {
        import_values(&Paths::detect(), &bundles)?
    };
    report.picked = true;
    report.errors.extend(errors);
    rebuild_tray(&app);
    Ok(report)
}

#[tauri::command]
async fn pool_export(app: AppHandle) -> Result<serde_json::Value, String> {
    let root = Paths::detect().store_dir();
    let accounts = pool::load(&root);
    if accounts.is_empty() {
        return Err(i18n::tr("err.pool.empty"));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.txt"), &["txt"])
        .set_file_name("mailboxes.txt")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    let body: String = accounts
        .iter()
        .map(|a| format!("{}----{}----{}----{}\n", a.email, a.password, a.client_id, a.refresh_token))
        .collect();
    store::atomic_write(&path, &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
}

#[tauri::command]
async fn pick_zcode_path(app: AppHandle) -> Result<serde_json::Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.exe"), &["exe"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy() }))
}

#[tauri::command]
async fn set_zcode_path(app: AppHandle, path: String) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    s.zcode_path = store::normalize_zcode_path(&path);
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn launch_zcode(app: AppHandle) -> Result<(), String> {
    let paths = Paths::detect();
    let (p, ok) = effective_zcode_path(&paths);
    if !ok {
        return Err(i18n::trf("err.zcode.path_invalid_hint", &[("p", &p)]));
    }
    let r = store::launch_zcode(&p);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    store::open_url(&url)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]

static RELAY: std::sync::OnceLock<gateway::Gateway> = std::sync::OnceLock::new();

fn relay() -> &'static gateway::Gateway {
    RELAY.get_or_init(gateway::Gateway::new)
}

#[tauri::command]
async fn relay_status() -> Result<Value, String> {
    Ok(relay().status(&Paths::detect()))
}

#[tauri::command]
async fn relay_refresh(app: AppHandle, passes: Option<usize>) -> Result<Value, String> {
    let paths = Paths::detect();
    let gw = relay();
    let warm = gw.warm_quota(&paths, passes.unwrap_or(3), |p| {
        let _ = app.emit("relay-warm", p);
    });
    Ok(json!({ "warm": warm, "status": gw.status(&Paths::detect()) }))
}

#[tauri::command]
async fn relay_policy(policy: String, pinned: Option<String>) -> Result<Value, String> {
    relay().set_policy(policy, pinned)?;
    Ok(relay().status(&Paths::detect()))
}

#[tauri::command]
async fn relay_model_mode(mode: String, pinned: Option<String>) -> Result<Value, String> {
    let paths = Paths::detect();
    relay().set_model_mode(&paths, mode, pinned)?;
    Ok(relay().status(&paths))
}

#[tauri::command]
async fn relay_external(on: bool) -> Result<Value, String> {

    relay().set_external_and_run(&Paths::detect(), on)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_state,
            app_version,
            capture_current,
            rename_account,
            delete_account,
            update_account_from_live,
            switch_to,
            get_live_quota,
            get_account_quota,
            claim_preview,
            claim_refresh,
            claim_start,
            claim_captcha_config,
            claim_captcha_submit,
            claim_cancel,
            captcha_show,
            oauth_providers,
            oauth_begin,
            reg_input,
            reg_action,
            reg_set_mode,
            reg_close_window,
            reg_fetch_link,
            mail_precheck,
            captcha_solve,
            pool_list,
            pending_list,
            pending_remove,
            pending_remove_many,
            blocked_list,
            blocked_add,
            blocked_remove,
            pool_import_pick,
            pool_import_text,
            pool_remove,
            pool_remove_many,
            pool_get,
            pool_mark_verified,
            pool_reset,
            pool_upsert,
            outlook_reauth_begin,
            outlook_reauth_poll,
            set_auth_proxy,
            kill_zcode,
            set_behavior,
            set_language,
            autostart_status,
            autostart_set,
            export_one,
            export_all,
            import_files,
            pool_export,
            pick_zcode_path,
            set_zcode_path,
            launch_zcode,
            open_external,
            relay_status,
            relay_refresh,
            relay_policy,
            relay_model_mode,
            relay_external,
            reveal_main,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let paths = Paths::detect();
                    if store::load_settings(&paths).close_to_tray() {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                if window.label() == "captcha" {
                    *pending_guard() = None;
                }
            }
        })
        .setup(|app| {
            i18n::init_from_settings(&store::load_settings(&Paths::detect()));
            if let Ok(data_dir) = app.path().app_local_data_dir() {
                flowlog::init(&data_dir);
            }

            let paths = Paths::detect();

            relay().set_app(app.handle().clone());

            relay().restore_persisted(&paths);
            // ⚠ 设置**只读一次**。以前这里读两遍（先判断开关、再取端口），
            //   两次之间文件若被原子替换，第二遍会读失败 → 回默认值 → 端口悄悄变 8899。
            let st0 = store::load_settings(&paths);
            if st0.relay_external == Some(true) {

                let want_port = st0
                    .relay_port
                    .unwrap_or(gateway::DEFAULT_PORT);
                match relay().start(paths, want_port) {
                    Ok(_) => relay().set_external(true),
                    Err(e) => crate::flowlog::log("relay", "start-fail", &e),
                }
            }
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(app.default_window_icon().expect("no window icon").clone())
                .tooltip("Z·POOL")
                .menu(&tray_menu_inner(app.handle())?)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let id = event.id().as_ref().to_string();
                    match id.as_str() {
                        "show" => show_main(app),
                        "quit" => app.exit(0),
                        other => run_tray_action(app.clone(), other.to_string()),
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
