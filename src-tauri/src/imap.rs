//! 通用 IMAP 取信：域名自动寻址 → 登录 → 拉取 → 提取激活链接。
//!
//! 寻址按权重分组（借鉴 booking_imap 的成熟做法）：
//!   0 = 已知服务商映射（命中即秒连，前缀拼接对它们是错的）
//!   1 = 前缀拼接 `imap.<域>` / `mail.<域>` / …
//!
//! **本模块相对参考实现的关键增强：逐级上溯父域。**
//! 收件域经常是子域，真实 IMAP 却挂在父域上：实测 `kei_kun@po4.ueda.ne.jp`
//! 的 IMAP 是 `mail.ueda.ne.jp`，而 `imap.po4.ueda.ne.jp` 之类全部连不上。
//! 只拼完整域名的实现（`imap.{domain}`）在这类邮箱上会全盘失败。

use crate::graph::{self, Found};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

/// 单条连接的建立超时。慢服务器要给机会，但不能拖垮整轮发现。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 读写超时：取信可能拉几封大邮件。
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// 同一批内并发探测的主机数。
const MAX_PARALLEL: usize = 5;
/// 最多回看的邮件封数。
const MAX_SCAN: usize = 25;
/// 最多上溯几级父域。
const MAX_SUFFIX_DEPTH: usize = 4;
/// 最多带回几条链接（与 Graph 路径一致）。
const MAX_LINKS: usize = 5;

/// 已知服务商映射。前缀拼接对这些域名是错的（如 icloud → imap.mail.me.com）。
const KNOWN: &[(&str, &str)] = &[
    ("gmail.com", "imap.gmail.com"),
    ("googlemail.com", "imap.gmail.com"),
    ("outlook.com", "outlook.office365.com"),
    ("hotmail.com", "outlook.office365.com"),
    ("live.com", "outlook.office365.com"),
    ("msn.com", "outlook.office365.com"),
    ("icloud.com", "imap.mail.me.com"),
    ("me.com", "imap.mail.me.com"),
    ("mac.com", "imap.mail.me.com"),
    ("yahoo.com", "imap.mail.yahoo.com"),
    ("ymail.com", "imap.mail.yahoo.com"),
    ("aol.com", "imap.aol.com"),
    ("aim.com", "imap.aol.com"),
    ("zoho.com", "imap.zoho.com"),
    ("yandex.com", "imap.yandex.com"),
    ("yandex.ru", "imap.yandex.com"),
    ("mail.ru", "imap.mail.ru"),
    ("inbox.ru", "imap.mail.ru"),
    ("list.ru", "imap.mail.ru"),
    ("bk.ru", "imap.mail.ru"),
    ("rambler.ru", "imap.rambler.ru"),
    ("seznam.cz", "imap.seznam.cz"),
    ("t-online.de", "imap.t-online.de"),
    ("telekom.de", "imap.telekom.de"),
    ("o2.pl", "imap.poczta.o2.pl"),
    ("onet.pl", "imap.poczta.onet.pl"),
    ("interia.pl", "poczta.interia.pl"),
    ("naver.com", "imap.naver.com"),
    ("daum.net", "imap.daum.net"),
    ("hanmail.net", "imap.hanmail.net"),
    ("163.com", "imap.163.com"),
    ("126.com", "imap.126.com"),
    ("yeah.net", "imap.yeah.net"),
    ("qq.com", "imap.qq.com"),
    ("foxmail.com", "imap.qq.com"),
    ("vip.qq.com", "imap.exmail.qq.com"),
    ("exmail.qq.com", "imap.exmail.qq.com"),
    ("sina.com", "imap.sina.com"),
    ("sina.cn", "imap.sina.com"),
    ("sohu.com", "imap.sohu.com"),
    ("aliyun.com", "imap.aliyun.com"),
    ("139.com", "imap.139.com"),
    ("189.cn", "imap.189.cn"),
];

/// 公共后缀：上溯到此为止，再往外就是注册局的地盘（`ne.jp` / `com` …）。
const PUBLIC_SUFFIX: &[&str] = &[
    "com", "net", "org", "edu", "gov", "mil", "int", "info", "biz", "io", "co", "me", "cc", "tv",
    "jp", "cn", "kr", "tw", "hk", "sg", "us", "uk", "de", "fr", "ru", "pl", "cz", "it", "es", "nl",
    "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "ad.jp", "ed.jp", "gr.jp", "lg.jp",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "co.uk", "org.uk", "ac.uk", "gov.uk", "co.kr", "or.kr", "com.tw", "com.hk", "com.sg",
    "com.au", "co.nz", "com.br", "com.mx", "co.in", "com.tr",
];

/// 主机名前缀，按命中概率排序。
const PREFIXES: &[&str] = &["imap", "mail", "webmail", "pop", "smtp", "pop3", "imap-mail"];

/// 可用的 IMAP 端点（已通过登录验证）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// 实际登录成功的用户名（多数服务器要完整邮箱，少数要 @ 前的本地部分）。
    pub user: String,
}

type TlsStream = native_tls::TlsStream<TcpStream>;
type Session = imap::Session<TlsStream>;

/// 宽松 TLS：不少邮件服务器的证书链不完整或 TLS 实现不稳（interia、部分日本小服务商），
/// 严格校验会直接 `UNEXPECTED_EOF_WHILE_READING`。这里只做加密，不做身份校验。
fn tls_connector() -> Result<native_tls::TlsConnector, String> {
    native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .min_protocol_version(Some(native_tls::Protocol::Tlsv10))
        .build()
        .map_err(|e| crate::i18n::trf("err.imap.tls", &[("e", &e.to_string())]))
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| crate::i18n::trf("err.imap.dns", &[("host", host), ("e", &e.to_string())]))?
        .next()
        .ok_or_else(|| crate::i18n::trf("err.imap.dns", &[("host", host), ("e", "no address")]))
}

fn connect_client(host: &str, port: u16) -> Result<imap::Client<TlsStream>, String> {
    let addr = resolve(host, port)?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| crate::i18n::trf("err.imap.connect", &[("host", host), ("e", &e.to_string())]))?;
    let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
    let stream = tls_connector()?
        .connect(host, tcp)
        .map_err(|e| crate::i18n::trf("err.imap.tls", &[("e", &e.to_string())]))?;
    Ok(imap::Client::new(stream))
}

fn connect_and_login(host: &str, port: u16, user: &str, password: &str) -> Result<Session, String> {
    let client = connect_client(host, port)?;
    client
        .login(user, password)
        .map_err(|(e, _)| crate::i18n::trf("err.imap.login", &[("e", &e.to_string())]))
}

/// 该邮箱可能使用的登录名，按命中概率排序。
fn login_users(email: &str) -> Vec<String> {
    let local = email.split('@').next().unwrap_or("").trim();
    let mut v = vec![email.trim().to_string()];
    if !local.is_empty() && local != email.trim() {
        v.push(local.to_string());
    }
    v
}

fn is_public_suffix(s: &str) -> bool {
    PUBLIC_SUFFIX.iter().any(|p| p.eq_ignore_ascii_case(s))
}

/// 域名后缀候选：**父域优先**（邮件服务通常挂在父域，收件域常是主机级子域）。
///
/// `po4.ueda.ne.jp` → `["ueda.ne.jp", "po4.ueda.ne.jp"]`
/// `example.com`    → `["example.com"]`
pub fn domain_suffixes(domain: &str) -> Vec<String> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = d.split('.').filter(|s| !s.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();
    for i in 0..labels.len().min(MAX_SUFFIX_DEPTH) {
        let cand = labels[i..].join(".");
        if is_public_suffix(&cand) {
            break;
        }
        out.push(cand);
    }
    out.reverse();
    out
}

/// 候选主机列表（已去重，顺序即尝试优先级）。
pub fn candidate_hosts(domain: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for suffix in domain_suffixes(domain) {
        if let Some((_, h)) = KNOWN.iter().find(|(d, _)| d.eq_ignore_ascii_case(&suffix)) {
            out.push((*h).to_string());
        }
        for p in PREFIXES {
            out.push(format!("{p}.{suffix}"));
        }
    }
    out.dedup();
    out
}

/// 批量探测：同批并发，任一命中即返回（其余线程自行结束，不等待）。
fn probe_batch(batch: &[String], users: &[String], password: &str) -> Option<Endpoint> {
    let (tx, rx) = mpsc::channel();
    for host in batch {
        let tx = tx.clone();
        let host = host.clone();
        let users = users.to_vec();
        let password = password.to_string();
        std::thread::spawn(move || {
            for user in &users {
                if let Ok(mut s) = connect_and_login(&host, 993, user, &password) {
                    let _ = s.logout();
                    let _ = tx.send(Endpoint { host: host.clone(), port: 993, user: user.clone() });
                    return;
                }
            }
        });
    }
    drop(tx);
    rx.iter().next()
}

/// 自动寻址 + 登录验证。返回第一个可用的端点。
pub fn discover(email: &str, password: &str) -> Result<Endpoint, String> {
    let domain = email
        .rsplit_once('@')
        .map(|(_, d)| d.trim())
        .filter(|d| !d.is_empty())
        .ok_or_else(|| crate::i18n::trf("err.imap.bad_email", &[("email", email)]))?;
    let users = login_users(email);
    let hosts = candidate_hosts(domain);
    if hosts.is_empty() {
        return Err(crate::i18n::trf("err.imap.no_candidate", &[("domain", domain)]));
    }
    for batch in hosts.chunks(MAX_PARALLEL) {
        if let Some(ep) = probe_batch(batch, &users, password) {
            return Ok(ep);
        }
    }
    Err(crate::i18n::trf("err.imap.not_found", &[("domain", domain)]))
}

/// 仅验证登录（邮箱库「验证」按钮用），不拉邮件。
pub fn verify(email: &str, password: &str, known: Option<&Endpoint>) -> Result<Endpoint, String> {
    if let Some(ep) = known {
        for user in login_users(email) {
            if let Ok(mut s) = connect_and_login(&ep.host, ep.port, &user, password) {
                let _ = s.logout();
                return Ok(Endpoint { host: ep.host.clone(), port: ep.port, user });
            }
        }
        // 记录过的端点已失效：继续走自动寻址
    }
    discover(email, password)
}

/// 登录并读一次收件箱，返回邮件总数。证明「能连上」且「能读邮件」。
fn login_and_count(ep: &Endpoint, password: &str) -> Result<usize, String> {
    let mut session = connect_and_login(&ep.host, ep.port, &ep.user, password)?;
    let out = (|| -> Result<usize, String> {
        let mb = session
            .select("INBOX")
            .map_err(|e| crate::i18n::trf("err.imap.select", &[("e", &e.to_string())]))?;
        Ok(mb.exists as usize)
    })();
    let _ = session.logout();
    out
}

/// 注册前的邮箱预检：确认能登录、能打开收件箱。
///
/// 先试缓存端点（顺带验证它还有效），不行再走自动寻址。
/// 返回 `(端点, 收件箱邮件数)`；端点由调用方缓存回账号。
pub fn probe(
    email: &str,
    password: &str,
    known: Option<&Endpoint>,
) -> Result<(Endpoint, usize), String> {
    if let Some(ep) = known {
        if let Ok(n) = login_and_count(ep, password) {
            return Ok((ep.clone(), n));
        }
    }
    let ep = discover(email, password)?;
    let n = login_and_count(&ep, password)?;
    Ok((ep, n))
}

/// 解析一封邮件，取出主题/发件人/时间/HTML/纯文本正文。
struct Parsed {
    subject: String,
    from: String,
    date_ms: Option<i64>,
    html: String,
    text: String,
}

fn header_value(mail: &mailparse::ParsedMail, key: &str) -> String {
    mail.headers
        .iter()
        .find(|h| h.get_key().eq_ignore_ascii_case(key))
        .map(|h| h.get_value())
        .unwrap_or_default()
}

fn collect_body(part: &mailparse::ParsedMail, html: &mut String, text: &mut String) {
    let ctype = part.ctype.mimetype.to_ascii_lowercase();
    if part.subparts.is_empty() {
        if ctype == "text/html" || ctype == "text/plain" {
            if let Ok(body) = part.get_body() {
                if ctype == "text/html" {
                    html.push_str(&body);
                    html.push('\n');
                } else {
                    text.push_str(&body);
                    text.push('\n');
                }
            }
        }
        return;
    }
    for sub in &part.subparts {
        collect_body(sub, html, text);
    }
}

fn parse_mail(raw: &[u8]) -> Option<Parsed> {
    let mail = mailparse::parse_mail(raw).ok()?;
    let subject = header_value(&mail, "Subject");
    let from = header_value(&mail, "From");
    let raw_date = header_value(&mail, "Date");
    let date_ms = chrono::DateTime::parse_from_rfc2822(raw_date.trim())
        .ok()
        .map(|d| d.timestamp_millis());
    let mut html = String::new();
    let mut text = String::new();
    collect_body(&mail, &mut html, &mut text);
    Some(Parsed { subject, from, date_ms, html, text })
}

/// 拉取最近的邮件并提取 `chat.z.ai` 激活链接。
///
/// 返回结构与 Graph 路径一致（`Found`），调用方无需区分两条链路。
pub fn fetch_links(
    ep: &Endpoint,
    password: &str,
    limit: usize,
    since_ms: Option<i64>,
) -> Result<Found, String> {
    let mut session = connect_and_login(&ep.host, ep.port, &ep.user, password)?;
    let result = (|| -> Result<Found, String> {
        session
            .select("INBOX")
            .map_err(|e| crate::i18n::trf("err.imap.select", &[("e", &e.to_string())]))?;
        let mut uids: Vec<u32> = session
            .uid_search("ALL")
            .map_err(|e| crate::i18n::trf("err.imap.search", &[("e", &e.to_string())]))?            .into_iter()
            .collect();
        uids.sort_unstable();

        let take = limit.clamp(1, MAX_SCAN);
        let recent: Vec<u32> = uids.iter().rev().take(take).cloned().collect();

        let mut found = Found {
            links: Vec::new(),
            scanned: 0,
            newest_subject: None,
            newest_at: None,
        };

        for (idx, uid) in recent.iter().enumerate() {
            let set = uid.to_string();
            let fetches = match session.uid_fetch(&set, "(BODY.PEEK[])") {
                Ok(f) => f,
                Err(_) => continue,
            };
            for f in fetches.iter() {
                let Some(raw) = f.body() else { continue };
                let Some(mail) = parse_mail(raw) else { continue };
                found.scanned += 1;
                if idx == 0 && found.newest_subject.is_none() {
                    found.newest_subject = Some(mail.subject.chars().take(80).collect());
                    found.newest_at = mail
                        .date_ms
                        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms))
                        .map(|d| d.to_rfc3339());
                }
                if let (Some(since), Some(at)) = (since_ms, mail.date_ms) {
                    if at < since {
                        continue;
                    }
                }
                let mut hay = mail.html;
                hay.push('\n');
                hay.push_str(&mail.text);
                if hay.len() > 512 * 1024 {
                    hay.truncate(512 * 1024);
                }
                for link in graph::scan_links(&hay) {
                    if !found.links.contains(&link) {
                        found.links.push(link);
                    }
                }
                if found.links.len() >= MAX_LINKS {
                    break;
                }
            }
            if found.links.len() >= MAX_LINKS {
                break;
            }
        }
        Ok(found)
    })();
    let _ = session.logout();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_walk_up_to_parent_first() {
        assert_eq!(
            domain_suffixes("po4.ueda.ne.jp"),
            vec!["ueda.ne.jp".to_string(), "po4.ueda.ne.jp".to_string()]
        );
        assert_eq!(domain_suffixes("example.com"), vec!["example.com".to_string()]);
        assert_eq!(
            domain_suffixes("mail.corp.example.co.uk"),
            vec![
                "example.co.uk".to_string(),
                "corp.example.co.uk".to_string(),
                "mail.corp.example.co.uk".to_string()
            ]
        );
    }

    #[test]
    fn suffixes_stop_before_public_suffix() {
        for d in ["com", "co.jp", "ne.jp", "co.uk"] {
            assert!(domain_suffixes(d).is_empty(), "{d} 不该被当成可注册域");
        }
    }

    #[test]
    fn candidates_include_parent_domain_mail_host() {
        let hosts = candidate_hosts("po4.ueda.ne.jp");
        assert!(hosts.contains(&"mail.ueda.ne.jp".to_string()), "父域的 mail. 前缀必须在内");
        assert!(hosts.contains(&"imap.po4.ueda.ne.jp".to_string()));
    }

    #[test]
    fn known_provider_wins_for_icloud() {
        let hosts = candidate_hosts("icloud.com");
        assert_eq!(hosts[0], "imap.mail.me.com", "已知映射必须排最前");
    }

    #[test]
    fn login_users_prefers_full_address() {
        let u = login_users("kei_kun@po4.ueda.ne.jp");
        assert_eq!(u[0], "kei_kun@po4.ueda.ne.jp");
        assert_eq!(u[1], "kei_kun");
    }
}
