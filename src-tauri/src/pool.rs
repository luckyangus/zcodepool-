//! 邮箱池：Outlook（Graph）与任意 IMAP 邮箱的统一存储。
//!
//! 两种导入格式：
//!   `email:password`                                      —— 通用 IMAP，服务器自动寻址
//!   `email----password----client_id----refresh_token`     —— Outlook（Graph API）
//!   `email----password----imap_host----imap_port`         —— IMAP，显式指定服务器
//!
//! 第 3/4 段按内容判别：UUID + refresh token → Outlook；主机名 + 端口 → IMAP。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const STATUS_NEW: &str = "new";
pub const STATUS_VERIFIED: &str = "verified";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_INVALID: &str = "invalid";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MailAccount {
    pub email: String,
    #[serde(default)]
    pub password: String,
    /// Microsoft Graph 的 client_id（仅 Outlook 账号有）
    #[serde(default)]
    pub client_id: String,
    /// Microsoft Graph 的 refresh_token（仅 Outlook 账号有）
    #[serde(default)]
    pub refresh_token: String,
    /// IMAP 端点：自动寻址成功后就地缓存，避免每次注册都重新探测
    #[serde(default)]
    pub imap_host: Option<String>,
    #[serde(default)]
    pub imap_port: Option<u16>,
    #[serde(default)]
    pub imap_user: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub verified_at: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

fn default_status() -> String {
    STATUS_NEW.to_string()
}

#[derive(Serialize, Clone, Debug)]
pub struct Summary {
    pub email: String,
    pub status: String,
    pub verified_at: Option<String>,
    pub note: Option<String>,
    pub has_token: bool,
    pub created_at: String,
    /// `outlook` 走 Graph，`imap` 走自动寻址的通用 IMAP
    pub kind: &'static str,
    pub imap_host: Option<String>,
}

impl MailAccount {
    /// Outlook 账号：有 client_id 与 refresh_token 才能走 Graph API。
    pub fn is_outlook(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.refresh_token.trim().is_empty()
    }

    /// 通用 IMAP 账号：有密码且不是 Outlook。
    pub fn is_imap(&self) -> bool {
        !self.password.trim().is_empty() && !self.is_outlook()
    }

    pub fn kind(&self) -> &'static str {
        if self.is_outlook() {
            "outlook"
        } else {
            "imap"
        }
    }

    /// 已缓存的 IMAP 端点（未探测过则为 None）。
    pub fn endpoint(&self) -> Option<crate::imap::Endpoint> {
        let host = self.imap_host.as_ref()?.trim();
        if host.is_empty() {
            return None;
        }
        Some(crate::imap::Endpoint {
            host: host.to_string(),
            port: self.imap_port.unwrap_or(993),
            user: self
                .imap_user
                .clone()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| self.email.clone()),
        })
    }

    pub fn set_endpoint(&mut self, ep: &crate::imap::Endpoint) {
        self.imap_host = Some(ep.host.clone());
        self.imap_port = Some(ep.port);
        self.imap_user = Some(ep.user.clone());
    }

    pub fn to_summary(&self) -> Summary {
        Summary {
            email: self.email.clone(),
            status: self.status.clone(),
            verified_at: self.verified_at.clone(),
            note: self.note.clone(),
            has_token: !self.refresh_token.trim().is_empty(),
            created_at: self.created_at.clone(),
            kind: self.kind(),
            imap_host: self.imap_host.clone(),
        }
    }
}

/// 账号池的两个区，各自一个文件：
///   `mail`    —— 邮箱库：Outlook 与已验证的取信邮箱
///   `pending` —— 待注册账号：导入 `email:password` 的通用邮箱，等着跑注册
pub const ZONE_MAIL: &str = "mail";
pub const ZONE_PENDING: &str = "pending";

fn zone_file(root: &Path, zone: &str) -> std::path::PathBuf {
    match zone {
        ZONE_PENDING => root.join("pending.json"),
        _ => root.join("mail.json"),
    }
}

fn zone_format(zone: &str) -> &'static str {
    match zone {
        ZONE_PENDING => "zpool-pending-pool",
        _ => "zpool-mail-pool",
    }
}

pub fn load_zone(root: &Path, zone: &str) -> Vec<MailAccount> {
    let raw = match std::fs::read_to_string(zone_file(root, zone)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Array(a)) => {
            serde_json::from_value(serde_json::Value::Array(a)).unwrap_or_default()
        }
        Ok(v) => serde_json::from_value(
            v.get("accounts")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![])),
        )
        .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_zone(root: &Path, zone: &str, accounts: &[MailAccount]) -> Result<(), String> {
    std::fs::create_dir_all(root)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    let doc = serde_json::json!({
        "format": zone_format(zone),
        "version": 2,
        "accounts": accounts,
    });
    let body = serde_json::to_string_pretty(&doc)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    std::fs::write(zone_file(root, zone), body)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))
}

/// 邮箱库（默认区）——保持既有调用点不变。
pub fn load(root: &Path) -> Vec<MailAccount> {
    load_zone(root, ZONE_MAIL)
}

pub fn save(root: &Path, accounts: &[MailAccount]) -> Result<(), String> {
    save_zone(root, ZONE_MAIL, accounts)
}

/// 双区查找，待注册优先（同一邮箱两处都有时，以待注册那份为准）。
pub fn find_any(root: &Path, email: &str) -> Option<(MailAccount, &'static str)> {
    let pending = load_zone(root, ZONE_PENDING);
    if let Some(a) = find(&pending, email) {
        return Some((a.clone(), ZONE_PENDING));
    }
    let mail = load_zone(root, ZONE_MAIL);
    if let Some(a) = find(&mail, email) {
        return Some((a.clone(), ZONE_MAIL));
    }
    None
}

/// 按邮箱改状态：两个区里存在的都改（注册流程不关心账号落在哪个区）。
pub fn set_status_any(
    root: &Path,
    email: &str,
    status: &str,
    note: Option<String>,
) -> Result<(), String> {
    for zone in [ZONE_PENDING, ZONE_MAIL] {
        let mut accounts = load_zone(root, zone);
        if let Some(a) = find_mut(&mut accounts, email) {
            a.status = status.to_string();
            a.note = note.clone();
            if status == STATUS_VERIFIED {
                a.verified_at = Some(crate::store::now_ts());
            }
            save_zone(root, zone, &accounts)?;
        }
    }
    Ok(())
}

pub fn find<'a>(accounts: &'a [MailAccount], email: &str) -> Option<&'a MailAccount> {
    let e = email.trim().to_ascii_lowercase();
    accounts.iter().find(|a| a.email.to_ascii_lowercase() == e)
}

pub fn set_status(
    root: &Path,
    email: &str,
    status: &str,
    note: Option<String>,
) -> Result<(), String> {
    let mut accounts = load(root);
    if let Some(a) = find_mut(&mut accounts, email) {
        a.status = status.to_string();
        a.note = note;
        if status == STATUS_VERIFIED {
            a.verified_at = Some(crate::store::now_ts());
        }
        save(root, &accounts)?;
    }
    Ok(())
}

pub fn find_mut<'a>(accounts: &'a mut [MailAccount], email: &str) -> Option<&'a mut MailAccount> {
    let e = email.trim().to_ascii_lowercase();
    accounts
        .iter_mut()
        .find(|a| a.email.to_ascii_lowercase() == e)
}

fn looks_like_uuid(s: &str) -> bool {
    let s = s.trim();
    s.len() == 36
        && s.chars().filter(|c| *c == '-').count() == 4
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// refresh token 没有统一格式，只做长度 + 常见前缀的宽松判断。
fn looks_like_refresh_token(s: &str) -> bool {
    let s = s.trim();
    s.len() > 40
        && s.chars().all(|c| c.is_ascii_graphic())
        && !s.contains('@')
}

fn looks_like_host(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.contains('.')
        && !s.contains(' ')
        && !s.contains('@')
        && !s.contains(':')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn is_email(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        }
        None => false,
    }
}

/// 解析一行导入文本。支持 `:` 与 `----` 两种分隔，见模块头注释。
pub fn parse_line(line: &str, now: &str) -> Option<MailAccount> {
    let line = line.trim_start_matches('\u{feff}').trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (email, password, client_id, refresh_token, imap_host, imap_port) =
        if line.contains("----") {
            let f: Vec<&str> = line.split("----").map(str::trim).collect();
            let email = f.first().copied().unwrap_or("").to_string();
            let password = f.get(1).copied().unwrap_or("").to_string();
            let f3 = f.get(2).copied().unwrap_or("");
            let f4 = f.get(3).copied().unwrap_or("");
            // 第 3/4 段是「UUID + refresh token」→ Outlook；是「主机名 + 端口」→ IMAP
            if looks_like_host(f3) && !looks_like_uuid(f3) {
                (email, password, String::new(), String::new(), f3.to_string(), f4.parse::<u16>().ok())
            } else {
                (email, password, f3.to_string(), f4.to_string(), String::new(), None)
            }
        } else if let Some((addr, rest)) = line.split_once(':') {
            // 密码可能含 `:`，只在第一个冒号处切
            (addr.trim().to_string(), rest.trim().to_string(), String::new(), String::new(), String::new(), None)
        } else {
            (line.to_string(), String::new(), String::new(), String::new(), String::new(), None)
        };

    if !is_email(&email) {
        return None;
    }
    if password.is_empty() && client_id.is_empty() && refresh_token.is_empty() {
        return None;
    }

    Some(MailAccount {
        email,
        password,
        client_id,
        refresh_token,
        imap_host: if imap_host.trim().is_empty() { None } else { Some(imap_host) },
        imap_port,
        imap_user: None,
        status: STATUS_NEW.to_string(),
        verified_at: None,
        note: None,
        created_at: now.to_string(),
    })
}

pub fn parse_lines(raw: &str, now: &str) -> Vec<MailAccount> {
    raw.lines().filter_map(|l| parse_line(l, now)).collect()
}

/// 已知无法用于注册的邮箱域名。
///
/// 两类：一次性/临时邮箱服务，以及实测被上游拒收的域名。
/// 用户可以在 `~/.zcode-pool/blocked_domains.json` 里追加
/// （数组形式 `["a.com"]`，或 `{"domains": ["a.com"]}`）。
/// 内置黑名单：**留空**。
///
/// 曾经硬编码过一批一次性邮箱服务，但「哪些域名不能注册」随业务变化，
/// 写死在代码里既不准也难维护。全部交给用户在「域名黑名单」里自己维护。
const BLOCKED_DOMAINS: &[&str] = &[];

fn blocked_file(root: &Path) -> std::path::PathBuf {
    root.join("blocked_domains.json")
}

/// 内置黑名单（留空；保留接口是为了以后要加通用名单时有地方放）。
pub fn blocked_builtin() -> Vec<&'static str> {
    BLOCKED_DOMAINS.to_vec()
}

/// 用户自己维护的黑名单。
pub fn blocked_custom(root: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(blocked_file(root)) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let list = v
        .as_array()
        .cloned()
        .or_else(|| v.get("domains").and_then(|x| x.as_array()).cloned())
        .unwrap_or_default();
    list.iter()
        .filter_map(|x| x.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn blocked_write(root: &Path, list: &[String]) -> Result<(), String> {
    std::fs::create_dir_all(root)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    let doc = serde_json::json!({ "domains": list });
    let body = serde_json::to_string_pretty(&doc)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))?;
    std::fs::write(blocked_file(root), body)
        .map_err(|e| crate::i18n::trf("err.pool.write", &[("e", &e.to_string())]))
}

/// 把用户输入收拾成纯域名：邮箱、URL、带端口、大写都能吃。
fn normalize_domain(input: &str) -> String {
    let s = input.trim().to_ascii_lowercase();
    let s = s.trim_start_matches("https://").trim_start_matches("http://");
    let s = s.rsplit('@').next().unwrap_or(""); // 邮箱 → 域名
    let s = s.split('/').next().unwrap_or(""); // 去掉路径
    let s = s.split(':').next().unwrap_or(""); // 去掉端口
    s.trim_matches('.').trim().to_string()
}

pub fn blocked_add(root: &Path, input: &str) -> Result<String, String> {
    let d = normalize_domain(input);
    if d.is_empty() || !d.contains('.') || d.contains(' ') {
        return Err(crate::i18n::trf("err.blocked.bad_domain", &[("input", input.trim())]));
    }
    let mut list = blocked_custom(root);
    if !list.iter().any(|x| x == &d) {
        list.push(d.clone());
        list.sort();
        blocked_write(root, &list)?;
    }
    Ok(d)
}

pub fn blocked_remove(root: &Path, domain: &str) -> Result<(), String> {
    let d = normalize_domain(domain);
    let mut list = blocked_custom(root);
    let before = list.len();
    list.retain(|x| x != &d);
    if list.len() != before {
        blocked_write(root, &list)?;
    }
    Ok(())
}

/// 该邮箱的域名是否在禁注册名单里（内置 + 用户文件）。命中则返回命中的域名。
pub fn domain_blocked(root: &Path, email: &str) -> Option<String> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    // 子域也算命中：mail.emailsl.com 同样被拒
    let hit = |d: &str| {
        let d = d.trim().to_ascii_lowercase();
        !d.is_empty() && (domain == d || domain.ends_with(&format!(".{d}")))
    };
    if let Some(d) = BLOCKED_DOMAINS.iter().find(|d| hit(d)) {
        return Some((*d).to_string());
    }
    if let Ok(raw) = std::fs::read_to_string(blocked_file(root)) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            let list = v
                .as_array()
                .cloned()
                .or_else(|| v.get("domains").and_then(|x| x.as_array()).cloned())
                .unwrap_or_default();
            for item in list {
                if let Some(d) = item.as_str() {
                    if hit(d) {
                        return Some(d.to_string());
                    }
                }
            }
        }
    }
    None
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ImportResult {
    pub added: usize,
    pub skipped: usize,
    pub total_parsed: usize,
    /// 新导入的账号里，需要自动寻址 IMAP 的数量（Outlook 账号不计）
    pub imap_new: usize,
    /// 分流统计：进待注册区的数量 / 进邮箱库的数量
    pub pending_added: usize,
    pub mail_added: usize,
}

pub fn import(accounts: &mut Vec<MailAccount>, raw: &str, now: String) -> ImportResult {
    let parsed = parse_lines(raw, &now);
    let mut r = ImportResult {
        total_parsed: parsed.len(),
        ..Default::default()
    };
    for acc in parsed {
        if find(accounts, &acc.email).is_some() {
            r.skipped += 1;
            continue;
        }
        if acc.is_imap() && acc.imap_host.is_none() {
            r.imap_new += 1;
        }
        accounts.push(acc);
        r.added += 1;
    }
    r
}

/// 导入文本并按格式自动分流：
///   `email:password`（通用邮箱）→ 待注册区
///   四段 `----`（Outlook）      → 邮箱库
/// 任一区已存在的邮箱都算跳过，不会重复入池。
pub fn import_split(root: &Path, raw: &str, now: &str) -> Result<ImportResult, String> {
    let mut pending = load_zone(root, ZONE_PENDING);
    let mut mail = load_zone(root, ZONE_MAIL);
    let mut r = ImportResult::default();
    for line in raw.lines() {
        let Some(acc) = parse_line(line, now) else {
            continue;
        };
        r.total_parsed += 1;
        if find(&pending, &acc.email).is_some() || find(&mail, &acc.email).is_some() {
            r.skipped += 1;
            continue;
        }
        if acc.is_outlook() {
            mail.push(acc);
            r.mail_added += 1;
        } else {
            if acc.imap_host.is_none() {
                r.imap_new += 1;
            }
            pending.push(acc);
            r.pending_added += 1;
        }
        r.added += 1;
    }
    save_zone(root, ZONE_PENDING, &pending)?;
    save_zone(root, ZONE_MAIL, &mail)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> MailAccount {
        parse_line(line, "now").expect("应当解析成功")
    }

    #[test]
    fn parses_colon_format() {
        let a = one("kei_kun@po4.ueda.ne.jp:kei2520");
        assert_eq!(a.email, "kei_kun@po4.ueda.ne.jp");
        assert_eq!(a.password, "kei2520");
        assert!(a.client_id.is_empty());
        assert!(a.is_imap(), "只有地址+密码的账号应判为 IMAP");
        assert!(!a.is_outlook());
        assert_eq!(a.kind(), "imap");
    }

    #[test]
    fn colon_format_keeps_colons_inside_password() {
        let a = one("someone@example.com:pa:ss:word");
        assert_eq!(a.password, "pa:ss:word", "密码里的冒号不能被切掉");
    }

    #[test]
    fn parses_outlook_four_fields() {
        let a = one("someone@outlook.com----pw123----00000000-1111-2222-3333-444444444444----M.C500_SAMPLE_token_value_here_abcdefghijklmnop");
        assert_eq!(a.password, "pw123");
        assert!(a.is_outlook());
        assert_eq!(a.kind(), "outlook");
        assert!(a.imap_host.is_none());
    }

    #[test]
    fn parses_explicit_imap_host() {
        let a = one("someone@example.com----pw----mail.example.com----993");
        assert!(a.is_imap());
        assert_eq!(a.imap_host.as_deref(), Some("mail.example.com"));
        assert_eq!(a.imap_port, Some(993));
        assert!(a.client_id.is_empty());
    }

    #[test]
    fn skips_bom_blank_comment_and_malformed() {
        let raw = "\u{feff}a@b.com:p1\n\n# c\nnot-an-email----pw\nfoo@bar.com\n";
        let v = parse_lines(raw, "now");
        assert_eq!(v.len(), 1, "无密码的行不该入池");
        assert_eq!(v[0].email, "a@b.com");
    }

    #[test]
    fn import_dedups_by_email_keeps_existing() {
        let mut acc = vec![MailAccount {
            email: "a@b.com".into(),
            password: "old".into(),
            client_id: String::new(),
            refresh_token: String::new(),
            imap_host: None,
            imap_port: None,
            imap_user: None,
            status: STATUS_VERIFIED.into(),
            verified_at: None,
            note: None,
            created_at: String::new(),
        }];
        let r = import(&mut acc, "a@b.com:new\nc@d.com:p\n", "now".into());
        assert_eq!(r.added, 1);
        assert_eq!(r.skipped, 1);
        assert_eq!(r.imap_new, 1);
        assert_eq!(acc[0].password, "old", "已存在的不能被覆盖");
    }

    #[test]
    fn endpoint_defaults_to_993_and_full_email() {        let mut a = one("kei_kun@po4.ueda.ne.jp:pw");
        assert!(a.endpoint().is_none(), "未探测前没有端点");
        a.set_endpoint(&crate::imap::Endpoint {
            host: "mail.ueda.ne.jp".into(),
            port: 993,
            user: "kei_kun@po4.ueda.ne.jp".into(),
        });
        let ep = a.endpoint().expect("应当有端点");
        assert_eq!(ep.host, "mail.ueda.ne.jp");
        assert_eq!(ep.port, 993);
        assert_eq!(ep.user, "kei_kun@po4.ueda.ne.jp");
    }

    #[test]
    fn import_split_routes_by_format() {
        let dir = std::env::temp_dir().join(format!("zpool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = "plain@example.com:pw1\n\
                   outlook@example.com----pw2----00000000-1111-2222-3333-444444444444----M.C500_token_value_abcdefghijklmnop\n";
        let r = import_split(&dir, raw, "now").unwrap();
        assert_eq!(r.pending_added, 1, "通用邮箱应进待注册区");
        assert_eq!(r.mail_added, 1, "Outlook 应进邮箱库");
        assert_eq!(load_zone(&dir, ZONE_PENDING).len(), 1);
        assert_eq!(load_zone(&dir, ZONE_MAIL).len(), 1);
        assert_eq!(load_zone(&dir, ZONE_PENDING)[0].email, "plain@example.com");
        assert_eq!(load_zone(&dir, ZONE_MAIL)[0].email, "outlook@example.com");
        // 双查：两个区都能按邮箱找到
        assert_eq!(find_any(&dir, "plain@example.com").unwrap().1, ZONE_PENDING);
        assert_eq!(find_any(&dir, "outlook@example.com").unwrap().1, ZONE_MAIL);
        // 重复导入不重复入池
        let again = import_split(&dir, raw, "now").unwrap();
        assert_eq!(again.added, 0);
        assert_eq!(again.skipped, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
