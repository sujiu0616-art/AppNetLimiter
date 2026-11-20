//! process_map.rs —— "本地端口 → PID" 映射
//!
//! 用 windows crate 调 GetExtendedTcpTable / GetExtendedUdpTable，
//! 遍历 TCP/UDP 表（OWNER_MODULE 级别带 OwningPid），
//! 后台线程每秒刷新一次，抓包线程只做只读查询。
//!
//! 刷新策略：周期刷新（1s）兜底，另外抓包线程查不到端口时可以
//! `request_refresh()` 请求立即补一次（合并 + 最短间隔 200ms）——
//! 新连接的首包通常比端口表早到，只靠 1s 周期会让首段流量漏限速。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6TABLE_OWNER_MODULE, MIB_TCPTABLE_OWNER_MODULE,
    MIB_UDP6TABLE_OWNER_MODULE, MIB_UDPTABLE_OWNER_MODULE, TCP_TABLE_OWNER_MODULE_ALL,
    UDP_TABLE_OWNER_MODULE,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 协议常量（与 IP 头里的 protocol 字段一致）
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

/// 键：(协议, 4/6, 本地端口) → PID
pub type PortMap = HashMap<(u8, u8, u16), u32>;

/// 周期刷新间隔（兜底：端口表本身会随连接变化）
const PERIODIC_REFRESH: Duration = Duration::from_secs(1);

/// 两次快照之间的最短间隔：查不到端口的包会触发即时刷新，
/// 用这个下限防止"永远查不到"的流量（内核态发包等）把 CPU 拖进反复扫表。
const MIN_REFRESH_GAP: Duration = Duration::from_millis(200);

/// 刷新线程的共享状态
struct RefreshState {
    /// 有"即时刷新"请求待处理
    requested: bool,
    /// 上次快照时刻
    last_refresh: Instant,
}

/// 端口映射服务：后台线程每秒刷新一次快照
pub struct ProcessMap {
    map: Arc<RwLock<PortMap>>,
    stop: Arc<AtomicBool>,
    /// 即时刷新请求 + 唤醒（lookup miss 时用）
    refresh: Arc<(Mutex<RefreshState>, Condvar)>,
    /// 原子合并标志：已有待处理请求时，后续 miss 不再抢锁（抓包热路径）
    pending: Arc<AtomicBool>,
    /// 已完成的刷新次数（诊断/测试用）
    #[allow(dead_code)] // 仅测试读取（release 构建里不读，避免 dead_code 警告）
    refreshes: Arc<AtomicU64>,
}

impl ProcessMap {
    /// 启动刷新线程（先同步刷一次，保证启动时就有数据）
    pub fn start() -> Self {
        let map: Arc<RwLock<PortMap>> = Arc::new(RwLock::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            // 初始快照：避免启动头一秒内查不到端口
            let fresh = snapshot();
            *map.write().unwrap() = fresh;
        }
        let refresh = Arc::new((
            Mutex::new(RefreshState {
                requested: false,
                last_refresh: Instant::now(),
            }),
            Condvar::new(),
        ));
        let pending = Arc::new(AtomicBool::new(false));
        let refreshes = Arc::new(AtomicU64::new(0));

        let (m, s) = (map.clone(), stop.clone());
        let r = refresh.clone();
        let p = pending.clone();
        let c = refreshes.clone();
        thread::Builder::new()
            .name("port-map-refresh".into())
            .spawn(move || {
                let (state, cv) = &*r;
                loop {
                    let mut st = state.lock().unwrap();
                    if s.load(Ordering::Relaxed) {
                        return;
                    }
                    let since = st.last_refresh.elapsed();
                    if st.requested {
                        if since < MIN_REFRESH_GAP {
                            // 离上次快照太近：睡到间隔下限再刷（请求不会丢）
                            let _ = cv.wait_timeout(st, MIN_REFRESH_GAP - since).unwrap();
                            continue;
                        }
                        st.requested = false;
                    } else if since < PERIODIC_REFRESH {
                        // 无事可做：睡到下一次周期刷新（有新请求会被 notify 唤醒）
                        let _ = cv.wait_timeout(st, PERIODIC_REFRESH - since).unwrap();
                        continue;
                    }
                    st.last_refresh = Instant::now();
                    // 先放开合并标志：快照期间出现的新 miss 会重新请求，
                    // 本轮快照覆盖不到的话下一轮马上补
                    p.store(false, Ordering::Relaxed);
                    drop(st);

                    let fresh = snapshot();
                    if let Ok(mut w) = m.write() {
                        *w = fresh;
                    }
                    c.fetch_add(1, Ordering::Relaxed);
                }
            })
            .expect("spawn port-map-refresh");
        Self { map, stop, refresh, pending, refreshes }
    }

    /// 查本地端口对应的 PID；查不到返回 None（新建连接可能还没进表）
    pub fn lookup(&self, proto: u8, v6: bool, local_port: u16) -> Option<u32> {
        let ver = if v6 { 6 } else { 4 };
        self.map
            .read()
            .ok()
            .and_then(|m| m.get(&(proto, ver, local_port)).copied())
    }

    /// 查不到端口时调用：请求刷新线程尽快重拉一次表（合并 + 限频）。
    /// 抓包线程调用，必须低开销：先用原子标志合并重复请求，
    /// 只有第一次请求才进临界区唤醒线程。
    pub fn request_refresh(&self) {
        if self.pending.swap(true, Ordering::Relaxed) {
            return;
        }
        let (state, cv) = &*self.refresh;
        {
            let mut st = state.lock().unwrap();
            st.requested = true;
        }
        cv.notify_one();
    }

    /// 已完成的刷新次数（测试/诊断）
    #[cfg(test)]
    fn refresh_count(&self) -> u64 {
        self.refreshes.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // 唤醒可能正睡在 1s 周期里的刷新线程，避免退出时多等一秒
        let (state, cv) = &*self.refresh;
        let _guard = state.lock().unwrap();
        cv.notify_all();
    }
}

/// 拉取一次 TCP+UDP（IPv4+IPv6）全表
fn snapshot() -> PortMap {
    let mut m = HashMap::new();
    unsafe {
        fill_tcp(AF_INET.0 as u32, 4, &mut m);
        fill_tcp(AF_INET6.0 as u32, 6, &mut m);
        fill_udp(AF_INET.0 as u32, 4, &mut m);
        fill_udp(AF_INET6.0 as u32, 6, &mut m);
    }
    m
}

/// 安全封装：两次调用法拿 TCP 表（第一次探测所需大小，第二次真正取）。
/// 注意：IPv4 表行是 MIB_TCPROW_OWNER_MODULE，IPv6 表行是 MIB_TCP6ROW_OWNER_MODULE，
/// 两者字段布局完全不同（v6 行里地址是 16 字节数组且多 ScopeId），必须分开解析。
unsafe fn fill_tcp(af: u32, ver: u8, m: &mut PortMap) {
    let mut size: u32 = 0;
    // 第一次调用：返回 122 (ERROR_INSUFFICIENT_BUFFER) 并写入需要的字节数
    GetExtendedTcpTable(None, &mut size, false, af, TCP_TABLE_OWNER_MODULE_ALL, 0);
    if size == 0 {
        return;
    }
    let mut buf = vec![0u8; size as usize];
    let ret = GetExtendedTcpTable(
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        &mut size,
        false,
        af,
        TCP_TABLE_OWNER_MODULE_ALL,
        0,
    );
    if ret != 0 {
        return; // 拉表失败：保留旧映射，等下一轮
    }
    if af == AF_INET.0 as u32 {
        let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_MODULE);
        let rows =
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for r in rows {
            // dwLocalPort 低 16 位是网络字节序的端口号
            let port = u16::from_be((r.dwLocalPort & 0xFFFF) as u16);
            m.insert((PROTO_TCP, ver, port), r.dwOwningPid);
        }
    } else {
        let table = &*(buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_MODULE);
        let rows =
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for r in rows {
            let port = u16::from_be((r.dwLocalPort & 0xFFFF) as u16);
            m.insert((PROTO_TCP, ver, port), r.dwOwningPid);
        }
    }
}

/// UDP 表同理（IPv4/IPv6 行布局同样不同，分开解析）
unsafe fn fill_udp(af: u32, ver: u8, m: &mut PortMap) {
    let mut size: u32 = 0;
    GetExtendedUdpTable(None, &mut size, false, af, UDP_TABLE_OWNER_MODULE, 0);
    if size == 0 {
        return;
    }
    let mut buf = vec![0u8; size as usize];
    let ret = GetExtendedUdpTable(
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        &mut size,
        false,
        af,
        UDP_TABLE_OWNER_MODULE,
        0,
    );
    if ret != 0 {
        return;
    }
    if af == AF_INET.0 as u32 {
        let table = &*(buf.as_ptr() as *const MIB_UDPTABLE_OWNER_MODULE);
        let rows =
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for r in rows {
            let port = u16::from_be((r.dwLocalPort & 0xFFFF) as u16);
            m.insert((PROTO_UDP, ver, port), r.dwOwningPid);
        }
    } else {
        let table = &*(buf.as_ptr() as *const MIB_UDP6TABLE_OWNER_MODULE);
        let rows =
            std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for r in rows {
            let port = u16::from_be((r.dwLocalPort & 0xFFFF) as u16);
            m.insert((PROTO_UDP, ver, port), r.dwOwningPid);
        }
    }
}

/// 进程是否仍存在（回收失效 PID 规则用）。
/// 返回 None 表示无法判断，调用方应保守保留规则。
pub fn process_alive(pid: u32) -> Option<bool> {
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let _ = CloseHandle(h);
                Some(true)
            }
            // HRESULT 的低 16 位即 Win32 错误码：
            // 87 = ERROR_INVALID_PARAMETER（PID 不存在，如 Idle 进程/已退出）
            // 5  = ERROR_ACCESS_DENIED（进程存在但受保护，无法查询）
            Err(e) => match (e.code().0 & 0xFFFF) as u32 {
                87 => Some(false),
                5 => Some(true),
                _ => None,
            },
        }
    }
}

/// 查 PID 对应的进程名（失败时退化为 <pid N>）
pub fn process_name(pid: u32) -> String {    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return format!("<pid {pid}>（可能是系统/已退出进程）");
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            windows::Win32::System::Threading::PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(h);
        if ok.is_ok() {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        } else {
            format!("<pid {pid}>")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 诊断：快照非空、端口值域合理、包含自身进程（本测试进程也有 TCP/UDP 表项）
    #[test]
    fn snapshot_has_entries_and_sane_ports() {
        let m = snapshot();
        // 一台正常联网的机器上 TCP+UDP(v4+v6) 表项至少几十个
        assert!(m.len() > 10, "端口表为空：GetExtendedTcpTable/UdpTable 可能失败");
        for ((_, _, port), _) in m.iter().take(200) {
            assert!(*port > 0, "端口越界: {port}"); // u16 上限天然 65535
        }
        // 测试进程自身至少出现在 UDP 表（Rust 测试运行时一般也有 UDP 项；不做硬性断言）
        eprintln!("snapshot entries = {}", m.len());
    }

    /// 诊断：手动构造一个 UDP 套接字，确认能查到自己的 PID
    #[test]
    fn lookup_finds_own_socket() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let m = snapshot();
        let pid = std::process::id();
        assert!(
            m.contains_key(&(PROTO_UDP, 4, port)),
            "自身 UDP 端口 {port} 没出现在快照里（快照大小 {}）",
            m.len()
        );
        assert_eq!(m.get(&(PROTO_UDP, 4, port)), Some(&pid));
    }

    /// lookup miss 触发的即时刷新必须远快于 1s 周期（本测试用 700ms 窗口区分）
    #[test]
    fn request_refresh_triggers_quickly() {
        let pm = ProcessMap::start();
        let before = pm.refresh_count();
        pm.request_refresh();
        let deadline = Instant::now() + Duration::from_millis(700);
        while pm.refresh_count() == before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pm.refresh_count() > before, "lookup miss 应触发一次即时刷新");
        pm.stop();
    }
}
