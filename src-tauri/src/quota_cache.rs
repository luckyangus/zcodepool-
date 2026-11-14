//! 账号库额度快照的落盘缓存。
//!
//! 网关那条路径早就有 `quota-cache.json`（见 `gateway.rs`），账号库这条却一直是
//! 「每次启动全量重拉」：前端 `quotaDue` 是内存变量，`enrollAccounts()` 给每个
//! 账号设 `Date.now()`，于是 N 个号要排 N×8 秒才刷得完，期间界面上一直是空的。
//!
//! 这里补一层同构的缓存。**单独一个文件**，不与网关共用 ——
//! `gateway::write_quota_cache` 是整表覆盖，两边写同一个文件会互相清空。
//!
//! 缓存只影响「展示用的快照」：真正决定能不能发请求的是 `virtual_device_mid`
//! 与凭据，陈旧几秒的额度数字不会让网关多发或少发一次请求。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 新鲜度阈值。只决定「多久以内的快照还能直接拿来显示」——
/// 运行期的刷新节奏由前端 sweepTick 控制（它一律 force），不受这里影响。
///
/// 额度按周期重置，半小时前的数字用于首屏展示完全够用；
/// 定得太短会让「刚重启就又想拉一遍」的老问题回来。
const TTL_MS: u64 = 30 * 60 * 1000;

/// 盘上最多留多少条，防止长期使用后文件无限膨胀。
const MAX_ENTRIES: usize = 200;

/// 当前登录身份的额度用这个 key。账号 id 是 uuid，不会撞上。
pub const LIVE_KEY: &str = "__live__";

fn cache_file(store_dir: &Path) -> PathBuf {
    store_dir.join("account-quota-cache.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load(store_dir: &Path) -> Value {
    fs_read(cache_file(store_dir))
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.get("entries").map(|e| e.is_object()).unwrap_or(false))
        .unwrap_or_else(|| json!({ "entries": {} }))
}

fn fs_read(p: PathBuf) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// 读一条未过期的快照。过期或不存在都返回 `None`，调用方据此决定要不要联网。
pub fn read(store_dir: &Path, id: &str) -> Option<Value> {
    let v = load(store_dir);
    let e = v.get("entries")?.get(id)?;
    let at = e.get("at").and_then(|x| x.as_u64())?;
    if now_ms().saturating_sub(at) > TTL_MS {
        return None;
    }
    e.get("ov").cloned()
}

/// 写回一条快照。读-改-写 + 临时文件原子替换，与网关的做法保持一致。
pub fn write(store_dir: &Path, id: &str, overview: &Value) {
    let mut v = load(store_dir);
    let now = now_ms();
    let Some(entries) = v.get_mut("entries").and_then(|e| e.as_object_mut()) else {
        return;
    };
    entries.insert(id.to_string(), json!({ "at": now, "ov": overview }));

    // 顺手清掉过期项：否则账号删掉以后条目会一直留在盘上。
    let stale: Vec<String> = entries
        .iter()
        .filter(|(_, e)| {
            let at = e.get("at").and_then(|x| x.as_u64()).unwrap_or(0);
            now.saturating_sub(at) > TTL_MS
        })
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        entries.remove(&k);
    }
    // 极端情况兜一层上限：超了就先丢最旧的。
    while entries.len() > MAX_ENTRIES {
        let oldest = entries
            .iter()
            .min_by_key(|(_, e)| e.get("at").and_then(|x| x.as_u64()).unwrap_or(0))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                entries.remove(&k);
            }
            None => break,
        }
    }

    let p = cache_file(store_dir);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zpool-quota-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_isolated_per_account() {
        let d = tmpdir("rt");
        write(&d, "acc-1", &json!({ "a": 1 }));
        write(&d, "acc-2", &json!({ "a": 2 }));
        assert_eq!(read(&d, "acc-1").unwrap()["a"], 1);
        assert_eq!(read(&d, "acc-2").unwrap()["a"], 2);
        assert!(read(&d, "acc-3").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn second_write_keeps_first_entry() {
        // 读-改-写必须保留别的条目：整表覆盖会让并发写的账号互相清空。
        let d = tmpdir("keep");
        write(&d, "a", &json!({ "v": 1 }));
        write(&d, "b", &json!({ "v": 2 }));
        assert!(read(&d, "a").is_some(), "写 b 时把 a 冲掉了");
        assert!(read(&d, "b").is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn expired_entry_is_dropped() {
        let d = tmpdir("exp");
        let p = cache_file(&d);
        let old = now_ms() - TTL_MS - 1000;
        std::fs::write(&p, json!({ "entries": { "old": { "at": old, "ov": { "v": 1 } } } }).to_string())
            .unwrap();
        assert!(read(&d, "old").is_none(), "过期快照不该被读出来");
        // 写一条新的，过期项应被顺手清掉
        write(&d, "new", &json!({ "v": 2 }));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["entries"].get("old").is_none(), "过期项没有被清理");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_file_does_not_panic() {
        let d = tmpdir("bad");
        std::fs::write(cache_file(&d), "{ not json").unwrap();
        assert!(read(&d, "x").is_none());
        write(&d, "x", &json!({ "v": 1 }));
        assert_eq!(read(&d, "x").unwrap()["v"], 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
