//! config.rs —— 限速规则持久化（serde_json）
//!
//! 设计：规则按【进程名】保存（PID 重启后会变，按名字才有持久意义）。
//! - config.json 存 name → (下载KB/s, 上传KB/s)，0 = 不限；
//! - 启动时自动加载，每秒由 reconciler 把名字规则套用到当前活动的新 PID；
//! - UI 修改规则 → 立即对同名活动 PID 生效 + 写回 config.json。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::capture::{is_protected, Rules};
use crate::process_map::{process_alive, process_name};

/// 配置文件里的一条规则
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct NameRule {
    /// 进程名（如 chrome.exe，不区分大小写匹配）
    pub name: String,
    /// 下载限速 KB/s（0 = 不限）
    pub dl_kbps: u64,
    /// 上传限速 KB/s（0 = 不限）
    pub ul_kbps: u64,
    /// 循环周期：限速相位秒数（旧配置没有这个字段 → 默认 0 = 不循环）
    #[serde(default)]
    pub limit_secs: u64,
    /// 循环周期：放行相位秒数
    #[serde(default)]
    pub free_secs: u64,
}

/// 一条内存中的名字规则：双向限速 + 循环周期
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NameLimit {
    /// 下载 B/s（0 = 不限）
    pub dl: u64,
    /// 上传 B/s（0 = 不限）
    pub ul: u64,
    /// 限速相位秒数（0 或与 free 任一为 0 = 不循环）
    pub limit_secs: u64,
    /// 放行相位秒数
    pub free_secs: u64,
}

impl NameLimit {
    /// 是否设置了循环
    pub fn cycles(&self) -> bool {
        self.limit_secs > 0 && self.free_secs > 0
    }
}

/// 配置文件根结构
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    /// 版本号（未来格式迁移用）
    pub version: u32,
    pub rules: Vec<NameRule>,
}

/// 规则仓库：内存中的名字规则 + 持久化 + 与运行时 PID 规则表的同步。
/// 界面线程与 reconciler 共享，内部可变（Mutex）。
pub struct RuleStore {
    /// 运行时规则表（按 PID，抓包热路径用）
    pub rules: Rules,
    /// 名字 → 限速规则（含循环周期）
    name_rules: Mutex<HashMap<String, NameLimit>>,
    config_path: PathBuf,
}

impl RuleStore {
    /// 从 exe 同目录读 config.json（不存在则空配置）
    pub fn load(rules: Rules, config_path: PathBuf) -> Self {
        let mut name_rules = HashMap::new();
        match std::fs::read_to_string(&config_path) {
            Ok(text) => {
                // 容忍 UTF-8 BOM：记事本 / PowerShell 5.1 `Set-Content -Encoding UTF8`
                // 写出的文件都带 BOM，serde_json 会直接解析失败
                let text = text.trim_start_matches('\u{feff}');
                match serde_json::from_str::<Config>(text) {
                    Ok(cfg) => {
                        for r in cfg.rules {
                            // 保护名单（代理/网络关键进程）即使写进了配置也不生效
                            if is_protected(&r.name) {
                                eprintln!("忽略保护名单里的规则：{}", r.name);
                                continue;
                            }
                            // 序列化前已归一化为小写；容错再归一一次
                            name_rules.insert(
                                r.name.to_lowercase(),
                                NameLimit {
                                    dl: r.dl_kbps.saturating_mul(1024),
                                    ul: r.ul_kbps.saturating_mul(1024),
                                    limit_secs: r.limit_secs,
                                    free_secs: r.free_secs,
                                },
                            );
                        }
                        println!(
                            "已加载配置 {}（{} 条规则）",
                            config_path.display(),
                            name_rules.len()
                        );
                    }
                    Err(e) => eprintln!("配置解析失败（忽略，使用空配置）：{e}"),
                }
            }
            Err(_) => {} // 没有配置文件：首次运行
        }
        Self {
            rules,
            name_rules: Mutex::new(name_rules),
            config_path,
        }
    }

    /// 当前名字规则（UI 显示 / 测试用）
    #[allow(dead_code)] // 目前仅单测直接调用；UI 用 get() 逐条查询
    pub fn name_rules(&self) -> HashMap<String, NameLimit> {
        self.name_rules.lock().unwrap().clone()
    }

    /// 查某进程名的规则（含循环周期）
    pub fn get(&self, name: &str) -> Option<NameLimit> {
        self.name_rules.lock().unwrap().get(&name.to_lowercase()).copied()
    }

    /// 设置/更新一条规则：立即对同名活动 PID 生效 + 写回配置文件。
    /// `active` 传当前活动进程列表（pid, name），用于把名字规则落到 PID 规则表。
    /// `limit_secs`/`free_secs` 是循环周期（任一为 0 = 不循环，一直限速）。
    pub fn set(
        &self,
        name: &str,
        dl_kbps: u64,
        ul_kbps: u64,
        limit_secs: u64,
        free_secs: u64,
        active: &[(u32, String)],
    ) -> Result<(), String> {
        let key = name.to_lowercase();
        // 代理/网络关键进程永不限速：误伤会直接断网（FlClash 的 TUN 过整机流量）
        if is_protected(&key) {
            return Err(format!(
                "{name} 在保护名单里（代理/网络关键进程），不允许限速"
            ));
        }
        let dl = dl_kbps.saturating_mul(1024);
        let ul = ul_kbps.saturating_mul(1024);
        {
            let mut g = self.name_rules.lock().unwrap();
            if dl == 0 && ul == 0 {
                g.remove(&key); // 两个方向都不限 = 删除规则
            } else {
                g.insert(
                    key.clone(),
                    NameLimit { dl, ul, limit_secs, free_secs },
                );
            }
        }
        self.apply_to(&key, active);
        self.save()
    }

    /// 对同名活动 PID 套用当前规则（规则不存在则移除其 PID 规则）
    fn apply_to(&self, key: &str, active: &[(u32, String)]) {
        let rule = self.name_rules.lock().unwrap().get(key).copied();
        for (pid, pname) in active {
            if pname.to_lowercase() == key {
                match rule {
                    Some(r) => {
                        self.rules.set(
                            *pid,
                            pname.clone(),
                            r.dl,
                            r.ul,
                            r.limit_secs,
                            r.free_secs,
                        );
                    }
                    None => {
                        self.rules.remove(*pid);
                    }
                }
            }
        }
    }

    /// reconciler：给"名字在规则表里但 PID 还没规则"的活动进程补规则，
    /// 并回收已退出 / PID 被复用（名字对不上）的运行时规则。
    /// 由 UI 每 500ms 驱动一次（新启动的进程自动继承其名字的限速规则）。
    pub fn reconcile(&self, active: &[(u32, String)]) {
        let rules_now: Vec<(String, NameLimit)> = self
            .name_rules
            .lock()
            .unwrap()
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        for (pid, pname) in active {
            let key = pname.to_lowercase();
            if let Some(&(_, r)) = rules_now.iter().find(|(k, _)| *k == key) {
                if (r.dl > 0 || r.ul > 0) && self.rules.get(*pid).is_none() {
                    self.rules.set(
                        *pid,
                        pname.clone(),
                        r.dl,
                        r.ul,
                        r.limit_secs,
                        r.free_secs,
                    );
                }
            }
        }

        // 回收失效的 PID 规则：规则按 PID 存在运行时表里，而 Windows 会复用 PID。
        // 进程退出后不清理，旧规则迟早会落到复用该 PID 的无关进程头上。
        // 判据：① PID 已不存在 → 删；② 进程存活但名字对不上（PID 被复用）→ 删；
        // ③ 无法判断（受保护进程等）→ 保守保留。
        let active_pids: HashSet<u32> = active.iter().map(|(pid, _)| *pid).collect();
        let stale: Vec<(u32, String)> = self
            .rules
            .snapshot()
            .into_iter()
            .filter(|r| !active_pids.contains(&r.pid))
            .map(|r| (r.pid, r.name.clone()))
            .collect();
        for (pid, name) in stale {
            let keep = match process_alive(pid) {
                Some(false) => false, // 进程确实不在了
                // 存活：核对名字，防止 PID 复用后误伤新进程（大小写不敏感）
                Some(true) => process_name(pid).eq_ignore_ascii_case(&name),
                None => true, // 无法判断：保留
            };
            if !keep {
                self.rules.remove(pid);
            }
        }
    }

    /// 写回 config.json（原子写：先写 .tmp 再改名，避免断电写坏）
    pub fn save(&self) -> Result<(), String> {
        let mut rules: Vec<NameRule> = self
            .name_rules
            .lock()
            .unwrap()
            .iter()
            .map(|(name, r)| NameRule {
                name: name.clone(),
                dl_kbps: r.dl / 1024,
                ul_kbps: r.ul / 1024,
                limit_secs: r.limit_secs,
                free_secs: r.free_secs,
            })
            .collect();
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        let cfg = Config { version: 1, rules };
        let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
        let tmp = self.config_path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .and_then(|_| {
                // Windows 上 rename 覆盖已存在文件用 replace 语义
                std::fs::rename(&tmp, &self.config_path)
            })
            .map_err(|e| format!("写入 {} 失败：{e}", self.config_path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_config(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("app_limiter_test_{tag}_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn save_load_roundtrip() {
        let path = tmp_config("roundtrip");
        // 写
        {
            let store = RuleStore::load(Rules::new(), path.clone());
            store.set("Chrome.exe", 300, 50, 0, 0, &[]).unwrap();
            store.set("curl.exe", 0, 20, 0, 0, &[]).unwrap();
            assert!(path.exists(), "save 后配置文件应存在");
        }
        // 读回
        let store = RuleStore::load(Rules::new(), path.clone());
        assert_eq!(store.get("chrome.exe").map(|r| (r.dl, r.ul)), Some((300 * 1024, 50 * 1024))); // 大小写不敏感
        assert_eq!(store.get("CHROME.EXE").map(|r| (r.dl, r.ul)), Some((300 * 1024, 50 * 1024)));
        assert_eq!(store.get("curl.exe").map(|r| (r.dl, r.ul)), Some((0, 20 * 1024)));
        assert_eq!(store.get("notepad.exe"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_zero_removes_rule() {
        let path = tmp_config("zero");
        let store = RuleStore::load(Rules::new(), path.clone());
        store.set("a.exe", 100, 100, 0, 0, &[]).unwrap();
        assert!(store.get("a.exe").is_some());
        store.set("a.exe", 0, 0, 0, 0, &[]).unwrap(); // 全 0 = 删除
        assert!(store.get("a.exe").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_and_reconcile_to_active_pids() {
        let path = tmp_config("reconcile");
        let store = RuleStore::load(Rules::new(), path.clone());
        let active = vec![(11u32, "curl.exe".to_string()), (22, "CURL.EXE".to_string()), (33, "b.exe".to_string())];
        store.set("curl.exe", 8, 0, 0, 0, &active).unwrap();
        // set 时立即对同名 PID（大小写不敏感）生效
        assert_eq!(store.rules.get(11).map(|l| l.dl_limit()), Some(8 * 1024));
        assert_eq!(store.rules.get(22).map(|l| l.dl_limit()), Some(8 * 1024));
        assert!(store.rules.get(33).is_none());
        // 新出现的同名 PID 由 reconcile 补上
        let active2 = vec![(99, "curl.exe".to_string())];
        store.reconcile(&active2);
        assert_eq!(store.rules.get(99).map(|l| l.dl_limit()), Some(8 * 1024));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_config_is_ignored() {
        let path = tmp_config("corrupt");
        std::fs::write(&path, b"{ not json").unwrap();
        let store = RuleStore::load(Rules::new(), path.clone());
        assert!(store.name_rules().is_empty(), "坏配置应回退为空配置");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconcile_prunes_dead_pid() {
        let path = tmp_config("prune_dead");
        let store = RuleStore::load(Rules::new(), path.clone());
        // PID 0 是 Idle 进程：OpenProcess 必然失败（ERROR_INVALID_PARAMETER）→ 视为已退出
        store.rules.set(0, "idle.exe".into(), 8 * 1024, 0, 0, 0);
        store.reconcile(&[]);
        assert!(store.rules.get(0).is_none(), "已退出进程的 PID 规则应被回收");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconcile_keeps_live_matching_pid() {
        let path = tmp_config("prune_live");
        let store = RuleStore::load(Rules::new(), path.clone());
        let pid = std::process::id();
        let real = process_name(pid);
        store.rules.set(pid, real.clone(), 8 * 1024, 0, 0, 0);
        // 进程存活、名字一致：即使暂时没有流量（不在 active 里）也必须保留
        store.reconcile(&[]);
        assert_eq!(store.rules.get(pid).map(|l| l.dl_limit()), Some(8 * 1024));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconcile_prunes_reused_pid() {
        let path = tmp_config("prune_reused");
        let store = RuleStore::load(Rules::new(), path.clone());
        // 当前 PID 配上别的进程名：模拟 PID 被复用（旧规则名与新进程名不符）
        let pid = std::process::id();
        store.rules.set(pid, "definitely_not_us.exe".into(), 8 * 1024, 0, 0, 0);
        store.reconcile(&[]);
        assert!(store.rules.get(pid).is_none(), "PID 复用后旧规则应被回收");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn utf8_bom_config_is_accepted() {
        // 记事本 / PowerShell 5.1 写出的文件带 BOM，必须能读
        let path = tmp_config("bom");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"{\"version\":1,\"rules\":[{\"name\":\"curl.exe\",\"dl_kbps\":8,\"ul_kbps\":0}]}");
        std::fs::write(&path, bytes).unwrap();
        let store = RuleStore::load(Rules::new(), path.clone());
        assert_eq!(
            store.get("curl.exe").map(|r| (r.dl, r.ul)),
            Some((8 * 1024, 0)),
            "带 BOM 的配置应正常解析"
        );
        // 旧配置没有周期字段 → 默认不循环
        assert_eq!(store.get("curl.exe").map(|r| (r.limit_secs, r.free_secs)), Some((0, 0)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cycle_roundtrip_and_old_config_compat() {
        let path = tmp_config("cycle");
        {
            let store = RuleStore::load(Rules::new(), path.clone());
            store.set("game.exe", 8, 0, 600, 30, &[]).unwrap();
            let r = store.get("game.exe").unwrap();
            assert_eq!((r.limit_secs, r.free_secs), (600, 30));
            assert!(r.cycles());
        }
        // 读回：周期必须保留（否则重启后"循环"静默变成"一直限速"）
        let store = RuleStore::load(Rules::new(), path.clone());
        let r = store.get("game.exe").unwrap();
        assert_eq!((r.dl, r.limit_secs, r.free_secs), (8 * 1024, 600, 30));
        let _ = std::fs::remove_file(&path);
    }
}
