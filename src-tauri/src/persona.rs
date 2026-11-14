//! 账号级浏览器指纹档案（Persona）。
//!
//! 目的：让每个注册账号在页面 JS 眼里是**另一台机器**，而不是同一台机器
//! 反复出现。所有取值都从「真实存在且互相自洽」的候选集合里派生 ——
//! 屏幕分辨率带任务栏高度、核心数配内存、GPU 字符串带真实 PCI id、
//! 窗口位置落在伪屏幕内。拼出一个物理上不可能的硬件组合，比不伪装更容易被识别。
//!
//! 派生是确定性的：同一 `email` + 同一 install salt 永远得到同一份档案，
//! 这样账号重登、补注册不会因为指纹突变触发二次验证。
//!
//! 不在这里处理的项（有意为之）：
//! - **UA 版本**：必须与 WebView2 运行时版本逐位一致，否则与 `userAgentData`
//!   自相矛盾。注册窗口因此不再覆盖 UA，直接用运行时真实值。
//! - **时区 / 语言**：必须与出口 IP 的地理位置一致，跟着系统走才是自洽的。
//!   要改就得先有 GeoIP 对齐，否则伪造出的「美区时区 + 中国 IP」更可疑。

use serde_json::{json, Value};

// ---------- 确定性随机源 ----------

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(s: &str) -> u64 {
    let mut h = FNV_OFFSET;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// splitmix64。种子经 FNV 折叠后再强制置位，避免全零种子退化成常量序列。
struct Rng(u64);

impl Rng {
    fn new(seed: &str) -> Self {
        Rng(fnv1a(seed) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

// ---------- 候选集合 ----------
//
// 每一项都必须是真实整机会出现的组合，且同组内互相自洽。

/// (屏幕宽, 屏幕高, 任务栏高度)。availWidth == width（任务栏在底部），
/// availHeight = height - 任务栏 —— 实测 Windows 就是这个关系。
const SCREENS: &[(u32, u32, u32)] = &[
    (1920, 1080, 40),
    (1920, 1080, 48),
    (2560, 1440, 48),
    (1366, 768, 40),
    (1536, 864, 40),
    (1600, 900, 40),
    (1440, 900, 40),
    (1680, 1050, 40),
    (1280, 1024, 40),
    (1920, 1200, 40),
    (2560, 1080, 48),
    (3440, 1440, 48),
];

/// (核心数, 内存 GB)。错配（24 核 8G / 4 核 32G）在整机里几乎不存在。
const HW: &[(u32, u32)] = &[
    (4, 8),
    (6, 8),
    (8, 8),
    (8, 16),
    (12, 16),
    (16, 16),
    (16, 32),
    (20, 32),
    (24, 32),
    (12, 32),
    (8, 32),
];

/// (unmasked vendor, unmasked renderer)。renderer 里的 PCI id 与型号对应，
/// 型号之间共享同一代驱动 —— 保持同一厂商可避免暴露「同机不同显卡」。
const GPUS: &[(&str, &str)] = &[
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 (0x00002504) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 3070 (0x00002484) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 (0x00002882) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 3080 (0x00002206) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce GTX 1660 SUPER (0x000021C4) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 2070 SUPER (0x00001E84) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Ti (0x00002489) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
    (
        "Google Inc. (NVIDIA)",
        "ANGLE (NVIDIA, NVIDIA GeForce GTX 1650 (0x00001F82) Direct3D11 vs_5_0 ps_5_0, D3D11)",
    ),
];

/// 注册窗口尺寸。宽度在 560~720 基础上整体翻倍 —— 原来的窄窗把注册表单和滑块
/// 全挤成一列，右侧大片留白却让内容折行。
///
/// 高度不变：下限仍卡在 820，再矮滑块会落到视口外（踩过）。
/// 宽度下限 1120 是翻倍后的对应值；上限 1440 仍能被 1920 宽的伪屏幕装下，
/// 而 `fits` 过滤保证窗口和伪屏幕自洽，不会派生「窗口比屏幕宽」的机器。
const WINDOWS: &[(u32, u32)] = &[
    (1120, 820),
    (1200, 840),
    (1240, 860),
    (1320, 900),
    (1400, 900),
    (1280, 880),
    (1440, 940),
    (1360, 920),
];

/// 语言集合。系统是 zh-CN 时，Chromium 默认给出的就是这一组。
const LANGS: &[&str] = &["zh-CN", "en", "en-GB", "en-US"];

fn system_timezone() -> String {
    #[cfg(not(windows))]
    {
        if let Ok(tz) = iana_time_zone::get_timezone() {
            if !tz.trim().is_empty() {
                return tz;
            }
        }
    }
    // Windows 下没有 iana-time-zone 依赖，用 UTC 偏移映射常见值。
    // 注册链路本身就是国内环境，映射表覆盖不到的一律落回 Asia/Shanghai。
    let offset_min = {
        let now = chrono::Local::now();
        now.offset().local_minus_utc() / 60
    };
    match offset_min {
        480 => "Asia/Shanghai",
        540 => "Asia/Tokyo",
        420 => "Asia/Bangkok",
        0 => "UTC",
        -300 => "America/New_York",
        -480 => "America/Los_Angeles",
        60 => "Europe/Berlin",
        _ => "Asia/Shanghai",
    }
    .to_string()
}

/// 本机真实核心数。
///
/// 派生时**必须把它排除掉**：候选集合里有 16 核 / 32G 这类和本机重合的档位，
/// 一旦 (核心数, 内存, 屏幕) 三元组与本机完全撞上，等于把 YG 的真实设备指纹
/// 原样发给了风控 —— 那比不做伪装更糟，因为它是可关联到真人的。
/// 排除任意一项就能把这种情况的概率压到零，而核心数是唯一能从标准库
/// 零成本拿到的（内存和屏幕要额外 API，收益不抵复杂度）。
fn real_cores() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u32)
}

/// 派生一份账号级指纹档案。
///
/// `salt` 是安装级随机值（每台机器一份），保证同一邮箱在不同机器上不会
/// 撞出同一套指纹。
pub fn derive(email: &str, salt: &str) -> Value {
    let key = format!("{}|{}", email.trim().to_ascii_lowercase(), salt);
    let mut r = Rng::new(&key);

    let rc = real_cores();
    let hw_pool: Vec<&(u32, u32)> = HW
        .iter()
        .filter(|(c, _)| rc.map(|x| *c != x).unwrap_or(true))
        .collect();
    let hw_pool = if hw_pool.is_empty() { HW.iter().collect() } else { hw_pool };
    let (cores, mem) = **r.pick(&hw_pool);
    let (gpu_vendor, gpu_renderer) = *r.pick(GPUS);
    let (ww, wh) = *r.pick(WINDOWS);
    // 屏幕必须能同时容下窗口和任务栏 —— 否则会派生出一台「窗口比屏幕还高」
    // 的机器，这比不做伪装更容易被识别。窗口尺寸先定，屏幕从装得下它的里面挑。
    let fits: Vec<&(u32, u32, u32)> = SCREENS
        .iter()
        .filter(|(sw, sh, taskbar)| *sw >= ww && sh.saturating_sub(*taskbar) >= wh)
        .collect();
    let (sw, sh, taskbar) = **r.pick(&fits);

    // 窗口居中于伪屏幕：这样 screenX/screenY 与 screen.width/height 自洽，
    // 且窗口一定落在伪屏幕可视区内。
    //
    // Y 按**可用区**（扣掉任务栏）居中，不是整屏高：`screenY` 报的是内容区原点，
    // 比 `position()` 给的外框坐标低一个标题栏高度（实测 31px）。按整屏居中会算出
    // `screenY + innerHeight > screen.height` —— 真实机器上不会出现这种窗口。
    let wx = (sw.saturating_sub(ww)) / 2;
    let wy = (sh.saturating_sub(taskbar).saturating_sub(wh)) / 2;

    // canvas / audio 噪声种子：各自独立，避免两类指纹被关联推导。
    let canvas_seed = r.next();
    let audio_seed = r.next();

    json!({
        "seed": key,
        "screen": { "w": sw, "h": sh, "ah": sh.saturating_sub(taskbar) },
        "win": { "w": ww, "h": wh, "x": wx, "y": wy },
        "cores": cores,
        "mem": mem,
        "touch": 0,
        "gpu": { "vendor": gpu_vendor, "renderer": gpu_renderer },
        "canvasSeed": canvas_seed,
        "audioSeed": audio_seed,
        "tz": system_timezone(),
        "langs": LANGS,
        "platform": "Win32",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_email() {
        let a = derive("a@b.com", "salt1");
        let b = derive("a@b.com", "salt1");
        assert_eq!(a, b);
    }

    #[test]
    fn differs_across_accounts() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..40 {
            let v = derive(&format!("user{i}@example.com"), "salt1");
            seen.insert(v["canvasSeed"].as_u64().unwrap());
        }
        // 40 个账号不该出现明显碰撞
        assert!(seen.len() > 35, "canvas seed 重复过多: {}", seen.len());
    }

    #[test]
    fn window_fits_inside_screen() {
        for i in 0..200 {
            let v = derive(&format!("u{i}@e.com"), "s");
            let sw = v["screen"]["w"].as_u64().unwrap();
            let sh = v["screen"]["h"].as_u64().unwrap();
            let ww = v["win"]["w"].as_u64().unwrap();
            let wh = v["win"]["h"].as_u64().unwrap();
            let wx = v["win"]["x"].as_u64().unwrap();
            let wy = v["win"]["y"].as_u64().unwrap();
            assert!(wx + ww <= sw, "窗口横向溢出: {wx}+{ww} > {sw}");
            assert!(wy + wh <= sh, "窗口纵向溢出: {wy}+{wh} > {sh}");
            // screenY 比 position 的外框坐标低一个标题栏（实测 31px）。
            // 按整屏居中会让 screenY + innerHeight 溢出屏幕底部。
            assert!(wy + 40 + wh <= sh, "含标题栏后纵向溢出: {wy}+40+{wh} > {sh}");
            // 还要放得进可用区（扣掉任务栏），否则任务栏会和窗口底边打架
            let ah = v["screen"]["ah"].as_u64().unwrap();
            assert!(wh <= ah, "窗口高过可用区: {wh} > {ah}");
            // 滑块必须落在视口内；宽度下限已随整体翻倍到 1120
            assert!(ww >= 1120 && wh >= 820);
        }
    }

    #[test]
    fn never_matches_host_core_count() {
        // 撞上本机真实核心数会让「核心数/内存/屏幕」有概率与本机完全一致，
        // 那等于把真机指纹发给风控。这条断言锁住这个性质。
        let Some(rc) = real_cores() else { return };
        for i in 0..300 {
            let v = derive(&format!("c{i}@e.com"), "s");
            assert_ne!(
                v["cores"].as_u64().unwrap() as u32, rc,
                "派生出与本机相同的核心数 {rc}"
            );
        }
    }

    #[test]
    fn screen_matches_known_profiles() {
        for i in 0..200 {
            let v = derive(&format!("s{i}@e.com"), "s");
            let w = v["screen"]["w"].as_u64().unwrap() as u32;
            let h = v["screen"]["h"].as_u64().unwrap() as u32;
            assert!(SCREENS.iter().any(|(a, b, _)| *a == w && *b == h));
            // availHeight 必须小于等于屏幕高
            let ah = v["screen"]["ah"].as_u64().unwrap();
            assert!(ah <= v["screen"]["h"].as_u64().unwrap());
        }
    }
}
