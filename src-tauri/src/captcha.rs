//! 滑块验证码自动求解。
//!
//! 为什么必须是子进程 + CDP：验证码 token 与会话（certifyId）绑定，
//! 在别的浏览器里解出来的结果搬不进注册表单；而 Tauri 的 WebView 里
//! JS 合成事件会被行为分析拒绝（`isTrusted=false`）。所以由 Python 通过
//! CDP 挂到同一个 WebView2 上，用原生鼠标事件拖动 —— 两者都满足。

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 内嵌脚本：与二进制同版本，避免外部文件丢失或版本错配。
const SOLVER_PY: &str = include_str!("../assets/captcha_solver.py");

/// 主窗口的调试端口。它不是注册窗口所在的环境（两者 data_directory 不同、
/// 环境不同），求解器在主窗口里永远找不到 chat.z.ai 页面，只作兜底。
/// 改这里时与 `tauri.conf.json` 的 `additionalBrowserArgs` 保持一致。
const MAIN_CDP_PORT: u16 = 17322;

/// 登录窗口的端口每次注册现取（见 `lib.rs::pick_cdp_port`），所以不能写死。
/// 顺序上先试登录窗口：求解目标只在它里面。
fn cdp_urls(login_port: u16) -> String {
    if login_port == 0 {
        format!("http://127.0.0.1:{MAIN_CDP_PORT}")
    } else {
        format!("http://127.0.0.1:{login_port},http://127.0.0.1:{MAIN_CDP_PORT}")
    }
}

/// 求解上限。保持较短：失败要尽快回到宿主的重试循环，
/// 而不是在这里闷头跑几分钟（那样宿主看起来就是卡死了）。
///
/// 实测单次 4 轮（含展开验证码弹窗）约 20~35 秒；给到 45 秒留余量。
/// 之前是 90 秒 / 8 轮 —— 日志里出现过单次跑满 43 秒还没定位成功的情况，
/// 前端 4 次重试叠起来就是三分钟，用户看到的就是「卡住了」。
const SOLVER_TIMEOUT_SECS: u64 = 45;

/// 单次调用最多提交几次。其余靠「换图筛选 + 宿主重试」补。
/// 4 轮足够覆盖「换图后立刻重定位」，再多的空转不如把控制权交回宿主：
/// 宿主重新调用时会重新 `ensure_open`，弹窗状态有机会被重置。
const MAX_ATTEMPTS: &str = "4";

/// release 版是 windows 子系统（无控制台），子进程默认会**新建一个黑窗**，
/// 每次注册都闪一下。用 CREATE_NO_WINDOW 让 python 静默跑。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 统一走这里起 python，保证不弹控制台。
fn python_command(python: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(python);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// 找一个能用的 Python（同时确认求解所需的三个库都在）。
/// 结果缓存：探测本身也要起进程，不该每次注册都重来一遍。
fn python_exe() -> Result<String, String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let found = CACHE.get_or_init(|| {
        const PROBE: &str = "import playwright, numpy, PIL";
        for cand in ["python", "python3", "py"] {
            let out = python_command(cand)
                .args(["-c", PROBE])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
            if matches!(out, Ok(o) if o.status.success()) {
                return Some(cand.to_string());
            }
        }
        None
    });
    found
        .clone()
        .ok_or_else(|| crate::i18n::tr("err.captcha.python"))
}

/// 把内嵌脚本落盘到数据目录（每次覆盖，保证与当前二进制一致）。
fn write_solver(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| crate::i18n::trf("err.captcha.write", &[("e", &e.to_string())]))?;
    let path = dir.join("captcha_solver.py");
    std::fs::write(&path, SOLVER_PY)
        .map_err(|e| crate::i18n::trf("err.captcha.write", &[("e", &e.to_string())]))?;
    Ok(path)
}

/// 起子进程求解，返回脚本打印的 JSON（`{"ok":bool,...}`）。
///
/// stdout/stderr 各起一个读线程：进程退出后管道里可能还有数据，
/// 直接 `wait_with_output` 在超时被杀的场景下会拿不到已产出的内容。
pub fn solve(store_dir: &Path, login_port: u16) -> Result<Value, String> {
    let python = python_exe()?;
    let script = write_solver(store_dir)?;
    let urls = cdp_urls(login_port);

    let mut child = python_command(&python)
        .arg(&script)
        .arg(&urls)
        .arg(MAX_ATTEMPTS)
        // Windows 上 Python 默认按系统编码（GBK）写 stdout，而 Rust 按 UTF-8 读，
        // 含中文的结果会被读成空串（踩过：表现为「验证码求解没有返回结果」）。
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| crate::i18n::trf("err.captcha.spawn", &[("e", &e.to_string())]))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // 用字节读 + 有损解码：即使子进程吐出的不是合法 UTF-8，也要能看见内容，
    // 而不是把结果吞成空串。
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut h) = stdout {
            let _ = h.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut h) = stderr {
            let _ = h.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    });

    let deadline = Instant::now() + Duration::from_secs(SOLVER_TIMEOUT_SECS);
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(crate::i18n::trf(
                        "err.captcha.timeout",
                        &[("sec", &SOLVER_TIMEOUT_SECS.to_string())],
                    ));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(crate::i18n::trf("err.captcha.spawn", &[("e", &e.to_string())]));
            }
        }
    }

    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    // 无论成败都留一条：含退出码与两侧输出长度，下次出问题不必再猜。
    crate::flowlog::log(
        "captcha",
        "solver-exit",
        &format!(
            "code={:?} stdout={}B stderr={}B {}",
            status.and_then(|s| s.code()),
            out.len(),
            err.len(),
            err.trim().chars().take(300).collect::<String>()
        ),
    );

    // 脚本最后一行是结果 JSON；前面可能有别的东西，逐行找最后一个能解析的。
    for line in out.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            return Ok(v);
        }
    }
    Err(crate::i18n::trf(
        "err.captcha.no_result",
        &[("out", &out.trim().chars().take(200).collect::<String>())],
    ))
}
