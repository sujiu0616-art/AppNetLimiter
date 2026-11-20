//! capture.rs —— WinDivert 抓包 / 双向限速裁决 / 重注入
//!
//! 线程模型：
//! - capture 线程：阻塞在 WinDivertRecv 上，抓到包后立即裁决——
//!   不属于任何被限速进程 → 原样重注入（对其他程序零影响）；
//!   命中规则且对应方向令牌充足 → 立即重注入；
//!   命中规则但令牌不足 → 打上"到期时间"进延迟队列。
//! - scheduler 线程：睡到最近的到期时间，把到期的包重注入。
//! - 退出流程：shutdown(Recv) 打断阻塞的 recv → capture 线程排空驱动队列后退出
//!   → 延迟队列里剩余的包全部立刻重注入（不丢包）→ 关闭句柄。
//!
//! 多进程：规则表是 `RwLock<HashMap<PID, Arc<ProcessLimit>>>`，
//! 每个进程上传/下载各一个令牌桶；增删改规则即时生效，无需重启。
//!
//! 下载整形的关键结论（2026-11-21 反复实测定稿，详见 `charge_dl_seg`）：
//! 入站 TCP 段按**序号去重**后扣额度排队，**重传副本直接丢弃**。
//! 不做去重/不丢副本的话，低速率档的额度会被重传吃掉，1 KB/s 实测会跑到 2 倍甚至归零。
//!
//! 说明：windivert 安全封装的 recv/send 是 &self，但 shutdown/close 是 &mut self，
//! 无法满足跨线程共享 + 优雅退出，所以这里直接使用同一作者的 windivert-sys
//! 原始 API（WinDivert C API 官方文档保证线程安全），自行管理句柄。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;

use windivert::prelude::*; // WinDivertFlags / WinDivertLayer / WinDivertShutdownMode（透传自 windivert-sys）
use windivert_sys::address::WINDIVERT_ADDRESS;
use windivert_sys::{
    WinDivertClose, WinDivertOpen, WinDivertParam, WinDivertRecv, WinDivertSend,
    WinDivertSetParam, WinDivertShutdown,
};
use windows48::Win32::Foundation::HANDLE;

use crate::limiter::{DelayQueue, TokenBucket};
use crate::process_map::{process_name, ProcessMap, PROTO_TCP, PROTO_UDP};

/// 单包缓冲上限：最大 IP 包 65535 + 余量
const MAX_PACKET: usize = 0xFFFF + 64;

/// 驱动收包队列调优（默认 4096 包 / 4MB）：本工具是单线程抓包，
/// 突发流量或调度抖动时队列一满驱动就会丢包（表现为整机网络卡顿/重传）。
/// 这里放宽到 8192 包 / 8MB，队列时间沿用默认 2s。
/// 取值必须在 windivert.h 的 QUEUE_* MIN/MAX 范围内（有单测守着）。
const QUEUE_LEN: u64 = 8192;
const QUEUE_TIME_MS: u64 = 2000;
const QUEUE_SIZE_BYTES: u64 = 8 * 1024 * 1024;

/// 延迟队列上限：包数 / 字节数
const QUEUE_MAX_PACKETS: usize = 8192;
const QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// TCP 序号合理跳跃上限：超过就认为不是同一条连接（端口复用 / 新连接），重新对齐。
/// 10MB 远大于任何正常窗口推进量，又远小于 32 位序号空间，不会误判。
const MAX_SEQ_AHEAD: u32 = 10 * 1024 * 1024;

/// 流表清理阈值：超过这个时间没活动的流条目惰性回收
const FLOW_IDLE_SECS: u64 = 120;

/// 单个进程内允许跟踪的 TCP 流数量上限（超过则清理空闲条目）
const MAX_FLOWS_PER_PROCESS: usize = 512;

/// 过滤器：默认只抓 TCP/UDP 且跳过环回。
/// 设置环境变量 `APP_LIMITER_LOOPBACK=1` 时也抓环回（调试用：本地验证上传/下载路径）。
fn filter_expr() -> &'static str {
    if std::env::var_os("APP_LIMITER_LOOPBACK").is_some() {
        "(tcp or udp)"
    } else {
        "(tcp or udp) and !loopback"
    }
}

/// TCP 流标识：本地端口 + 对端地址 + 对端端口。
/// 只按 PID 记账的话，同一进程的多条连接共用一条时间线，额度会被分到互不相干的流上。
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub(crate) struct FlowKey {
    pub sport: u16,
    pub dport: u16,
    pub v6: bool,
    /// 对端地址（v4 用前 4 字节，其余为 0）
    pub addr: [u8; 16],
}

/// 单条 TCP 流的下载整形状态
struct FlowState {
    /// 已记账到的「下一条期望的 TCP 序号」：用来区分新数据与重传副本
    next_seq: Option<u32>,
    /// 最近一次活动时刻（惰性清理用）
    last: Instant,
}

/// 入站段的整形裁决
pub(crate) enum DlCharge {
    /// 立即放行
    Free,
    /// 等到这个时刻再放行
    Hold(Instant),
    /// 丢弃：重传副本不占额度也不放行（接收端本来也会丢掉重复段）。
    /// 若放行，低速率档的节流会被重传顶穿（实测 1 KB/s 档会跑到 2 倍）。
    Drop,
}

/// 单个进程的限速状态：上传/下载各一个令牌桶
pub struct ProcessLimit {
    pub pid: u32,
    pub name: String,
    /// 下载限速（字节/秒，0=不限）
    dl_bps: AtomicU64,
    /// 上传限速（字节/秒，0=不限）
    ul_bps: AtomicU64,
    /// 循环周期：限速相位持续秒数（0 或与 free_secs 任一为 0 = 不循环，一直限速）
    limit_secs: AtomicU64,
    /// 循环周期：放行相位持续秒数
    free_secs: AtomicU64,
    /// 周期起点（新建规则 / 改规则时重置）
    cycle_start: Mutex<Instant>,
    /// 上一次算出的相位（true = 限速中）。用来在相位切换时做清理。
    phase_active: AtomicBool,
    dl_bucket: Mutex<TokenBucket>,
    ul_bucket: Mutex<TokenBucket>,
    /// 下载整形的流表（仅 TCP：按序号去重后扣额度）
    dl_flows: Mutex<HashMap<FlowKey, FlowState>>,
    /// 已放行的累计字节（用于界面/控制台显示实时速率）
    pub dl_bytes: AtomicU64,
    pub ul_bytes: AtomicU64,
    /// 该进程因重传副本 / 滞留过久 / 队列满被丢弃的包数
    pub dropped: AtomicU64,
}

impl ProcessLimit {
    fn new(pid: u32, name: String, dl_bps: u64, ul_bps: u64) -> Self {
        Self {
            pid,
            name,
            dl_bps: AtomicU64::new(dl_bps),
            ul_bps: AtomicU64::new(ul_bps),
            limit_secs: AtomicU64::new(0),
            free_secs: AtomicU64::new(0),
            cycle_start: Mutex::new(Instant::now()),
            phase_active: AtomicBool::new(true),
            dl_bucket: Mutex::new(TokenBucket::new(dl_bps.max(1))),
            ul_bucket: Mutex::new(TokenBucket::new(ul_bps.max(1))),
            dl_flows: Mutex::new(HashMap::new()),
            dl_bytes: AtomicU64::new(0),
            ul_bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn dl_limit(&self) -> u64 {
        self.dl_bps.load(Ordering::Relaxed)
    }
    pub fn ul_limit(&self) -> u64 {
        self.ul_bps.load(Ordering::Relaxed)
    }
    /// 循环周期：(限速秒, 放行秒)；任一为 0 表示不循环
    pub fn cycle(&self) -> (u64, u64) {
        (self.limit_secs.load(Ordering::Relaxed), self.free_secs.load(Ordering::Relaxed))
    }

    /// 当前是否处于「限速相位」。
    /// 周期 = 限速秒 + 放行秒，从规则生效时刻起算并无限循环；
    /// 两个秒数任一为 0 视为不循环（一直限速）。
    pub fn active_at(&self, now: Instant) -> bool {
        let (l, f) = self.cycle();
        if l == 0 || f == 0 {
            return true;
        }
        let period = l + f;
        let elapsed = now.saturating_duration_since(*self.cycle_start.lock().unwrap()).as_secs();
        elapsed % period < l
    }

    /// 相位切换检测：与上次相比变了就返回 Some(当前是否限速中)。
    /// capture 线程用它决定"刚切到放行相位"时清掉积压包。
    pub fn take_phase_change(&self, now: Instant) -> Option<bool> {
        let active = self.active_at(now);
        let prev = self.phase_active.swap(active, Ordering::Relaxed);
        if prev != active {
            Some(active)
        } else {
            None
        }
    }

    /// (是否限速中, 当前相位还剩多少秒)。不循环时返回 (true, 0)。
    pub fn cycle_state(&self, now: Instant) -> (bool, u64) {
        let (l, f) = self.cycle();
        if l == 0 || f == 0 {
            return (true, 0);
        }
        let period = l + f;
        let elapsed = now.saturating_duration_since(*self.cycle_start.lock().unwrap()).as_secs();
        let phase = elapsed % period;
        if phase < l {
            (true, l - phase)
        } else {
            (false, period - phase)
        }
    }

    /// 重置下载侧的时间线与流表（相位切换时用：旧债务/旧流状态不带到新相位）
    fn reset_dl_clock(&self) {
        let rate = self.dl_bps.load(Ordering::Relaxed).max(1);
        *self.dl_bucket.lock().unwrap() = TokenBucket::new(rate);
        self.dl_flows.lock().unwrap().clear();
    }

    /// 设置循环周期（并重置周期起点）
    fn set_cycle(&self, limit_secs: u64, free_secs: u64) {
        self.limit_secs.store(limit_secs, Ordering::Relaxed);
        self.free_secs.store(free_secs, Ordering::Relaxed);
        *self.cycle_start.lock().unwrap() = Instant::now();
    }

    /// 本进程累计被丢弃的包数（重传副本 / 滞留过久 / 队列满）。
    /// 注意：这是 limiter 层的丢包，与 `DelayQueue::stats().dropped`（队列内部计数）
    /// 是两条不同的路径，界面必须分别展示，否则低速率档的持续丢包会被"丢包 0"掩盖。
    pub fn dropped_so_far(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 下载整形（TCP）：按序号去重后，用令牌桶算出这个包该在什么时候放行。
    ///
    /// 关键点（2026-11-21 实测）：
    /// - **重传副本直接丢弃**：不放行、不占额度。放行的话低速率档会被重传顶穿，
    ///   实测 1 KB/s 档会跑到 2 倍；对这些副本送不送给接收端与 TCP 语义等价
    ///   （接收端本来也会丢弃重复段）。
    /// - 去重保证同一段只扣一次额度；令牌桶的绝对时间线负责长期平均速率。
    ///
    /// 实测达成率（30 s 稳态窗口，逐秒计数）：1 KB/s 95%、2 KB/s 106%、4 KB/s 105%、8 KB/s 101%。
    fn charge_dl_seg(&self, key: FlowKey, seq: u32, payload: usize, now: Instant) -> DlCharge {
        if payload == 0 {
            return DlCharge::Free; // 纯 ACK 不占下载额度
        }
        let mut flows = self.dl_flows.lock().unwrap();
        let st = flows.entry(key).or_insert(FlowState { next_seq: None, last: now });
        st.last = now;
        let end = seq.wrapping_add(payload as u32);
        match st.next_seq {
            None => {
                // 首见这条流：对齐并正常扣额度（首段也是真实数据）
                st.next_seq = Some(end);
            }
            Some(expected) => {
                let ahead = seq.wrapping_sub(expected); // 期望点之后多少（新数据 / 空洞）
                let behind = expected.wrapping_sub(seq); // 期望点之前多少（重传 / 重叠）
                if ahead <= MAX_SEQ_AHEAD || behind > MAX_SEQ_AHEAD {
                    // 新数据，或前后都超出合理范围（端口复用 / 新连接）：重新对齐
                    st.next_seq = Some(end);
                } else {
                    // 重传副本：丢掉（不占额度、不放行）
                    return DlCharge::Drop;
                }
            }
        }
        let due = self.dl_bucket.lock().unwrap().take(payload);
        // 流表惰性清理：连接多、长时间不动的流不回收会一直占内存
        if flows.len() > MAX_FLOWS_PER_PROCESS {
            let idle = std::time::Duration::from_secs(FLOW_IDLE_SECS);
            flows.retain(|_, f| now.saturating_duration_since(f.last) < idle);
        }
        DlCharge::Hold(due)
    }
}

/// 永不限速的进程名（保护名单）：这些进程一被限速，整机网络会立刻异常，
/// 典型是代理客户端（FlClash/mihomo 的 TUN 会把整机流量都过一遍）。
pub const PROTECTED_NAMES: &[&str] = &[
    "flclash.exe",
    "mihomo.exe",
    "clash.exe",
    "clash-meta.exe",
    "clash-verge.exe",
    "clash-verge-service.exe",
];

/// 是否在保护名单里（大小写不敏感）
pub fn is_protected(name: &str) -> bool {
    PROTECTED_NAMES.iter().any(|p| p.eq_ignore_ascii_case(name))
}

/// 规则表：PID → 限速状态。抓包线程每包只读查表，改规则即时生效。
#[derive(Clone)]
pub struct Rules {
    inner: Arc<RwLock<HashMap<u32, Arc<ProcessLimit>>>>,
}

impl Rules {
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// 新增或更新一条规则（立即生效）。返回是否是新增。
    /// `limit_secs`/`free_secs` 为循环周期（任一为 0 = 不循环，一直限速）。
    pub fn set(
        &self,
        pid: u32,
        name: String,
        dl_bps: u64,
        ul_bps: u64,
        limit_secs: u64,
        free_secs: u64,
    ) -> bool {
        // 保护名单兜底：即使上层漏了校验，也不许落到抓包热路径的规则表里
        if is_protected(&name) {
            return false;
        }
        let mut g = self.inner.write().unwrap();
        if let Some(e) = g.get(&pid) {
            e.dl_bps.store(dl_bps, Ordering::Relaxed);
            e.ul_bps.store(ul_bps, Ordering::Relaxed);
            e.set_cycle(limit_secs, free_secs);
            // 速率变化立刻换新桶：旧规则欠的债不带到新规则里
            if dl_bps > 0 {
                *e.dl_bucket.lock().unwrap() = TokenBucket::new(dl_bps);
                // 流表也重新对齐：换了速率还按旧时间线排队没有意义
                e.dl_flows.lock().unwrap().clear();
            }
            if ul_bps > 0 {
                *e.ul_bucket.lock().unwrap() = TokenBucket::new(ul_bps);
            }
            false
        } else {
            let lim = Arc::new(ProcessLimit::new(pid, name, dl_bps, ul_bps));
            lim.set_cycle(limit_secs, free_secs);
            g.insert(pid, lim);
            true
        }
    }

    /// 删除规则（该进程立刻恢复不限速）；返回是否删掉了
    pub fn remove(&self, pid: u32) -> bool {
        self.inner.write().unwrap().remove(&pid).is_some()
    }

    /// 查一条规则（抓包热路径用）
    pub fn get(&self, pid: u32) -> Option<Arc<ProcessLimit>> {
        self.inner.read().unwrap().get(&pid).cloned()
    }

    /// 当前所有规则（显示用）
    pub fn snapshot(&self) -> Vec<Arc<ProcessLimit>> {
        let mut v: Vec<_> = self.inner.read().unwrap().values().cloned().collect();
        v.sort_by_key(|r| r.pid);
        v
    }

    #[allow(dead_code)] // 仅单测使用（命令行版用 snapshot() 判空）
    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

/// 延迟队列元素：原始包数据 + 原始地址 + 归属（重注入后按归属累计字节）
struct PendingPacket {
    data: Vec<u8>,
    addr: WINDIVERT_ADDRESS,
    pid: u32,
    outbound: bool,
}

/// 运行统计（全局）
#[derive(Default)]
pub struct Stats {
    pub seen_pkts: AtomicU64,  // 经过本工具的包总数（≈命中过滤器的包）
    pub seen_bytes: AtomicU64, // 字节总数
    /// 诊断：包能解析但端口在映射表里查不到进程（典型：内核态发包 / 新连接 1s 窗口）
    pub unmapped_pkts: AtomicU64,
    /// 诊断：端口解析失败（非首片分片 / 非 TCP/UDP / IPv6 深扩展头）
    pub unparsed_pkts: AtomicU64,
    /// 诊断：查到进程但该进程没有限速规则（正常直通）
    pub unruled_pkts: AtomicU64,
    /// 诊断：命中规则并进入限速裁决的包
    pub limited_pkts: AtomicU64,
}

/// 单个进程的实时流量计数（fetch_add 无锁；写者只有 capture 线程）
pub struct PidActivity {
    pub dl: AtomicU64, // 累计下载字节（到达计数）
    pub ul: AtomicU64, // 累计上传字节
    last_ms: AtomicU64, // 最近一次计数时刻（自启动毫秒，用于惰性清理）
}

/// 所有"有网络活动"进程的实时计数表：capture 线程写入，UI 线程快照读。
/// 职责：① 给 UI 提供活动进程列表 + 名字 + 实时速率；
///       ② 给 reconciler 提供活动 PID 集合（新进程自动套用名字规则）。
pub struct Activity {
    start: Instant,
    rows: RwLock<HashMap<u32, Arc<PidActivity>>>,
    names: RwLock<HashMap<u32, String>>,
}

/// Activity 快照行
#[derive(Clone)]
pub struct ActivityRow {
    pub pid: u32,
    pub name: String,
    pub dl: u64,
    pub ul: u64,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            rows: RwLock::new(HashMap::new()),
            names: RwLock::new(HashMap::new()),
        }
    }

    /// capture 线程每包调用：计数；首见 PID 时查一次进程名并缓存
    pub fn bump(&self, pid: u32, outbound: bool, n: usize) {
        let now_ms = self.start.elapsed().as_millis() as u64;
        let row = {
            let mut g = self.rows.write().unwrap(); // 唯一写者，无竞争
            g.entry(pid)
                .or_insert_with(|| {
                    // 首见：查名字（可能失败，退化为 <pid N>）
                    let name = process_name(pid);
                    self.names.write().unwrap().insert(pid, name);
                    Arc::new(PidActivity {
                        dl: AtomicU64::new(0),
                        ul: AtomicU64::new(0),
                        last_ms: AtomicU64::new(now_ms),
                    })
                })
                .clone()
        };
        let counter = if outbound { &row.ul } else { &row.dl };
        counter.fetch_add(n as u64, Ordering::Relaxed);
        row.last_ms.store(now_ms, Ordering::Relaxed);
    }

    /// UI 快照：(pid, name, dl, ul)。顺带惰性清理超过 2 分钟无活动的行。
    pub fn snapshot(&self) -> Vec<ActivityRow> {
        let now_ms = self.start.elapsed().as_millis() as u64;
        let rows = self.rows.read().unwrap().clone();
        let names = self.names.read().unwrap().clone();
        let mut out = Vec::with_capacity(rows.len());
        for (pid, row) in rows {
            let idle = now_ms.saturating_sub(row.last_ms.load(Ordering::Relaxed));
            if idle > 120_000 {
                // 超过 2 分钟没有流量：清理（names 一起清，防止已退出进程常驻列表）
                self.rows.write().unwrap().remove(&pid);
                self.names.write().unwrap().remove(&pid);
                continue;
            }
            out.push(ActivityRow {
                pid,
                name: names.get(&pid).cloned().unwrap_or_else(|| format!("<pid {pid}>")),
                dl: row.dl.load(Ordering::Relaxed),
                ul: row.ul.load(Ordering::Relaxed),
            });
        }
        out
    }
}

/// WinDivert 句柄封装。
/// 安全性说明：HANDLE 是内核对象句柄，WinDivert 的 C API 明确支持多线程
/// 并发调用（Recv/Send 各自独立完成内核 IO），因此手动声明 Send/Sync 是合理的。
pub struct DivertHandle(HANDLE);

unsafe impl Send for DivertHandle {}
unsafe impl Sync for DivertHandle {}

impl DivertHandle {
    /// 打开 NETWORK 层句柄（需要管理员权限，否则错误码 5）
    fn open() -> Result<Self, String> {
        let filter = std::ffi::CString::new(filter_expr()).unwrap();
        unsafe {
            let h = WinDivertOpen(
                filter.as_ptr(),
                WinDivertLayer::Network,
                0,                     // 优先级 0：默认
                WinDivertFlags::new(), // 不设 sniff/drop —— 抓到的包必须重注入
            );
            if h.is_invalid() {
                let e = std::io::Error::last_os_error();
                Err(match e.raw_os_error() {
                    Some(5) => format!("WinDivertOpen 失败（拒绝访问）：请以管理员身份运行。{e}"),
                    Some(2) | Some(126) => format!(
                        "WinDivertOpen 失败：找不到 WinDivert.dll，请把它和 WinDivert64.sys 放到 exe 同目录。{e}"
                    ),
                    _ => format!("WinDivertOpen 失败：{e}"),
                })
            } else {
                tune_queue_params(h);
                Ok(Self(h))
            }
        }
    }

    /// 阻塞收包；返回 (长度, 地址)。失败/被 shutdown 打断时返回 None。
    fn recv(&self, buf: &mut [u8]) -> Option<(usize, WINDIVERT_ADDRESS)> {
        unsafe {
            let mut addr: WINDIVERT_ADDRESS = std::mem::zeroed();
            let mut n: u32 = 0;
            let ok = WinDivertRecv(
                self.0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut n,
                &mut addr,
            );
            if ok.as_bool() {
                Some((n as usize, addr))
            } else {
                None
            }
        }
    }

    /// 重注入包（数据与地址必须原样）
    fn send(&self, data: &[u8], addr: &WINDIVERT_ADDRESS) -> bool {
        unsafe {
            let mut sent: u32 = 0;
            let ok = WinDivertSend(
                self.0,
                data.as_ptr() as *const _,
                data.len() as u32,
                &mut sent,
                addr,
            );
            ok.as_bool() && sent as usize == data.len()
        }
    }

    /// 打断阻塞中的 recv（只停收包方向，发送仍可用，便于退出时冲刷队列）
    fn shutdown_recv(&self) {
        unsafe {
            WinDivertShutdown(self.0, WinDivertShutdownMode::Recv);
        }
    }
}

impl Drop for DivertHandle {
    fn drop(&mut self) {
        // 关闭句柄；此后驱动不再拦截任何包，网络完全恢复
        unsafe {
            WinDivertClose(self.0);
        }
    }
}

/// 打开句柄后调优驱动收包队列（见 QUEUE_* 常量）。
/// 失败不是致命错误（例如加载了不支持 QUEUE_SIZE 的旧版 DLL），仅提示。
fn tune_queue_params(h: HANDLE) {
    unsafe {
        let params = [
            ("QUEUE_LENGTH", WinDivertParam::QueueLength, QUEUE_LEN),
            ("QUEUE_TIME", WinDivertParam::QueueTime, QUEUE_TIME_MS),
            ("QUEUE_SIZE", WinDivertParam::QueueSize, QUEUE_SIZE_BYTES),
        ];
        for (name, param, value) in params {
            if !WinDivertSetParam(h, param, value).as_bool() {
                let e = std::io::Error::last_os_error();
                eprintln!("WinDivertSetParam({name}={value}) 失败（忽略）：{e}");
            }
        }
    }
}

/// 抓包服务：管理两个工作线程与规则表
pub struct Capture {
    handle: Arc<DivertHandle>,
    stop: Arc<AtomicBool>,
    queue: Arc<DelayQueue<PendingPacket>>,
    #[allow(dead_code)] // 第三步界面会直接读规则表做列表渲染
    pub rules: Rules,
    /// 全部活动进程的实时计数（UI 数据源；抓包线程在裁决前统一计数）
    pub activity: Arc<Activity>,
    #[allow(dead_code)] // 全局统计（第三步界面展示总量）
    pub stats: Arc<Stats>,
    threads: Vec<JoinHandle<()>>,
}

impl Capture {
    /// 打开句柄并启动 capture / scheduler 两个线程
    pub fn start(pmap: &Arc<ProcessMap>, rules: Rules) -> Result<Self, String> {
        let handle = Arc::new(DivertHandle::open()?);
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(DelayQueue::new(QUEUE_MAX_PACKETS, QUEUE_MAX_BYTES));
        let stats = Arc::new(Stats::default());
        let activity = Arc::new(Activity::new());

        // ---- capture 线程：阻塞收包 + 立即裁决 ----
        let cap = {
            let handle = handle.clone();
            let stop = stop.clone();
            let queue = queue.clone();
            let stats = stats.clone();
            let activity = activity.clone();
            let rules = rules.clone();
            let pmap = pmap.clone();
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || {
                    // 精度优化：放行"其他进程"包的关键路径，略提优先级防止被饿着
                    unsafe {
                        let _ = windows::Win32::System::Threading::SetThreadPriority(
                            windows::Win32::System::Threading::GetCurrentThread(),
                            windows::Win32::System::Threading::THREAD_PRIORITY_ABOVE_NORMAL,
                        );
                    }
                    let mut buf = vec![0u8; MAX_PACKET];
                    // 退出排空：recv 失败时若 stop 已置位，说明是被 shutdown 打断，
                    // 需要再收若干次把驱动队列里已入队的包排完（shutdown 后空队列
                    // recv 立即失败，连续第二次失败即认为排空）。
                    let mut recv_failed = false;
                    loop {
                        let Some((n, addr)) = handle.recv(&mut buf) else {
                            if !stop.load(Ordering::Relaxed) || recv_failed {
                                break; // 真错误 / 已排空
                            }
                            recv_failed = true;
                            continue; // 被 shutdown 打断：再收一次
                        };
                        recv_failed = false;
                        let data = &buf[..n];
                        stats.seen_pkts.fetch_add(1, Ordering::Relaxed);
                        stats.seen_bytes.fetch_add(n as u64, Ordering::Relaxed);

                        let outbound = addr.outbound();
                        let mut handled = false;

                        // 1) 端口 → PID（本地端口：出站=源端口，入站=目的端口）
                        let parsed = classify_local(data, outbound);
                        if parsed.is_none() {
                            stats.unparsed_pkts.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some((proto, v6, port)) = parsed {
                            match pmap.lookup(proto, v6, port) {
                                Some(pid) => {
                                    // 所有能定位到进程的包都计入实时速率（UI 数据源）
                                    activity.bump(pid, outbound, n);
                                    // 2) 查规则；命中才走整形
                                    match rules.get(pid) {
                                        None => {
                                            stats.unruled_pkts.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Some(lim) => {
                                            let now = Instant::now();
                                            // 循环周期：放行相位里两个方向都不整形（等价于"此刻不限速"）
                                            let active = lim.active_at(now);
                                            let (cl, cf) = lim.cycle();
                                            if cl > 0 && cf > 0 {
                                                if let Some(is_active) = lim.take_phase_change(now) {
                                                    if !is_active {
                                                        // 刚切到放行相位：清掉排队积压 + 重置时间线。
                                                        // 不清的话积压会继续按旧到期时刻慢放，放行相位放不干净
                                                        // （实测 1 KB/s + 6s限/4s放：队列积压 190 包 / 273 KB）。
                                                        let purged: usize =
                                                            queue.purge_where(|p: &PendingPacket| p.pid == pid);
                                                        if purged > 0 {
                                                            lim.dropped.fetch_add(
                                                                purged as u64,
                                                                Ordering::Relaxed,
                                                            );
                                                        }
                                                    }
                                                    // 两个方向切换都把下载时间线归零
                                                    lim.reset_dl_clock();
                                                }
                                            }
                                            let ul_rate = if active { lim.ul_limit() } else { 0 };
                                            let dl_rate = if active { lim.dl_limit() } else { 0 };
                                            // 整形策略（2026-11-21 定稿）：
                                            // - 上传：出站包按自身字节过令牌桶
                                            // - 下载(TCP)：入站段按序号去重后排队；重传副本直接丢
                                            // - 下载(UDP)：没有序号可去重，按包字节过桶
                                            let shape_ul = outbound && ul_rate > 0;
                                            let shape_dl = !outbound && dl_rate > 0;

                                            if !(shape_ul || shape_dl) {
                                                // 规则存在但该方向不限速：直通，但仍要计入本方向流量，
                                                // 否则「只限下载」的进程在界面上传速率会恒为 0
                                                // （UI 对有规则的进程统一用 lim 的计数，见 ui.rs sample）
                                                let c = if outbound {
                                                    &lim.ul_bytes
                                                } else {
                                                    &lim.dl_bytes
                                                };
                                                c.fetch_add(n as u64, Ordering::Relaxed);
                                                stats.unruled_pkts.fetch_add(1, Ordering::Relaxed);
                                            } else {
                                                handled = true;
                                                stats.limited_pkts
                                                    .fetch_add(1, Ordering::Relaxed);

                                                if outbound {
                                                    // 上传：令牌桶按包字节排队
                                                    let due = lim.ul_bucket.lock().unwrap().take(n);
                                                    if due <= now {
                                                        handle.send(data, &addr);
                                                        lim.ul_bytes.fetch_add(
                                                            n as u64,
                                                            Ordering::Relaxed,
                                                        );
                                                    } else {
                                                        let pkt = PendingPacket {
                                                            data: data.to_vec(),
                                                            addr,
                                                            pid,
                                                            outbound,
                                                        };
                                                        if !queue.push(pkt, n, due) {
                                                            // 队列满：退还额度后直接放行，不能把连接卡死
                                                            lim.ul_bucket
                                                                .lock()
                                                                .unwrap()
                                                                .refund(n);
                                                            lim.dropped.fetch_add(
                                                                1,
                                                                Ordering::Relaxed,
                                                            );
                                                            handle.send(data, &addr);
                                                        }
                                                    }
                                                } else if proto == PROTO_TCP {
                                                    // 下载(TCP)：去重 → 新数据扣额度并延迟放行；重传副本直接丢
                                                    let mut hold: Option<Instant> = None;
                                                    let mut drop_pkt = false;
                                                    if let Some(t) = parse_tcp(data, false) {
                                                        match lim.charge_dl_seg(
                                                            t.key, t.seq, t.payload, now,
                                                        ) {
                                                            DlCharge::Hold(due) => {
                                                                if due > now {
                                                                    hold = Some(due);
                                                                }
                                                            }
                                                            DlCharge::Drop => drop_pkt = true,
                                                            DlCharge::Free => {}
                                                        }
                                                    }
                                                    if drop_pkt {
                                                        lim.dropped.fetch_add(1, Ordering::Relaxed);
                                                    } else {
                                                        match hold {
                                                            None => {
                                                                handle.send(data, &addr);
                                                                lim.dl_bytes.fetch_add(
                                                                    n as u64,
                                                                    Ordering::Relaxed,
                                                                );
                                                            }
                                                            Some(due) => {
                                                                let pkt = PendingPacket {
                                                                    data: data.to_vec(),
                                                                    addr,
                                                                    pid,
                                                                    outbound,
                                                                };
                                                                if !queue.push(pkt, n, due) {
                                                                    lim.dropped.fetch_add(
                                                                        1,
                                                                        Ordering::Relaxed,
                                                                    );
                                                                    handle.send(data, &addr);
                                                                }
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    // 下载(UDP)：没有序号可去重，按包字节过桶
                                                    let due = lim.dl_bucket.lock().unwrap().take(n);
                                                    if due <= now {
                                                        handle.send(data, &addr);
                                                        lim.dl_bytes.fetch_add(
                                                            n as u64,
                                                            Ordering::Relaxed,
                                                        );
                                                    } else {
                                                        let pkt = PendingPacket {
                                                            data: data.to_vec(),
                                                            addr,
                                                            pid,
                                                            outbound,
                                                        };
                                                        if !queue.push(pkt, n, due) {
                                                            lim.dl_bucket
                                                                .lock()
                                                                .unwrap()
                                                                .refund(n);
                                                            lim.dropped.fetch_add(
                                                                1,
                                                                Ordering::Relaxed,
                                                            );
                                                            handle.send(data, &addr);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                None => {
                                    stats.unmapped_pkts.fetch_add(1, Ordering::Relaxed);
                                    // 新连接首包通常比端口表早到：请求刷新线程立即补拉一次，
                                    // 让后续包尽快命中规则，而不是等最长 1s 的周期刷新
                                    pmap.request_refresh();
                                }
                            }
                        }

                        // 3) 其他流量：原样立即放行，不影响别的程序
                        if !handled {
                            handle.send(data, &addr);
                        }
                    }
                })
                .expect("spawn capture")
        };

        // ---- scheduler 线程：到点重注入 ----
        let sch = {
            let handle = handle.clone();
            let queue = queue.clone();
            let rules = rules.clone();
            std::thread::Builder::new()
                .name("scheduler".into())
                .spawn(move || {
                    // 精度优化：调度线程高优先级，降低被抢占造成的放行抖动
                    // （不用 TIME_CRITICAL，避免极端情况下饿死内核线程）
                    unsafe {
                        let _ = windows::Win32::System::Threading::SetThreadPriority(
                            windows::Win32::System::Threading::GetCurrentThread(),
                            windows::Win32::System::Threading::THREAD_PRIORITY_HIGHEST,
                        );
                    }
                    // pop_due 在队列关闭后返回 None → 线程自然退出
                    while let Some(pkt) = queue.pop_due() {
                        handle.send(&pkt.data, &pkt.addr);
                        if let Some(lim) = rules.get(pkt.pid) {
                            let c = if pkt.outbound {
                                &lim.ul_bytes
                            } else {
                                &lim.dl_bytes
                            };
                            c.fetch_add(pkt.data.len() as u64, Ordering::Relaxed);
                        }
                    }
                })
                .expect("spawn scheduler")
        };

        Ok(Self {
            handle,
            stop,
            queue,
            rules,
            activity,
            stats,
            threads: vec![cap, sch],
        })
    }

    /// 优雅退出：打断收包 → 等 capture 线程排空驱动队列 → 队列冲刷放行 → join。
    /// 顺序很关键：若在 capture 线程退出前就关闭延迟队列，它排空期间入队的
    /// 限速包会 push 失败被丢弃；先 join 再冲刷则退出路径零丢包。
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.shutdown_recv(); // capture 线程：排空驱动队列后退出

        // 先等 capture 线程退出（threads[0] 是 capture）
        if !self.threads.is_empty() {
            let cap = self.threads.swap_remove(0);
            let _ = cap.join();
        }

        // 队列关闭并取回所有滞留包，立刻全部重注入（尽量不丢包）
        for pkt in self.queue.close_and_drain() {
            self.handle.send(&pkt.data, &pkt.addr);
        }

        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // self.handle 是最后一个 Arc 引用，drop 时关闭 WinDivert 句柄
    }

    /// 延迟队列快照：(排队包数, 排队字节数, 累计丢弃包数)
    pub fn queue_stats(&self) -> (usize, usize, u64) {
        self.queue.stats()
    }

    /// 是否已收到停止信号（供辅助线程判断退出）
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// 从 IP 包解析 (协议, 是否 IPv6, 本地端口)。
/// 出站包的本地端口 = 源端口；入站包的本地端口 = 目的端口。
/// 解析不出来（分片非首片、ICMP、扩展头过深等）返回 None —— 按不限速放行。
fn classify_local(data: &[u8], outbound: bool) -> Option<(u8, bool, u16)> {
    let (proto, v6, sport, dport) = parse_ports(data)?;
    let local = if outbound { sport } else { dport };
    Some((proto, v6, local))
}

/// IP 头解析结果：传输层头偏移 + 源/目的地址在包内的位置。
/// 端口解析和 TCP 段解析共用这一段（避免两处各写一遍扩展头遍历，改漏一处就是坑）。
pub(crate) struct IpLayout {
    pub v6: bool,
    /// 传输层协议号（6=TCP / 17=UDP）
    pub proto: u8,
    /// 传输层头在包内的偏移
    pub l4: usize,
    /// 源地址偏移（入站段的对端地址）
    pub src: usize,
    /// 目的地址偏移（出站段的对端地址）
    pub dst: usize,
    /// 地址长度（4 或 16）
    pub addr_len: usize,
}

pub(crate) fn ip_layout(data: &[u8]) -> Option<IpLayout> {
    if data.is_empty() {
        return None;
    }
    match data[0] >> 4 {
        4 => {
            // IPv4：IHL 定位传输层头
            if data.len() < 20 {
                return None;
            }
            let ihl = (data[0] & 0x0F) as usize * 4;
            if ihl < 20 || data.len() < ihl + 4 {
                return None;
            }
            // Flags + Fragment Offset 在 IP 头偏移 6..8（注意：偏移 2..4 是总长度）
            let frag = u16::from_be_bytes([data[6], data[7]]);
            if frag & 0x1FFF != 0 {
                return None; // 非首片分片没有端口
            }
            let proto = data[9];
            if proto != PROTO_TCP && proto != PROTO_UDP {
                return None;
            }
            Some(IpLayout { v6: false, proto, l4: ihl, src: 12, dst: 16, addr_len: 4 })
        }
        6 => {
            // IPv6：固定 40 字节头 + 可选扩展头
            if data.len() < 48 {
                return None;
            }
            let mut next = data[6];
            let mut off = 40usize;
            // 最多走 8 层扩展头，防畸形包死循环
            for _ in 0..8 {
                match next {
                    6 | 17 => break, // TCP / UDP 头就在眼前
                    0 | 43 | 60 => {
                        // Hop-by-Hop / Routing / Dest-Options：长度 = (n+1)*8
                        if data.len() < off + 2 {
                            return None;
                        }
                        next = data[off];
                        off += (data[off + 1] as usize + 1) * 8;
                    }
                    44 => {
                        // Fragment：固定 8 字节（next header, reserved, offset+flags, id）
                        if data.len() < off + 8 {
                            return None;
                        }
                        // 非首片分片的载荷不是传输层头，那 4 个字节不是端口；
                        // 与 IPv4 的处理保持一致：只有首片（offset == 0）能解析端口。
                        // 字段布局：高 13 位是 fragment offset，低 3 位是保留位 + M 标志。
                        let frag_off = u16::from_be_bytes([data[off + 2], data[off + 3]]) >> 3;
                        if frag_off != 0 {
                            return None;
                        }
                        next = data[off];
                        off += 8;
                    }
                    _ => return None, // AH/ESP 等：放弃解析，放行
                }
            }
            if next != 6 && next != 17 {
                return None;
            }
            if data.len() < off + 4 {
                return None;
            }
            Some(IpLayout { v6: true, proto: next, l4: off, src: 8, dst: 24, addr_len: 16 })
        }
        _ => None,
    }
}

/// 纯解析：返回 (协议, v6, 源端口, 目的端口)
fn parse_ports(data: &[u8]) -> Option<(u8, bool, u16, u16)> {
    let l = ip_layout(data)?;
    let sport = u16::from_be_bytes([data[l.l4], data[l.l4 + 1]]);
    let dport = u16::from_be_bytes([data[l.l4 + 2], data[l.l4 + 3]]);
    Some((l.proto, l.v6, sport, dport))
}

/// TCP 段的解析结果（下载整形用：入站扣额度、去重）
pub(crate) struct TcpInfo {
    /// 流键：本地端口 + 对端地址 + 对端端口（入站/出站两个方向映射到同一个键）
    pub key: FlowKey,
    /// TCP 序号（载荷首字节序号）
    pub seq: u32,
    /// 载荷字节数（不含 TCP 头；纯 ACK 为 0）
    pub payload: usize,
}

/// 解析 TCP 段。`outbound` 决定流键方向（本地端口取哪一侧、对端地址取哪个）。
/// 解析不出来（TCP 头不完整 / 非 TCP）返回 None → 该包按原样放行。
pub(crate) fn parse_tcp(data: &[u8], outbound: bool) -> Option<TcpInfo> {
    let l = ip_layout(data)?;
    if l.proto != PROTO_TCP {
        return None;
    }
    if data.len() < l.l4 + 20 {
        return None; // 连 TCP 固定头都放不下
    }
    // 数据偏移在 TCP 头第 12 字节高 4 位（单位 4 字节）
    let data_off = ((data[l.l4 + 12] >> 4) as usize) * 4;
    if data_off < 20 || data.len() < l.l4 + data_off {
        return None;
    }
    let sport = u16::from_be_bytes([data[l.l4], data[l.l4 + 1]]);
    let dport = u16::from_be_bytes([data[l.l4 + 2], data[l.l4 + 3]]);
    let seq = u32::from_be_bytes([
        data[l.l4 + 4],
        data[l.l4 + 5],
        data[l.l4 + 6],
        data[l.l4 + 7],
    ]);
    // 流键统一成「本地端口 + 对端地址/端口」，两个方向落到同一个键上
    let (local_port, remote_port) = if outbound { (sport, dport) } else { (dport, sport) };
    let addr_off = if outbound { l.dst } else { l.src };
    let mut addr = [0u8; 16];
    addr[..l.addr_len].copy_from_slice(&data[addr_off..addr_off + l.addr_len]);
    Some(TcpInfo {
        key: FlowKey { sport: local_port, dport: remote_port, v6: l.v6, addr },
        seq,
        payload: data.len() - l.l4 - data_off,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tcp_v4(sport: u16, dport: u16) -> Vec<u8> {
        // 20B IP 头（proto=6）+ 4B 端口（其余省略，够解析即可）
        let mut p = vec![0u8; 24];
        p[0] = 0x45; // v4, IHL=5
        p[2] = 24; // total len
        p[8] = 64; // TTL
        p[9] = 6; // TCP
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn parse_v4_tcp() {
        let p = tcp_v4(1234, 443);
        assert_eq!(parse_ports(&p), Some((6, false, 1234, 443)));
    }

    #[test]
    fn parse_fragment_returns_none() {
        let mut p = tcp_v4(1, 2);
        p[7] = 0x02; // Fragment Offset = 2（偏移 6..8 的低 13 位）
        assert_eq!(parse_ports(&p), None);
    }

    /// IPv6 分片头：next=TCP，offset/flags 字段按需设置，端口放在分片头之后
    fn tcp_v6_frag(sport: u16, dport: u16, frag_off: u16) -> Vec<u8> {
        let mut p = vec![0u8; 52];
        p[0] = 0x60; // IPv6
        p[6] = 44; // Next Header = Fragment
        p[40] = 6; // Fragment 头的 Next Header = TCP
        // offset/flags：高 13 位是 offset，低 3 位是保留 + M 标志
        p[42..44].copy_from_slice(&(frag_off << 3).to_be_bytes());
        p[48..50].copy_from_slice(&sport.to_be_bytes());
        p[50..52].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn parse_v6_first_fragment_ok() {
        let p = tcp_v6_frag(1234, 443, 0);
        assert_eq!(parse_ports(&p), Some((6, true, 1234, 443)));
    }

    #[test]
    fn parse_v6_non_first_fragment_returns_none() {
        // 非首片：载荷不是传输层头，不能当端口解析（否则可能随机命中真实端口）
        let p = tcp_v6_frag(1234, 443, 1);
        assert_eq!(parse_ports(&p), None);
    }

    #[test]
    fn win_divert_queue_params_in_range() {
        use windivert_sys::{
            WINDIVERT_PARAM_QUEUE_LENGTH_MAX, WINDIVERT_PARAM_QUEUE_LENGTH_MIN,
            WINDIVERT_PARAM_QUEUE_SIZE_MAX, WINDIVERT_PARAM_QUEUE_SIZE_MIN,
            WINDIVERT_PARAM_QUEUE_TIME_MAX, WINDIVERT_PARAM_QUEUE_TIME_MIN,
        };
        // 超出 windivert.h 范围会让 WinDivertSetParam 直接失败，必须由单测守住
        assert!((WINDIVERT_PARAM_QUEUE_LENGTH_MIN..=WINDIVERT_PARAM_QUEUE_LENGTH_MAX)
            .contains(&QUEUE_LEN));
        assert!((WINDIVERT_PARAM_QUEUE_TIME_MIN..=WINDIVERT_PARAM_QUEUE_TIME_MAX)
            .contains(&QUEUE_TIME_MS));
        assert!((WINDIVERT_PARAM_QUEUE_SIZE_MIN..=WINDIVERT_PARAM_QUEUE_SIZE_MAX)
            .contains(&QUEUE_SIZE_BYTES));
    }

    #[test]
    fn local_port_follows_direction() {
        let p = tcp_v4(5000, 443);
        assert_eq!(classify_local(&p, true), Some((6, false, 5000))); // 出站 → 源端口
        assert_eq!(classify_local(&p, false), Some((6, false, 443))); // 入站 → 目的端口
    }

    #[test]
    fn rules_add_update_remove() {
        let r = Rules::new();
        assert!(r.is_empty()); // 初始为空
        assert!(r.set(100, "a.exe".into(), 8192, 0, 0, 0)); // 新增
        assert!(!r.set(100, "a.exe".into(), 4096, 2048, 0, 0)); // 更新
        let e = r.get(100).unwrap();
        assert_eq!((e.dl_limit(), e.ul_limit()), (4096, 2048));
        assert!(r.remove(100));
        assert!(r.get(100).is_none());
    }

    #[test]
    fn protected_names_are_never_limited() {
        // 代理/网络关键进程：一限速整机就断网
        assert!(is_protected("FlClash.exe"));
        assert!(is_protected("mihomo.exe"));
        assert!(!is_protected("chrome.exe"));
        let r = Rules::new();
        assert!(!r.set(1234, "FlClash.exe".into(), 1024, 0, 0, 0), "保护名单必须拒绝");
        assert!(r.get(1234).is_none());
    }

    // ── 下载整形：去重 + 重传副本丢弃 ─────────────────────────────────────

    /// 构造一个 TCP 包：20B IP + 20B TCP + payload 字节。
    /// 对端固定 10.0.0.1，本机固定 203.0.113.7；`outbound` 决定地址方向
    /// （出站：源=本机、目的=对端；入站相反），这样两个方向的流键应归一化到同一个。
    fn tcp_pkt(sport: u16, dport: u16, seq: u32, payload: usize, outbound: bool) -> Vec<u8> {
        let total = 40 + payload;
        let mut p = vec![0u8; total];
        p[0] = 0x45; // v4, IHL=5
        p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        p[8] = 64; // TTL
        p[9] = 6; // TCP
        let (src, dst) = if outbound {
            ([203, 0, 113, 7], [10, 0, 0, 1])
        } else {
            ([10, 0, 0, 1], [203, 0, 113, 7])
        };
        p[12..16].copy_from_slice(&src);
        p[16..20].copy_from_slice(&dst);
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p[24..28].copy_from_slice(&seq.to_be_bytes()); // TCP seq（l4+4）
        p[32] = 5 << 4; // data offset = 5（20 字节 TCP 头）
        p
    }

    fn flow() -> FlowKey {
        // 统一键：本地端口 5000 + 对端 10.0.0.1:443
        FlowKey { sport: 5000, dport: 443, v6: false, addr: [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] }
    }

    #[test]
    fn parse_tcp_normalizes_flow_key_both_directions() {
        // 入站：sport=对端 443, dport=本地 5000
        let inb = tcp_pkt(443, 5000, 100, 4, false);
        let t = parse_tcp(&inb, false).unwrap();
        assert_eq!(t.key, flow());
        assert_eq!((t.seq, t.payload), (100, 4));
        // 出站：sport=本地 5000, dport=对端 443 → 同一个流键
        let out = tcp_pkt(5000, 443, 100, 0, true);
        let t2 = parse_tcp(&out, true).unwrap();
        assert_eq!(t2.key, flow());
        assert_eq!(t2.payload, 0);
        // UDP 不走这条路径
        let mut u = tcp_pkt(443, 5000, 1, 0, false);
        u[9] = 17;
        assert!(parse_tcp(&u, false).is_none());
    }

    #[test]
    fn dl_charge_queues_new_data_and_drops_retransmits() {
        let lim = ProcessLimit::new(1, "a.exe".into(), 1024, 0); // 1 KB/s
        let k = flow();
        // 新数据 1400B：排队 ≈1.3s
        match lim.charge_dl_seg(k, 1000, 1400, Instant::now()) {
            DlCharge::Hold(due) => {
                let wait = due.saturating_duration_since(Instant::now());
                assert!(
                    wait > Duration::from_millis(1000) && wait <= Duration::from_secs(2),
                    "wait={wait:?}"
                );
            }
            _ => panic!("新数据必须扣额度"),
        }
        // 同序号重传副本：直接丢（否则低速率档被重传顶穿）
        assert!(matches!(
            lim.charge_dl_seg(k, 1000, 1400, Instant::now()),
            DlCharge::Drop
        ));
        // 后续新数据：继续排队
        assert!(matches!(
            lim.charge_dl_seg(k, 2400, 1400, Instant::now()),
            DlCharge::Hold(_)
        ));
        // 纯 ACK（payload=0）不占额度
        assert!(matches!(lim.charge_dl_seg(k, 3800, 0, Instant::now()), DlCharge::Free));
    }

    #[test]
    fn dl_charge_is_per_flow() {
        let lim = ProcessLimit::new(1, "a.exe".into(), 4096, 0);
        let k = flow();
        let first = match lim.charge_dl_seg(k, 100, 1000, Instant::now()) {
            DlCharge::Hold(d) => d,
            _ => panic!(),
        };
        // 另一条连接（端口不同）各排各的
        let other = FlowKey { dport: 5001, ..flow() };
        let d_other = match lim.charge_dl_seg(other, 100, 1000, Instant::now()) {
            DlCharge::Hold(d) => d,
            _ => panic!(),
        };
        assert!(d_other > first, "两条流必须各排各的");
    }

    // ── 循环周期（限 N 秒 / 放 M 秒）──────────────────────────────────────

    #[test]
    fn cycle_switches_between_limit_and_free() {
        let lim = ProcessLimit::new(1, "a.exe".into(), 1024, 0);
        // 默认不循环：一直限速
        assert!(lim.active_at(Instant::now()));

        lim.set_cycle(600, 30);
        let (active, remain) = lim.cycle_state(Instant::now());
        assert!(active, "刚设置时应处于限速相位");
        assert!((599..=600).contains(&remain), "remain={remain}");

        // 回拨起点，模拟已过 601 秒 → 应进入放行相位
        *lim.cycle_start.lock().unwrap() = Instant::now() - std::time::Duration::from_secs(601);
        let (active, remain) = lim.cycle_state(Instant::now());
        assert!(!active, "超过限速时长应放行");
        assert!((28..=30).contains(&remain), "remain={remain}");
        assert!(!lim.active_at(Instant::now()));

        // 回拨 631 秒 = 一个完整周期之后 → 又回到限速相位
        *lim.cycle_start.lock().unwrap() = Instant::now() - std::time::Duration::from_secs(631);
        assert!(lim.active_at(Instant::now()), "一个周期后应回到限速相位");
    }

    #[test]
    fn cycle_zero_means_always_limit() {
        let lim = ProcessLimit::new(1, "a.exe".into(), 1024, 0);
        lim.set_cycle(600, 0); // 缺放行秒 = 不循环
        assert!(lim.active_at(Instant::now()));
        assert_eq!(lim.cycle_state(Instant::now()), (true, 0));
        lim.set_cycle(0, 30); // 缺限速秒同样 = 不循环
        assert!(lim.active_at(Instant::now()));
    }
}
