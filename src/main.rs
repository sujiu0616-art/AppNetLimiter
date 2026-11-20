//! app_limiter —— Windows 按进程限速工具
//! 第三步（GUI 版）：egui 界面 + 规则持久化（config.json，按进程名保存）。
//!
//! 默认启动图形界面；加 `--cli` 用第二步的命令行交互模式。
//!
//! GUI 用法：
//!   1. 列表显示当前有网络活动的进程与实时上传/下载速率（500ms 刷新）
//!   2. 每行可输入下载/上传限速（KB/s，或加 m 后缀 = MB/s；0 = 不限）
//!   3. 点『应用』立即生效（同名进程所有实例）并写入 exe 同目录 config.json
//!   4. 点『清除』取消限速；新启动的同名进程自动继承规则
//!   5. 关闭窗口或 Ctrl+C 退出：冲刷队列、关闭 WinDivert 句柄、恢复网络

mod capture;
mod config;
mod limiter;
mod process_map;
mod ui;

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use capture::{is_protected, Capture, Rules};
use config::RuleStore;
use process_map::{process_name, ProcessMap};

/// RAII：把系统定时器精度提到 1ms（默认 ~15.6ms）。
/// Windows 10 2004+ 起 timeBeginPeriod 只影响本进程，不再拖累全局功耗。
/// Condvar::wait_timeout 的实际粒度随之下探到 ~1ms，
/// 配合调度线程最后 1.5ms 的自旋等待，放行精度可达亚毫秒。
struct TimerRes;
impl TimerRes {
    fn new() -> Self {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        Self
    }
}
impl Drop for TimerRes {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--cli") {
        run_cli();
    } else {
        run_gui();
    }
}

/// exe 所在目录（config.json 放这里，便携）
fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 读程序图标（编译期嵌入的 assets/app.ico 的最大帧）→ egui 窗口/任务栏图标
fn load_window_icon() -> Option<eframe::egui::IconData> {
    let bytes = include_bytes!("../assets/app.ico");
    match image::load_from_memory_with_format(bytes, image::ImageFormat::Ico) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            Some(eframe::egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(e) => {
            eprintln!("图标解码失败（用默认图标）：{e}");
            None
        }
    }
}

// ══════════════════════════════ GUI 模式 ══════════════════════════════

fn run_gui() {
    // 1) 精度：定时器 1ms（Drop 时恢复）
    let _timer = TimerRes::new();

    // 2) 端口→PID 映射 + 规则表 + 配置加载
    let pmap = Arc::new(ProcessMap::start());
    let rules = Rules::new();
    let store = Arc::new(RuleStore::load(rules.clone(), exe_dir().join("config.json")));

    // 3) WinDivert 抓包（失败最常见原因：没用管理员运行）
    let cap = match Capture::start(&pmap, rules.clone()) {
        Ok(c) => Arc::new(Mutex::new(c)),
        Err(e) => {
            eprintln!("{e}");
            pmap.stop();
            std::process::exit(1);
        }
    };

    // 4) Ctrl+C → 置退出标志（App 每帧检查后走窗口退出路径）
    let exit_flag = Arc::new(AtomicBool::new(false));
    {
        let exit_flag = exit_flag.clone();
        let _ = ctrlc::set_handler(move || {
            exit_flag.store(true, Ordering::Relaxed);
        });
    }

    // 5) 队列统计快照线程（500ms 刷新，UI 无锁读）
    let qstats = Arc::new(ui::QueueStatSnap::default());
    {
        let qstats = qstats.clone();
        let cap = cap.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(cap) = cap.try_lock() else { continue };
            if cap.is_stopped() {
                break;
            }
            qstats.update(cap.queue_stats());
        });
    }

    // 6) 界面
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("AppNetLimiter")
        .with_inner_size([920.0, 560.0])
        .with_min_inner_size([760.0, 380.0]);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon); // 标题栏/任务栏显示自定义图标
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let app = {
        let store = store.clone();
        let rules = rules.clone();
        let activity = cap.lock().unwrap().activity.clone();
        let stats = cap.lock().unwrap().stats.clone();
        let qstats = qstats.clone();
        let exit_flag = exit_flag.clone();
        move |cc: &eframe::CreationContext<'_>| -> Result<
            Box<dyn eframe::App>,
            Box<dyn std::error::Error + Send + Sync>,
        > {
            Ok(Box::new(ui::LimiterApp::new(cc, store, rules, activity, stats, qstats, exit_flag)))
        }
    };
    let gui_result = eframe::run_native("AppNetLimiter", options, Box::new(app));

    // 7) 统一收尾：冲刷队列、join 线程、关闭 WinDivert 句柄（网络恢复）
    {
        let mut cap = cap.lock().unwrap();
        cap.shutdown();
    }
    pmap.stop();
    if let Err(e) = gui_result {
        eprintln!("界面异常退出：{e}");
        std::process::exit(1);
    }
    println!("已退出。WinDivert 句柄已关闭，网络恢复正常。");
}

// ══════════════════════════════ CLI 模式（第二步） ══════════════════════════════

const USAGE: &str = "\
app_limiter —— Windows 按进程限速工具（CLI 模式）

启动规则可来自命令行（--rule 可多条），也可以不带参数启动后用 set 命令添加：
  --rule <PID>:<下载KB/s>:<上传KB/s>[:<开秒>:<停秒>]
                                         例：--rule 4012:300:50
                                              --rule 4012:8:0:600:600  （限速 600 秒、停 600 秒循环）
  --pid <PID> --dl <KB/s> [--ul <KB/s>]  单条规则的简写

运行时命令：
  list                                  显示规则、实时速率与循环相位
  set <PID> <下载> <上传> [限速秒 放行秒]  新增/修改规则（0 = 不限；周期留空 = 一直限速）
  del <PID>                             取消该进程的限速（立即恢复不限速）
  quit                                  退出
";

/// 启动规则：(PID, 下载KB/s, 上传KB/s, 限速秒, 放行秒)
type RuleSpec = (u32, u64, u64, u64, u64);

fn parse_args() -> Result<Vec<RuleSpec>, String> {
    let mut rules: Vec<RuleSpec> = Vec::new();
    let mut pid: Option<u32> = None;
    let mut dl: Option<u64> = None;
    let mut ul: Option<u64> = None;

    let argv: Vec<String> = std::env::args().skip(1).filter(|a| a != "--cli").collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--rule" => {
                let s = argv.get(i + 1).ok_or("--rule 缺少取值 PID:下载:上传[:限速秒:放行秒]")?;
                let parts: Vec<&str> = s.split(':').collect();
                if parts.len() != 3 && parts.len() != 5 {
                    return Err(format!("--rule 格式应为 PID:下载:上传[:限速秒:放行秒]，收到 {s}"));
                }
                let p: u32 = parts[0].parse().map_err(|_| "PID 必须是数字".to_string())?;
                let d: u64 = parts[1].parse().map_err(|_| "下载限速必须是数字".to_string())?;
                let u: u64 = parts[2].parse().map_err(|_| "上传限速必须是数字".to_string())?;
                let (ls, fs) = if parts.len() == 5 {
                    (
                        parts[3].parse().map_err(|_| "限速秒必须是数字".to_string())?,
                        parts[4].parse().map_err(|_| "放行秒必须是数字".to_string())?,
                    )
                } else {
                    (0u64, 0u64)
                };
                rules.push((p, d, u, ls, fs));
                i += 1; // 吃掉取值
            }
            "--pid" => {
                pid = Some(
                    argv.get(i + 1)
                        .ok_or("--pid 缺少取值")?
                        .parse()
                        .map_err(|_| "--pid 必须是数字")?,
                );
                i += 1;
            }
            "--dl" => {
                dl = Some(
                    argv.get(i + 1)
                        .ok_or("--dl 缺少取值")?
                        .parse()
                        .map_err(|_| "--dl 必须是数字（KB/s）")?,
                );
                i += 1;
            }
            "--ul" => {
                ul = Some(
                    argv.get(i + 1)
                        .ok_or("--ul 缺少取值")?
                        .parse()
                        .map_err(|_| "--ul 必须是数字（KB/s）")?,
                );
                i += 1;
            }
            other => return Err(format!("未知参数: {other}")),
        }
        i += 1;
    }
    // 兼容第一步的简写形式
    if let Some(p) = pid {
        rules.push((p, dl.unwrap_or(0), ul.unwrap_or(0), 0, 0));
    }
    Ok(rules)
}

/// 字节/秒 → 人类可读速率
fn fmt_rate(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bps / 1024.0 / 1024.0)
    } else if bps >= 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// 限速值（字节/秒）→ 展示文本
fn fmt_limit(bps: u64) -> String {
    if bps == 0 {
        "不限".to_string()
    } else if bps >= 1024 * 1024 {
        format!("限 {:.1} MB/s", bps as f64 / 1024.0 / 1024.0)
    } else {
        format!("限 {:.1} KB/s", bps as f64 / 1024.0)
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.2} MB", b as f64 / 1024.0 / 1024.0)
    } else if b >= 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// 运行时命令
enum Cmd {
    List,
    Set { pid: u32, dl: u64, ul: u64, limit_secs: u64, free_secs: u64 },
    Del(u32),
    Quit,
    Invalid(String),
}

/// 解析一行 stdin 命令
fn parse_cmd(line: &str) -> Option<Cmd> {
    let mut it = line.split_whitespace();
    let word = it.next()?;
    let rest: Vec<&str> = it.collect();
    match word.to_ascii_lowercase().as_str() {
        "list" => Some(Cmd::List),
        "quit" | "exit" | "q" => Some(Cmd::Quit),
        "del" | "delete" | "remove" => match rest.first().and_then(|s| s.parse::<u32>().ok()) {
            Some(pid) => Some(Cmd::Del(pid)),
            None => Some(Cmd::Invalid("用法: del <PID>".into())),
        },
        "set" => {
            if rest.len() != 3 && rest.len() != 5 {
                return Some(Cmd::Invalid(
                    "用法: set <PID> <下载KB/s> <上传KB/s> [开秒 停秒]（只填一个数 = 开=停）".into(),
                ));
            }
            let pid = rest[0].parse::<u32>();
            let dl = rest[1].parse::<u64>();
            let ul = rest[2].parse::<u64>();
            let ls = if rest.len() == 5 { rest[3].parse::<u64>() } else { Ok(0) };
            let fs = if rest.len() == 5 { rest[4].parse::<u64>() } else { Ok(0) };
            match (pid, dl, ul, ls, fs) {
                (Ok(pid), Ok(dl), Ok(ul), Ok(limit_secs), Ok(free_secs)) => Some(Cmd::Set {
                    pid,
                    dl,
                    ul,
                    limit_secs,
                    free_secs,
                }),
                _ => Some(Cmd::Invalid("PID/限速值/周期秒数必须是数字".into())),
            }
        }
        other => Some(Cmd::Invalid(format!("未知命令: {other}（可用 list/set/del/quit）"))),
    }
}

/// 循环周期 → 展示文本（不循环返回空串）
fn fmt_cycle(limit_secs: u64, free_secs: u64) -> String {
    if limit_secs > 0 && free_secs > 0 {
        format!(" ｜ 循环：开 {limit_secs}s / 停 {free_secs}s")
    } else {
        String::new()
    }
}

/// 打印规则与实时速率
fn print_rules(
    rules: &Rules,
    prev: &mut std::collections::HashMap<u32, (u64, u64, Instant)>,
    queue: (usize, usize, u64),
) {
    let now = Instant::now();
    let list = rules.snapshot();
    println!("── 规则 {} 条 ─────────────────────────────────────────────", list.len());
    if list.is_empty() {
        println!("   （无）用 set <PID> <下载> <上传> [限速秒 放行秒] 添加");
    } else {
        println!(
            "   {:<8} {:<22} {:>22} {:>22}  {:<12}",
            "PID", "进程", "下载", "上传", "相位"
        );
        for e in &list {
            let dl = e.dl_bytes.load(Ordering::Relaxed);
            let ul = e.ul_bytes.load(Ordering::Relaxed);
            let (pdl, pul, t) = prev.get(&e.pid).copied().unwrap_or((dl, ul, now));
            let dt = now.duration_since(t).as_secs_f64().max(0.001);
            let dl_rate = (dl.saturating_sub(pdl)) as f64 / dt;
            let ul_rate = (ul.saturating_sub(pul)) as f64 / dt;
            // 循环相位：限速中 / 放行中 + 倒计时
            let (l, f) = e.cycle();
            let phase = if l > 0 && f > 0 {
                let (active, remain) = e.cycle_state(now);
                format!("{} {}s", if active { "限速中" } else { "已暂停" }, remain)
            } else {
                "一直限速".to_string()
            };
            println!(
                "   {:<8} {:<22} {:>10} ({}) {:>10} ({})  {:<12}",
                e.pid,
                e.name,
                fmt_rate(dl_rate),
                fmt_limit(e.dl_limit()),
                fmt_rate(ul_rate),
                fmt_limit(e.ul_limit()),
                phase,
            );
            prev.insert(e.pid, (dl, ul, now));
        }
    }
    let (qlen, qbytes, qdrop) = queue;
    println!("   队列 {qlen} 包 / {}，丢包 {qdrop}", fmt_bytes(qbytes as u64));
}

fn run_cli() {
    let specs = match parse_args() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("参数错误: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if specs.is_empty() {
        println!("{USAGE}");
        println!("提示：也可以不带参数启动，然后用 set 命令添加规则。\n");
    }

    // 1) 端口→PID 映射（后台每秒刷新）
    let pmap = Arc::new(ProcessMap::start());

    // 2) 规则表（多进程，双向）+ WinDivert 抓包
    // 精度优化：定时器粒度 1ms（Drop 时恢复）
    let _timer = TimerRes::new();
    let rules = Rules::new();
    for (pid, dl_kb, ul_kb, limit_secs, free_secs) in &specs {
        let name = process_name(*pid);
        // 保护名单：代理/网络关键进程（FlClash 等）永不限速，否则整机断网
        if is_protected(&name) {
            eprintln!("拒绝：{name}（PID {pid}）在保护名单里，未加规则");
            continue;
        }
        rules.set(
            *pid,
            name.clone(),
            dl_kb * 1024,
            ul_kb * 1024,
            *limit_secs,
            *free_secs,
        );
        println!(
            "规则: PID {pid}（{name}） 下载 {} / 上传 {}{}",
            fmt_limit(dl_kb * 1024),
            fmt_limit(ul_kb * 1024),
            fmt_cycle(*limit_secs, *free_secs)
        );
    }

    let mut cap = match Capture::start(&pmap, rules.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            pmap.stop();
            std::process::exit(1);
        }
    };
    println!("已接管网络（WinDivert NETWORK 层）。输入 list/set/del/quit，Ctrl+C 退出。\n");

    // 3) Ctrl+C 与 stdin 命令都只翻转标志/发消息，退出收尾统一在主循环
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || {
            stop.store(true, Ordering::Relaxed);
        });
    }
    let (tx, rx) = mpsc::channel::<Cmd>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if let Some(cmd) = parse_cmd(&line) {
                    if tx.send(cmd).is_err() {
                        break;
                    }
                }
            }
        });
    }

    // 4) 主循环：每 2 秒打印一次速率表，随时处理命令
    let mut prev: std::collections::HashMap<u32, (u64, u64, Instant)> = std::collections::HashMap::new();
    let mut last_print = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));

        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Quit => stop.store(true, Ordering::Relaxed),
                Cmd::List => print_rules(&rules, &mut prev, cap.queue_stats()),
                Cmd::Invalid(msg) => println!("{msg}"),
                Cmd::Del(pid) => {
                    if rules.remove(pid) {
                        prev.remove(&pid);
                        println!("已取消 PID {pid} 的限速（立即生效）");
                    } else {
                        println!("PID {pid} 不在规则表里");
                    }
                }
                Cmd::Set { pid, dl, ul, limit_secs, free_secs } => {
                    let name = process_name(pid);
                    if is_protected(&name) {
                        println!("拒绝：{name}（PID {pid}）在保护名单里，未加规则");
                        continue;
                    }
                    let added =
                        rules.set(pid, name.clone(), dl * 1024, ul * 1024, limit_secs, free_secs);
                    prev.remove(&pid); // 重置速率基线，避免换规则瞬间显示跳变
                    println!(
                        "{}规则: PID {pid}（{name}） 下载 {} / 上传 {}{}",
                        if added { "新增" } else { "已更新" },
                        fmt_limit(dl * 1024),
                        fmt_limit(ul * 1024),
                        fmt_cycle(limit_secs, free_secs)
                    );
                }
            }
        }

        if last_print.elapsed() >= Duration::from_secs(2) {
            print_rules(&rules, &mut prev, cap.queue_stats());
            last_print = Instant::now();
        }
    }

    // 5) 优雅退出：冲刷队列、join 线程、关闭 WinDivert 句柄
    println!("\n收到退出信号，正在恢复网络……");
    cap.shutdown();
    pmap.stop();
    println!("已退出。WinDivert 句柄已关闭，网络恢复正常。");
}
