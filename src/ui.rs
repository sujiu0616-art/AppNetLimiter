//! ui.rs —— eframe/egui 图形界面（第三步）
//!
//! 数据流：capture 线程 → Activity/Rules/Stats（原子/锁）→ UI 每 500ms 采样渲染。
//! UI 只做读快照与发命令（改规则），绝不在 UI 线程碰包路径 → 界面不卡顿。
//!
//! 限速输入格式：纯数字 = KB/s（0 = 不限）；数字 + m/MB = MB/s，如 2m = 2048 KB/s。

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::capture::{Activity, ActivityRow, Rules, Stats};
use crate::config::RuleStore;

/// 队列统计快照（后台线程每 500ms 刷新，UI 无锁读）
#[derive(Default)]
pub struct QueueStatSnap {
    pub len: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
}

impl QueueStatSnap {
    pub fn update(&self, s: (usize, usize, u64)) {
        self.len.store(s.0 as u64, Ordering::Relaxed);
        self.bytes.store(s.1 as u64, Ordering::Relaxed);
        self.dropped.store(s.2, Ordering::Relaxed);
    }
}

/// 渲染期间记录、结束后统一执行的行动作（避开闭包内多次可变借用 self）
enum RowAction {
    Apply { name: String, dl_s: String, ul_s: String, cyc_s: String },
    Clear { name: String },
}

/// 解析循环周期输入 → (限速开着的秒数, 关掉的秒数)。
/// - 留空 = 不循环（一直限速）；
/// - **只填一个数 = 开 X 秒 / 停 X 秒**（最常见用法，如 `600`）；
/// - 填两个数 = 分别指定开、停（如 `600/30`）。
/// 非数字字符一律当分隔符，所以 `600/30`、`600,30`、`600秒 30秒` 都认。
/// 任一值为 0 视为非法：那会变成"限完就永久放行"，与"循环"相反。
pub fn parse_cycle(s: &str) -> Result<(u64, u64), String> {
    let mut nums: Vec<u64> = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            nums.push(cur.parse::<u64>().map_err(|_| format!("无法解析数字『{cur}』"))?);
            cur.clear();
        }
    }
    if !cur.is_empty() {
        nums.push(cur.parse::<u64>().map_err(|_| format!("无法解析数字『{cur}』"))?);
    }
    match nums.len() {
        0 => Ok((0, 0)), // 留空 = 不循环
        1 => {
            if nums[0] == 0 {
                return Err("周期秒数必须大于 0（留空 = 一直限速）".into());
            }
            Ok((nums[0], nums[0]))
        }
        2 => {
            if nums[0] == 0 || nums[1] == 0 {
                return Err("『开』和『关』的秒数都必须大于 0".into());
            }
            Ok((nums[0], nums[1]))
        }
        _ => Err(format!("周期应填一个数（开=关）或『开/关』，如 600 或 600/30，收到『{s}』")),
    }
}

/// (开秒, 关秒) → 输入框文本；两者相等时只显示一个数（与输入习惯一致）
fn cycle_to_text(limit: u64, free: u64) -> String {
    if limit > 0 && free > 0 {
        if limit == free {
            format!("{limit}")
        } else {
            format!("{limit}/{free}")
        }
    } else {
        String::new()
    }
}

/// 解析限速输入 → KB/s。宽容写法：任意大小写与空格，允许带单位后缀；**留空 = 不限（0）**。
/// 例：""/"0"→0；"500"/"500KB"/"500KB/s"/"500kbps"/"500k"→500；
/// "2m"/"2MB"/"2MB/s"/"2mbps"→2048；"1.5m"→1536。
/// 单位里含 m/M 即按 MB 折算，否则按 KB。
pub fn parse_limit_kbps(s: &str) -> Result<u64, String> {
    let t: String = s
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if t.is_empty() {
        // 只填一个方向、另一个留空是很自然的用法：留空按"不限"处理
        return Ok(0);
    }
    // 取开头的数字/小数点部分作为数值，其余当单位
    let digits: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() {
        return Err(format!("无法解析『{s}』：应以数字开头，如 1、500、2m"));
    }
    let v: f64 = digits
        .parse()
        .map_err(|_| format!("无法解析数字『{digits}』"))?;
    if !v.is_finite() || v < 0.0 {
        return Err("数值非法".into());
    }
    let unit = &t[digits.len()..];
    // 单位里出现 m/M（mb、mbps、m/s…）→ MB；其余（kb、kbps、/s、空）按 KB
    let is_mb = unit.contains('m');
    let kbps = if is_mb { v * 1024.0 } else { v };
    // 小于 1 KB/s 的输入不能被截断成 0：0 的语义是"不限"，
    // 静默解除限速是最危险的失败方向（如输入 0.5 以为限住了，其实完全放开）
    if v > 0.0 && kbps < 1.0 {
        return Err(format!("『{s}』太小：最小 1 KB/s（0 = 不限）"));
    }
    Ok(kbps as u64)
}

/// 字节/秒 → 可读
fn fmt_rate(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bps / 1024.0 / 1024.0)
    } else if bps >= 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else {
        format!("{bps:.0} B/s")
    }
}

/// 字节数 → 可读（总量/队列占用用这个，不能用 fmt_rate，否则会显示成 x/s）
fn fmt_bytes(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.2} MB", b as f64 / 1024.0 / 1024.0)
    } else if b >= 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

/// B/s 限速值 → 输入框预填文本（KB/s 取整，整 MB 用 m 后缀）
fn limit_to_text(bps: u64) -> String {
    if bps == 0 {
        String::new()
    } else if bps >= 1024 * 1024 && bps % (1024 * 1024) == 0 {
        format!("{}m", bps / 1024 / 1024)
    } else {
        format!("{}", bps / 1024)
    }
}

/// 列表头：标题 + 右上角的可悬停说明标识。
///
/// 为什么要有可见标识：tooltip 本身是"看不见的功能"，用户不知道能悬停；
/// 放一个小的蓝色「?」在标题右上方，既点明那里能看说明，又不占表格横向空间。
fn header_with_hint(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).strong());
        // 上标式小标识：用一个 10px 高的顶对齐小区域，避免拉高表头行
        ui.allocate_ui_with_layout(
            egui::vec2(8.0, 10.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("?")
                            .size(9.0)
                            .color(egui::Color32::from_rgb(80, 132, 214)),
                    )
                    .sense(egui::Sense::hover()),
                )
                .on_hover_text(hint);
            },
        );
    });
}

/// 注册中文字体（微软雅黑 → 黑体 → 宋体，取第一个可读的）
fn install_cjk_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\simsun.ttc",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(bytes));
            // 追加到各字体族末尾：缺字符时回落到中文字体
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(list) = fonts.families.get_mut(&fam) {
                    list.push("cjk".into());
                }
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("警告：未找到系统中文字体，界面中文可能显示为方块");
}

/// 主应用
pub struct LimiterApp {
    store: Arc<RuleStore>,
    rules: Rules,
    activity: Arc<Activity>,
    stats: Arc<Stats>,
    qstats: Arc<QueueStatSnap>,
    exit_flag: Arc<AtomicBool>,
    /// 搜索过滤（进程名子串，不区分大小写）
    search: String,
    /// 排序方式：false=按进程名（默认，行位置稳定不跳）；true=按速率降序
    sort_by_rate: bool,
    /// 进程名 → (下载输入, 上传输入)
    edits: HashMap<String, (String, String, String)>,
    /// pid → (dl累计, ul累计, 采样时刻)：速率计算基线（只在采样点更新）
    rates: HashMap<u32, (u64, u64, Instant)>,
    /// pid → (下载 B/s, 上传 B/s)：采样点算好的速率（渲染只读）
    rate_bps: HashMap<u32, (f64, f64)>,
    /// 最近一次快照（500ms 刷新）
    rows: Vec<ActivityRow>,
    /// 底部状态消息
    status: String,
    last_sample: Instant,
    /// APP_LIMITER_STATS 指定的导出文件（自动化验证用）
    stats_dump: Option<std::path::PathBuf>,
    last_dump: Instant,
}

impl LimiterApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        store: Arc<RuleStore>,
        rules: Rules,
        activity: Arc<Activity>,
        stats: Arc<Stats>,
        qstats: Arc<QueueStatSnap>,
        exit_flag: Arc<AtomicBool>,
    ) -> Self {
        install_cjk_fonts(&cc.egui_ctx);
        Self {
            store,
            rules,
            activity,
            stats,
            qstats,
            exit_flag,
            search: String::new(),
            sort_by_rate: false,
            edits: HashMap::new(),
            rates: HashMap::new(),
            rate_bps: HashMap::new(),
            rows: Vec::new(),
            status: "就绪。修改限速后点『应用』立即生效并保存到 config.json。".into(),
            last_sample: Instant::now(),
            stats_dump: std::env::var_os("APP_LIMITER_STATS").map(std::path::PathBuf::from),
            last_dump: Instant::now(),
        }
    }

    /// 500ms 采样：活动进程快照 + 速率计算 + reconciler + 导出
    fn sample(&mut self) {
        let now = Instant::now();
        let rows = self.activity.snapshot();
        // reconciler：新出现的进程自动套用其名字的规则
        let active: Vec<(u32, String)> = rows.iter().map(|r| (r.pid, r.name.clone())).collect();
        self.store.reconcile(&active);

        // 速率：限速进程用"放行计数"，其余进程直通（到达计数即放行计数）
        let mut fresh = HashMap::with_capacity(rows.len());
        let mut bps = HashMap::with_capacity(rows.len());
        for r in &rows {
            let (dl, ul) = match self.rules.get(r.pid) {
                Some(l) => (
                    l.dl_bytes.load(Ordering::Relaxed),
                    l.ul_bytes.load(Ordering::Relaxed),
                ),
                None => (r.dl, r.ul),
            };
            let (rdl, rul) = match self.rates.get(&r.pid) {
                Some(&(pdl, pul, t)) => {
                    let dt = now.duration_since(t).as_secs_f64().max(0.001);
                    (
                        (dl.saturating_sub(pdl)) as f64 / dt,
                        (ul.saturating_sub(pul)) as f64 / dt,
                    )
                }
                None => (0.0, 0.0),
            };
            fresh.insert(r.pid, (dl, ul, now));
            bps.insert(r.pid, (rdl, rul));
        }
        // 基线/速率只保留仍活跃的 PID（退出的进程不泄漏）
        self.rates = fresh;
        self.rate_bps = bps;
        self.rows = rows;

        // 导出统计（自动化验证通道）
        if self.stats_dump.is_some() && self.last_dump.elapsed() >= Duration::from_millis(500) {
            self.dump_stats();
            self.last_dump = Instant::now();
        }
    }

    fn dump_stats(&self) {
        let Some(path) = &self.stats_dump else { return };
        let mut text = String::new();
        for r in &self.rows {
            let (dl_lim, ul_lim) = self.store.get(&r.name).map(|l| (l.dl, l.ul)).unwrap_or((0, 0));
            let (rdl, rul) = self.rate_bps.get(&r.pid).copied().unwrap_or((0.0, 0.0));
            // 第 9 列：该进程被丢弃的包数（limiter 层：重传副本 / 滞留过久 / 队满），
            // 与队列层的 dropped 是两条不同路径
            let drop = self.rules.get(r.pid).map(|l| l.dropped_so_far()).unwrap_or(0);
            text.push_str(&format!(
                "{} {} {} {} {} {} {:.1} {:.1} {}\n",
                r.pid, r.name, r.dl, r.ul, dl_lim, ul_lim, rdl, rul, drop
            ));
        }
        // 全局诊断行（匹配层计数）
        text.push_str(&format!(
            "# global limited={} unmapped={} unparsed={} unruled={} seen={} bytes={}\n",
            self.stats.limited_pkts.load(Ordering::Relaxed),
            self.stats.unmapped_pkts.load(Ordering::Relaxed),
            self.stats.unparsed_pkts.load(Ordering::Relaxed),
            self.stats.unruled_pkts.load(Ordering::Relaxed),
            self.stats.seen_pkts.load(Ordering::Relaxed),
            self.stats.seen_bytes.load(Ordering::Relaxed),
        ));
        if let Ok(mut f) = std::fs::File::create(path) {
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// 应用/清除一条名字规则（对同名所有活动 PID 立即生效）
    fn apply_action(&mut self, act: RowAction) {
        let active: Vec<(u32, String)> =
            self.rows.iter().map(|r| (r.pid, r.name.clone())).collect();
        match act {
            RowAction::Apply { name, dl_s, ul_s, cyc_s } => {
                match (
                    parse_limit_kbps(&dl_s),
                    parse_limit_kbps(&ul_s),
                    parse_cycle(&cyc_s),
                ) {
                    (Ok(dl), Ok(ul), Ok((limit_secs, free_secs))) => {
                        match self.store.set(&name, dl, ul, limit_secs, free_secs, &active) {
                            Ok(()) => {
                                let cyc = if limit_secs > 0 && free_secs > 0 {
                                    format!("，循环 开{limit_secs}s/停{free_secs}s")
                                } else {
                                    String::new()
                                };
                                self.status = format!(
                                    "已应用并保存：{name} ↓{} ↑{}{cyc}（对同名所有实例立即生效）",
                                    if dl == 0 { "不限".to_string() } else { format!("{dl} KB/s") },
                                    if ul == 0 { "不限".to_string() } else { format!("{ul} KB/s") }
                                );
                            }
                            Err(e) => self.status = e,
                        }
                    }
                    (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                        self.status = format!("输入有误：{e}");
                    }
                }
            }
            RowAction::Clear { name } => match self.store.set(&name, 0, 0, 0, 0, &active) {
                Ok(()) => {
                    self.edits.remove(&name);
                    self.status = format!("已取消 {name} 的限速并保存");
                }
                Err(e) => self.status = e,
            },
        }
    }
}

impl eframe::App for LimiterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Ctrl+C → 置退出标志 → 这里转成窗口关闭命令，走统一收尾路径
        if self.exit_flag.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // 采样节流：500ms（事件驱动重绘不受影响，输入不会卡）
        if self.last_sample.elapsed() >= Duration::from_millis(500) {
            self.sample();
            self.last_sample = Instant::now();
        }

        // ── 顶栏：全局统计 ──────────────────────────────
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("AppNetLimiter");
                ui.separator();
                let seen = self.stats.seen_bytes.load(Ordering::Relaxed);
                let (qlen, qbytes, qdrop) = (
                    self.qstats.len.load(Ordering::Relaxed),
                    self.qstats.bytes.load(Ordering::Relaxed),
                    self.qstats.dropped.load(Ordering::Relaxed),
                );
                ui.label(format!(
                    "经过总量 {} │ 队列 {} 包 / {} │ 丢包 {}",
                    fmt_bytes(seen),
                    qlen,
                    fmt_bytes(qbytes),
                    qdrop
                ));
                // 限速层丢包（重传副本 / 滞留过久 / 队满）：低速率档真正在丢包的是这条路径，
                // 队列层的 dropped 常为 0，单看它会误判成"一个包都没丢"
                let lim_drop: u64 = self.rules.snapshot().iter().map(|l| l.dropped_so_far()).sum();
                if lim_drop > 0 {
                    ui.label(
                        egui::RichText::new(format!("│ 限速丢包 {lim_drop}"))
                            .color(egui::Color32::from_rgb(255, 140, 40)),
                    );
                }
                // 诊断：四个匹配层的计数，游戏限不住时一眼看出漏在哪层
                let (limited, unmapped, unparsed, unruled) = (
                    self.stats.limited_pkts.load(Ordering::Relaxed),
                    self.stats.unmapped_pkts.load(Ordering::Relaxed),
                    self.stats.unparsed_pkts.load(Ordering::Relaxed),
                    self.stats.unruled_pkts.load(Ordering::Relaxed),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "│ 限速命中 {limited} │ 无进程映射 {unmapped} │ 未解析 {unparsed} │ 无规则 {unruled}"
                    ))
                    .small()
                    .color(egui::Color32::GRAY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("退出").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });

        // ── 底栏：状态 ──────────────────────────────────
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&self.status).small().color(egui::Color32::GRAY));
            });
        });

        // ── 工具栏：搜索 ────────────────────────────────
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("过滤:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .desired_width(180.0)
                        .hint_text("进程名，如 chrome"),
                );
                if ui.button("清空").clicked() {
                    self.search.clear();
                }
                ui.checkbox(&mut self.sort_by_rate, "按速率排序");

            });
        });

        // ── 主表：进程列表 ──────────────────────────────
        let mut actions: Vec<RowAction> = Vec::new();
        egui::CentralPanel::default().show(ctx, |ui| {
            let search = self.search.to_lowercase();
            // 渲染数据先克隆成纯值，渲染闭包不再借 self.rows
            let mut display: Vec<(u32, String, f64, f64, bool)> = self
                .rows
                .iter()
                .filter(|r| search.is_empty() || r.name.to_lowercase().contains(&search))
                .map(|r| {
                    let (rdl, rul) = self.rate_bps.get(&r.pid).copied().unwrap_or((0.0, 0.0));
                    let has_rule = self
                        .store
                        .get(&r.name)
                        .map(|l| l.dl > 0 || l.ul > 0)
                        .unwrap_or(false);
                    (r.pid, r.name.clone(), rdl, rul, has_rule)
                })
                .collect();
            // 排序：默认按进程名（行位置稳定，不会因为速率每秒变化而乱跳）；
            // 勾选「按速率排序」后才按速率降序。同名多实例再按 PID 稳定排序。
            if self.sort_by_rate {
                display.sort_by(|a, b| {
                    (b.2 + b.3)
                        .total_cmp(&(a.2 + a.3))
                        .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
                        .then(a.0.cmp(&b.0))
                });
            } else {
                display.sort_by(|a, b| {
                    a.1.to_lowercase()
                        .cmp(&b.1.to_lowercase())
                        .then(a.0.cmp(&b.0))
                });
            }

            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(56.0)) // PID
                .column(Column::initial(132.0)) // 进程
                .column(Column::exact(84.0)) // 下载速率
                .column(Column::exact(84.0)) // 上传速率
                .column(Column::exact(76.0)) // 下载限速
                .column(Column::exact(76.0)) // 上传限速
                .column(Column::exact(88.0)) // 循环周期
                .column(Column::exact(92.0)) // 当前相位
                .column(Column::exact(112.0)) // 操作
                .header(24.0, |mut h| {
                    h.col(|ui| {
                        ui.strong("PID");
                    });
                    h.col(|ui| {
                        ui.strong("进程");
                    });
                    h.col(|ui| {
                        ui.strong("↓ 下载");
                    });
                    h.col(|ui| {
                        ui.strong("↑ 上传");
                    });
                    h.col(|ui| {
                        header_with_hint(
                            ui,
                            "↓ 限速",
                            "下载速度上限：\n数字 = KB/s（如 500），带 m = MB/s（如 2m），0 或留空 = 不限",
                        );
                    });
                    h.col(|ui| {
                        header_with_hint(
                            ui,
                            "↑ 限速",
                            "上传速度上限：\n数字 = KB/s（如 500），带 m = MB/s（如 2m），0 或留空 = 不限",
                        );
                    });
                    h.col(|ui| {
                        header_with_hint(
                            ui,
                            "开/停(秒)",
                            "开/停周期（自动循环）\n\
                             留空 = 不循环（一直限速）\n\
                             填 600 = 限速 600 秒，然后全程不限 600 秒，如此循环\n\
                             填 600/60 = 开 600 秒、停 60 秒\n\
                             填完按回车或点『应用』生效；右侧『当前相位』显示倒计时",
                        );
                    });
                    h.col(|ui| {
                        ui.strong("当前相位");
                    });
                    h.col(|ui| {
                        ui.strong("操作");
                    });
                })
                .body(|mut body| {
                    for (pid, name, dl_rate, ul_rate, has_rule) in display {
                        // 编辑状态按名字共享（同名多实例共用一套规则）
                        let (mut dl_s, mut ul_s, mut cyc_s) =
                            self.edits.get(&name).cloned().unwrap_or_else(|| {
                                let r = self.store.get(&name);
                                (
                                    limit_to_text(r.map(|l| l.dl).unwrap_or(0)),
                                    limit_to_text(r.map(|l| l.ul).unwrap_or(0)),
                                    cycle_to_text(
                                        r.map(|l| l.limit_secs).unwrap_or(0),
                                        r.map(|l| l.free_secs).unwrap_or(0),
                                    ),
                                )
                            });
                        // 当前相位（限速中 / 放行中 + 倒计时）
                        let phase = self
                            .store
                            .get(&name)
                            .filter(|l| l.cycles())
                            .map(|_| match self.rules.get(pid) {
                                Some(l) => {
                                    let (active, remain) = l.cycle_state(Instant::now());
                                    Some(if active {
                                        format!("限速中 {}s", remain)
                                    } else {
                                        format!("放行中 {}s", remain)
                                    })
                                }
                                None => None,
                            })
                            .flatten();
                        let mut do_apply = false;
                        let mut do_clear = false;
                        body.row(26.0, |mut row| {
                            row.col(|ui| {
                                ui.label(pid.to_string());
                            });
                            row.col(|ui| {
                                let text = if has_rule {
                                    egui::RichText::new(&name)
                                        .color(egui::Color32::from_rgb(255, 176, 32))
                                } else {
                                    egui::RichText::new(&name)
                                };
                                ui.label(text);
                            });
                            row.col(|ui| {
                                ui.label(fmt_rate(dl_rate));
                            });
                            row.col(|ui| {
                                ui.label(fmt_rate(ul_rate));
                            });
                            row.col(|ui| {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut dl_s)
                                        .id_source(("dl_limit", name.as_str())) // 稳定 id：行位置变化也不丢焦点
                                        .desired_width(60.0)
                                        .hint_text("如 1 / 2m"),
                                );
                                // 回车直接应用，省掉点按钮
                                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    do_apply = true;
                                }
                            });
                            row.col(|ui| {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut ul_s)
                                        .id_source(("ul_limit", name.as_str()))
                                        .desired_width(60.0)
                                        .hint_text("如 1 / 2m"),
                                );
                                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    do_apply = true;
                                }
                            });
                            row.col(|ui| {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut cyc_s)
                                        .id_source(("cycle", name.as_str()))
                                        .desired_width(76.0)
                                        .hint_text("关"),
                                );
                                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    do_apply = true;
                                }
                            });
                            row.col(|ui| match &phase {
                                Some(p) => {
                                    let color = if p.starts_with("限速") {
                                        egui::Color32::from_rgb(255, 140, 40)
                                    } else {
                                        egui::Color32::from_rgb(120, 200, 120)
                                    };
                                    ui.label(egui::RichText::new(p).small().color(color));
                                }
                                None => {
                                    ui.label(egui::RichText::new("—").small().color(egui::Color32::GRAY));
                                }
                            });
                            row.col(|ui| {
                                ui.horizontal(|ui| {
                                    do_apply = ui.button("应用").clicked();
                                    do_clear = ui.button("清除").clicked();
                                });
                            });
                        });
                        // 输入内容落回编辑状态
                        self.edits
                            .insert(name.clone(), (dl_s.clone(), ul_s.clone(), cyc_s.clone()));
                        if do_apply {
                            actions.push(RowAction::Apply { name: name.clone(), dl_s, ul_s, cyc_s });
                        }
                        if do_clear {
                            actions.push(RowAction::Clear { name });
                        }
                    }
                });
        });

        // 表格闭包结束，执行本帧动作（此时 self 可自由借用）
        for act in actions {
            self.apply_action(act);
        }

        // 空闲时节流重绘（输入事件会立即触发，不牺牲交互）
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_limits() {
        assert_eq!(parse_limit_kbps("500").unwrap(), 500);
        assert_eq!(parse_limit_kbps("0").unwrap(), 0);
        assert_eq!(parse_limit_kbps("2m").unwrap(), 2048);
        assert_eq!(parse_limit_kbps("2M").unwrap(), 2048);
        assert_eq!(parse_limit_kbps("1.5mb").unwrap(), 1536);
        assert_eq!(parse_limit_kbps(" 8 ").unwrap(), 8);
        assert_eq!(parse_limit_kbps("300kb").unwrap(), 300);
        assert!(parse_limit_kbps("abc").is_err());
        assert_eq!(parse_limit_kbps("").unwrap(), 0); // 留空 = 不限（只填一个方向的常见用法）
        assert!(parse_limit_kbps("-5").is_err());
        // 小于 1 KB/s 必须报错，不能截断成 0（0 = 不限，会静默解除限速）
        assert!(parse_limit_kbps("0.5").is_err());
        assert!(parse_limit_kbps("0.0001m").is_err());
        assert_eq!(parse_limit_kbps("0.0").unwrap(), 0); // 显式 0 仍是"不限"
        // 宽容写法：用户按提示常输入带 /s 的文本，必须识别（实测踩过：输入 1kb/s 被判非法）
        assert_eq!(parse_limit_kbps("1kb/s").unwrap(), 1);
        assert_eq!(parse_limit_kbps("1 KB/s").unwrap(), 1);
        assert_eq!(parse_limit_kbps("1k/s").unwrap(), 1);
        assert_eq!(parse_limit_kbps("2MB/s").unwrap(), 2048);
        assert_eq!(parse_limit_kbps("500kbps").unwrap(), 500);
        assert_eq!(parse_limit_kbps("2mbps").unwrap(), 2048);
        assert_eq!(parse_limit_kbps("1").unwrap(), 1);
    }

    #[test]
    fn limit_text_roundtrip() {
        assert_eq!(limit_to_text(0), "");
        assert_eq!(limit_to_text(8 * 1024), "8");
        assert_eq!(limit_to_text(2 * 1024 * 1024), "2m");
        // 往返
        assert_eq!(parse_limit_kbps(&limit_to_text(300 * 1024)).unwrap(), 300);
    }

    #[test]
    fn parse_cycles() {
        // 留空 = 不循环
        assert_eq!(parse_cycle("").unwrap(), (0, 0));
        assert_eq!(parse_cycle("   ").unwrap(), (0, 0));
        // 常见写法都认
        assert_eq!(parse_cycle("600/30").unwrap(), (600, 30));
        assert_eq!(parse_cycle(" 600 / 30 ").unwrap(), (600, 30));
        assert_eq!(parse_cycle("600,30").unwrap(), (600, 30));
        assert_eq!(parse_cycle("600秒/30秒").unwrap(), (600, 30));
        assert_eq!(parse_cycle("60s-10s").unwrap(), (60, 10));
        // 非法：只填一个数、或其中一个为 0（语义会变成"限完就永久放行"）
        assert_eq!(parse_cycle("600").unwrap(), (600, 600)); // 单值 = 开=停
        assert!(parse_cycle("600/0").is_err());
        assert!(parse_cycle("0/30").is_err());
        assert!(parse_cycle("1/2/3").is_err());
        // 反向显示
        assert_eq!(cycle_to_text(600, 30), "600/30");
        assert_eq!(cycle_to_text(600, 600), "600");
        assert_eq!(cycle_to_text(0, 0), "");
        assert_eq!(cycle_to_text(600, 0), "");
    }
}
