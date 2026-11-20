//! limiter.rs —— 限速核心：调度式令牌桶（Token Bucket）+ 延迟队列
//!
//! 原理：
//! 1. 每个被限速的"进程 × 方向"各有一个令牌桶。桶维护绝对时间线上的
//!    "债务清零时刻 next_free"：take(n) 返回该包应发出的时刻，
//!    next_free 精确前进 n/rate；晚醒不移动时间线 → 长期平均自带误差校正；
//! 2. 令牌充足（due <= now）立即放行，否则打上"到期时间"进延迟队列；
//! 3. 调度线程睡到最近的到期时间（先睡后自旋），到点重注入；
//! 4. 队列有包数/字节上限，超限或滞留过久（上限由调用方按包耗时定）
//!    直接丢包并退还额度（TCP 会自动退避降速）。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 突发窗口配置：允许 15ms 的流量作为 burst，上限 256KB（高速率防冲高）。
/// 不设大下限：burst 小于单包时，超出的部分按速率折算成等待时间即可。
/// 实测：3000B 下限在 8KB/s 档造成 ±30% 秒级波动；40ms 窗口在 20KB/s 档
/// 仍有 ±30%（队列空→时间线按 burst 重置的相位断裂），15ms 进一步收窄，
/// 秒级残余波动属 TCP 反馈环固有，长期平均由绝对时间线校正。
const BURST_WINDOW_SECS: f64 = 0.015;
const BURST_MIN_BYTES: f64 = 64.0;
const BURST_MAX_BYTES: f64 = 256.0 * 1024.0;

/// 令牌桶（调度式，精度优化版）：
///
/// 不再用"按流逝时间补充令牌"的模型，改为维护一条绝对时间线上的
/// "债务清零时刻 next_free"：
/// - take(n) 返回该包应当发出的时刻；next_free 精确前进 n/rate；
/// - 调度线程晚醒不会移动 next_free（它是绝对时刻），后续包的 due 不变，
///   长期平均速率因此**自带累计误差校正**——理论上少发的量自动补回，
///   多发的量自动扣回，不会漂移；
/// - 空闲期最多积累 burst 字节的"立即发送额度"，防止空闲后冲高。
pub struct TokenBucket {
    rate: f64,          // 字节/秒（>0）
    burst: f64,         // 允许的突发字节数
    next_free: Instant, // 债务清零时刻 = 下一包最早可发基准
}

impl TokenBucket {
    pub fn new(rate_bps: u64) -> Self {
        let rate = (rate_bps.max(1)) as f64;
        let burst = (rate * BURST_WINDOW_SECS).clamp(BURST_MIN_BYTES, BURST_MAX_BYTES);
        // 初始预充 burst 额度（next_free 回退 burst/rate）：启动初期允许小突发
        let next_free = Instant::now() - Duration::from_secs_f64(burst / rate);
        Self { rate, burst, next_free }
    }

    /// 预约 n 字节，返回应发送时刻（返回值 <= now 表示可立即发送）。
    ///
    /// 记账规则（全路径统一）：`next_free += n/rate`。
    /// due 只是"这包何时能放出"，而它占用的速率窗口从 due 起算 n/rate 秒，
    /// 因此下一包的 due 至少是 due + n/rate —— 这是包级串行化的关键。
    pub fn take(&mut self, n: usize) -> Instant {
        let now = Instant::now();
        let step = Duration::from_secs_f64(n as f64 / self.rate);
        let due = if self.next_free <= now {
            // 无债务：空闲积累的"立即发送额度"，上限 burst 字节
            let idle = now - self.next_free;
            let allowance = (idle.as_secs_f64() * self.rate).min(self.burst);
            let over = n as f64 - allowance;
            if over <= 0.0 {
                now // 额度内：立即发
            } else {
                // 超出额度：欠的部分按速率折算成等待时间
                now + Duration::from_secs_f64(over / self.rate)
            }
        } else {
            // 有债务：排到债务清零之后；due 与实际发送时刻无关（误差校正的关键）
            self.next_free
        };
        self.next_free = due + step;
        due
    }

    /// 退还未真正放出（被队列丢弃）的字节额度。
    /// **必须成对使用**：若 take() 后包被丢（队满/滞留过久），不回退时间线的话
    /// next_free 会被"丢弃的包"持续推远，此后每个新包 due 都超限 → 全量丢包，
    /// 吞吐彻底冻结（实测 8KB/s 档偏差 -92% 就是这个原因）。
    pub fn refund(&mut self, n: usize) {
        let step = Duration::from_secs_f64(n as f64 / self.rate);
        self.next_free = self.next_free.checked_sub(step).unwrap_or(self.next_free);
        // 最多回退到"当前 + burst 额度"的位置，避免长时间空闲后时间线倒流
        let floor = Instant::now() - Duration::from_secs_f64(self.burst / self.rate);
        if self.next_free < floor {
            self.next_free = floor;
        }
    }

}

/// 延迟队列内部状态
struct QueueState<T> {
    q: VecDeque<QItem<T>>, // 按 due 单调不减排列
    bytes: usize,          // 当前排队字节数
    max_packets: usize,    // 队列包数上限
    max_bytes: usize,      // 队列字节上限
    dropped: u64,          // 累计丢弃包数
    closed: bool,          // 关闭标志：唤醒所有等待者并停止入队
    last_pop: Option<Instant>, // 上次弹出时刻（高密度检测，用于收缩自旋窗口）
}

struct QItem<T> {
    due: Instant,
    size: usize,
    pkt: T,
}

/// 线程安全的延迟队列（Mutex + Condvar）。
///
/// 注：队列元素的"最长滞留时间"不在此处定——它取决于包大小与速率
/// （n/rate 秒），由调用方（capture.rs）按 `max(2×n/rate, 1s)` 判断；
/// 固定 2s 上限曾在 1KB/s 档饿死 4KB 的 GRO 重组包（它本身需要 4 秒），
/// 实测吞吐掉到 0.3KB/s。
pub struct DelayQueue<T> {
    st: Mutex<QueueState<T>>,
    cv: Condvar,
}

/// 精度参数（精度优化）：
/// - 离到期还远（> 自旋窗口）时用 Condvar 睡眠；配合 timeBeginPeriod(1)，
///   Windows 定时器粒度 ~15.6ms → ~1ms；
/// - 最后的自旋窗口内改忙等（spin_loop），到点立刻弹出 → 亚毫秒精度；
/// - 高密度场景（上次弹出距现在 < BUSY_GAP）把自旋窗口缩到 100µs，
///   避免高速率整形时整核自旋。
const SPIN_WINDOW: Duration = Duration::from_micros(1500);
const BUSY_SPIN_WINDOW: Duration = Duration::from_micros(100);
const BUSY_GAP: Duration = Duration::from_millis(5);

impl<T> DelayQueue<T> {
    pub fn new(max_packets: usize, max_bytes: usize) -> Self {
        Self {
            st: Mutex::new(QueueState {
                q: VecDeque::new(),
                bytes: 0,
                max_packets,
                max_bytes,
                dropped: 0,
                closed: false,
                last_pop: None,
            }),
            cv: Condvar::new(),
        }
    }

    /// 入队（按 due 有序插入）。队列满（包数/字节数超限）则丢弃并返回 false。
    /// 滞留时长是否可接受由调用方判断（不同包大小/速率下上限不同）。
    pub fn push(&self, pkt: T, size: usize, due: Instant) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.closed || st.q.len() >= st.max_packets || st.bytes + size > st.max_bytes {
            st.dropped += 1; // 队满丢包：TCP 会因丢包自动拥塞退避
            return false;
        }
        st.bytes += size;
        // 常见情况：新包 due 不早于队尾，直接 append
        if st.q.back().map(|b| due >= b.due).unwrap_or(true) {
            st.q.push_back(QItem { due, size, pkt });
        } else {
            // 少见情况（桶突发导致 due 回退）：从后往前找插入点保持有序
            let mut idx = st.q.len();
            while idx > 0 && st.q[idx - 1].due > due {
                idx -= 1;
            }
            st.q.insert(idx, QItem { due, size, pkt });
        }
        drop(st);
        self.cv.notify_one();
        true
    }

    /// 阻塞等待直到队头到期并弹出；队列关闭时返回 None。
    /// 精度策略：远端时间睡眠等待，最后 SPIN_WINDOW 内自旋等待（见常量注释）。
    pub fn pop_due(&self) -> Option<T> {
        let mut st = self.st.lock().unwrap();
        loop {
            if st.closed {
                return None;
            }
            let now = Instant::now();
            match st.q.front() {
                Some(front) if front.due <= now => {
                    st.last_pop = Some(now);
                    let item = st.q.pop_front().unwrap();
                    st.bytes -= item.size; // 与 stats() 的字节数保持一致（弹出即归还）
                    return Some(item.pkt);
                }
                Some(front) => {
                    let remaining = front.due - now;
                    // 高密度整形时收缩自旋窗口，避免长时间整核自旋
                    let window = if st.last_pop.is_some_and(|t| now - t < BUSY_GAP) {
                        BUSY_SPIN_WINDOW
                    } else {
                        SPIN_WINDOW
                    };
                    if remaining > window {
                        // 睡到"到期前一个自旋窗口"（到点会被 notify 或超时唤醒）
                        let (guard, _) = self
                            .cv
                            .wait_timeout(st, remaining - window)
                            .unwrap();
                        st = guard;
                    } else {
                        // 最后一段：临界区外自旋到精确到期点
                        let target = front.due;
                        drop(st); // 不持锁自旋，入队/关闭不被阻塞
                        while Instant::now() < target {
                            std::hint::spin_loop();
                        }
                        st = self.st.lock().unwrap();
                        // 回到循环头重新校验（期间可能 close 或有更早的包入队）
                    }
                }
                None => {
                    // 队列空：长眠直到有新包或关闭
                    st = self.cv.wait(st).unwrap();
                }
            }
        }
    }

    /// 关闭队列（唤醒等待线程），并返回所有剩余元素供调用者冲刷重注入。
    pub fn close_and_drain(&self) -> Vec<T> {
        let mut st = self.st.lock().unwrap();
        st.closed = true;
        drop(st);
        self.cv.notify_all();
        let mut st = self.st.lock().unwrap();
        let out = st.q.drain(..).map(|i| i.pkt).collect();
        st.bytes = 0;
        out
    }

    /// 丢弃所有满足谓词的元素（返回丢弃个数）。
    ///
    /// 用途：循环限速从"限速相位"切到"放行相位"时，把该进程还在排队的包清掉——
    /// 否则积压会继续按旧的到期时刻慢放，放行相位根本放不干净
    /// （实测：1 KB/s + 6s限/4s放 时队列积压 190 包 / 273 KB）。
    /// 对 TCP 丢掉这些包无害：重传会重发，而放行相位里重传是直通的。
    pub fn purge_where<F: Fn(&T) -> bool>(&self, pred: F) -> usize {
        let mut st = self.st.lock().unwrap();
        let before = st.q.len();
        let mut bytes_freed = 0usize;
        st.q.retain(|item| {
            if pred(&item.pkt) {
                bytes_freed += item.size;
                false
            } else {
                true
            }
        });
        st.bytes = st.bytes.saturating_sub(bytes_freed);
        before - st.q.len()
    }

    /// 快照统计：(当前排队包数, 排队字节数, 累计丢弃包数)
    pub fn stats(&self) -> (usize, usize, u64) {
        let st = self.st.lock().unwrap();
        (st.q.len(), st.bytes, st.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_immediate_when_burst_covers() {
        let mut b = TokenBucket::new(1024 * 1024); // burst ≈ 40KB
        assert!(b.take(1500) <= Instant::now());
    }

    #[test]
    fn bucket_defers_oversized_packet() {
        let mut b = TokenBucket::new(10_000); // burst = 400B（40ms 窗口）
        let start = Instant::now();
        let d = b.take(100_000);
        // 100KB − 400B 突发 ÷ 10KB/s ≈ 9.96s
        let wait = d.duration_since(start);
        assert!(
            wait > Duration::from_secs(9) && wait < Duration::from_secs(11),
            "wait={wait:?}"
        );
    }

    #[test]
    fn bucket_debt_spaced_exactly() {
        // 精度优化核心性质：进入债务态后，due 严格按 n/rate 推进，
        // 与调度线程晚醒多少无关（累计误差自动校正）
        let mut b = TokenBucket::new(1_000); // burst = 40B
        let _ = b.take(4_000); // 超出 burst → 进入债务态
        let d1 = b.take(100);
        let d2 = b.take(100);
        assert_eq!(d2.duration_since(d1), Duration::from_secs_f64(0.1));
    }

    #[test]
    fn bucket_burst_caps_idle_windfall() {
        // 空闲很久也只能立即发出 burst 额度（1MB/s × 15ms ≈ 15.7KB），超出必须排队
        let mut b = TokenBucket::new(1024 * 1024);
        std::thread::sleep(Duration::from_millis(50)); // 积累远超 burst 的空闲
        assert!(b.take(15_000) <= Instant::now()); // 额度内：立即
        let d = b.take(2_000); // 剩余额度不足 → 必须排队
        assert!(d > Instant::now());
    }

    #[test]
    fn queue_drops_when_full() {
        let q: DelayQueue<u32> = DelayQueue::new(2, 1024);
        let now = Instant::now();
        assert!(q.push(1, 100, now));
        assert!(q.push(2, 100, now));
        assert!(!q.push(3, 100, now)); // 第 3 个被丢
        let (_, _, dropped) = q.stats();
        assert_eq!(dropped, 1);
    }

    #[test]
    fn bucket_refund_restores_timeline() {
        // 丢弃的包必须退还额度，否则时间线一路前推导致后续包全部超时
        let mut b = TokenBucket::new(8 * 1024); // burst ≈ 122B
        let d1 = b.take(4_000);
        let d2 = b.take(4_000);
        assert!(d2 > d1, "包必须串行化：d1={d1:?} d2={d2:?}");
        b.refund(4_000); // 撤销第二个包
        let d3 = b.take(4_000);
        assert_eq!(d3, d2); // 退还得当的话，第三个包占回被撤销的第二个的位置
    }

    #[test]
    fn queue_drops_when_bytes_exceeded() {
        // 包数上限内但字节超限 → 丢弃
        let q: DelayQueue<u32> = DelayQueue::new(8, 200);
        let now = Instant::now();
        assert!(q.push(1, 100, now));
        assert!(q.push(2, 100, now));
        assert!(!q.push(3, 100, now)); // 100+100+100 > 200
        let (len, _, dropped) = q.stats();
        assert_eq!((len, dropped), (2, 1));
    }

    // ── 离线验证令牌桶本身的速率精度（不涉及网络反馈）────────────────────
    //
    // 端到端实测会被 TCP 反馈污染（重传、窗口、RTO），先把"桶"单独量出来，
    // 才知道端到端偏差是桶的问题还是网络层的问题。
    // 做法：不睡真实时间，直接读令牌桶给出的 due 时间线 —— 这才是"桶"的数学精度，
    // 与调度线程的唤醒抖动无关（抖动由 pop_due 的自旋窗口负责，另有测试）。

    fn sim_rate_virtual(rate_bps: u64, pkt: usize, packets: usize) -> f64 {
        let mut b = TokenBucket::new(rate_bps);
        let mut first: Option<Instant> = None;
        let mut last: Option<Instant> = None;
        for _ in 0..packets {
            let due = b.take(pkt);
            first.get_or_insert(due);
            last = Some(due);
        }
        // 时间跨度 = 首包到末包的 due 间隔 + 末包自己占用的时长
        let span = last.unwrap().duration_since(first.unwrap()).as_secs_f64()
            + pkt as f64 / rate_bps as f64;
        packets as f64 * pkt as f64 / span
    }

    #[test]
    fn bucket_offline_accuracy_mss_packets() {
        // 1 KB/s + 1.4KB 标准段，2000 个包
        let got = sim_rate_virtual(1024, 1432, 2000);
        let err = (got - 1024.0) / 1024.0 * 100.0;
        eprintln!("MSS 段: {got:.1} B/s，偏差 {err:+.2}%");
        assert!(err.abs() < 1.0, "偏差 {err:+.2}% 超出 ±1%");
    }

    #[test]
    fn bucket_offline_accuracy_gro_packets() {
        // 1 KB/s + 8.8KB GRO 大包（离散释放），200 个包
        let got = sim_rate_virtual(1024, 8800, 200);
        let err = (got - 1024.0) / 1024.0 * 100.0;
        eprintln!("GRO 包: {got:.1} B/s，偏差 {err:+.2}%");
        assert!(err.abs() < 1.0, "偏差 {err:+.2}% 超出 ±1%");
    }
}
