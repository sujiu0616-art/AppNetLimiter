# AppNetLimiter —— Windows 按进程限速工具

用 WinDivert 2.2 在用户态拦截/整形数据包，按进程分别限制上传、下载速度。

**当前进度：第三步（GUI 版，全部三步完成）** —— egui 界面显示活动进程与实时速率，双向限速即时生效，规则持久化到 config.json。
命令行模式仍保留：加 `--cli` 启动（用法见下）。

## 用法（第三步：GUI）

管理员运行 `target\release\AppNetLimiter.exe`（已内嵌 manifest，双击弹 UAC）：

- **进程表**：列出当前有网络活动的进程（PID / 进程名 / ↓下载速率 / ↑上传速率），500ms 刷新；
  有规则的进程名显示为橙色，且排在最前
- **限速输入**：↓限速 / ↑限速 两个框，纯数字 = KB/s，加 `m` 后缀 = MB/s（如 `2m` = 2048 KB/s），空/0 = 不限
- **应用**：立即对**同名所有实例**生效，并写入 exe 同目录 `config.json`
- **清除**：取消该进程名限速（立即恢复），并从配置里删除
- **过滤**：按进程名筛选
- 关闭窗口 / Ctrl+C / 点「退出」：冲刷延迟队列 → 关闭 WinDivert 句柄 → 网络恢复

配置文件 `config.json`（exe 同目录，按**进程名**保存，重启后自动套用到新 PID）：

```json
{
  "version": 1,
  "rules": [
    { "name": "chrome.exe", "dl_kbps": 300, "ul_kbps": 50 },
    { "name": "curl.exe",   "dl_kbps": 8,   "ul_kbps": 0  }
  ]
}
```

- 大小写不敏感；`0` 表示该方向不限速；带 UTF-8 BOM 的文件也能读（记事本 / PowerShell 5.1 写的）
- 新启动的同名进程会被自动套用规则（每秒 reconcile）
- 写入采用「先写 .tmp 再改名」，断电不会写坏

实测（GUI 版，release）：预置 `curl.exe dl=8` → 界面限速框显示 `8`、curl 行 ↓下载 7.4 KB/s、
实测放行 9078 B/s（≈8 KB/s）；其他进程零影响；关闭窗口 2 秒内网络恢复（ping 通）。

已知视觉瑕疵：glow 渲染下窗口左侧有一条窄黑边（不影响功能，后续可打磨）。

## 用法（第二步：CLI，`--cli`）

```powershell
# 多条规则：--rule <PID>:<下载KB/s>:<上传KB/s>（0 = 不限）
AppNetLimiter.exe --cli --rule 4012:300:50 --rule 5678:0:20

# 简写（单条规则）
AppNetLimiter.exe --cli --pid 4012 --dl 300 --ul 50

# 不带参数启动，再用交互命令添加
AppNetLimiter.exe --cli
> set 4012 300 50      # 新增/修改：下载 300KB/s、上传 50KB/s（立即生效）
> list                 # 显示规则与实时速率
> del 4012             # 取消该进程限速（立即恢复）
> quit
```

调试用：`set APP_LIMITER_LOOPBACK=1` 时过滤器也抓环回流量（本地验证上传/下载路径用）。

实测（release，双进程 + 上传 + 运行时改规则）：
| 场景 | 设定 | 实测 |
|---|---|---|
| 双进程下载 | PID A 20 / PID B 8 KB/s | 21.9 / 8.8 KB/s，队列 3 包，丢包 0 |
| 上传（环回调试） | 50 KB/s | 49.7 / 50.4 KB/s |
| 运行时 set | 20 → 4 KB/s | 更新后稳定 4.4 KB/s |
| 运行时 del | 取消 | 规则表清空、队列立即清空 |

## 目录结构

```
Cargo.toml          # 依赖与 release 瘦身配置
build.rs            # 编译 app.rc（图标 + manifest）
app.rc              # 资源脚本：1 ICON 图标 + 1 24 管理员 manifest
app.manifest        # requireAdministrator + UTF-8 代码页
assets/app.ico      # 程序图标（16–256 共 7 帧，exe 资源 + 窗口/任务栏图标）
src/
  main.rs           # 入口：GUI / --cli 分流、TimerRes、窗口图标、收尾 shutdown
  ui.rs             # egui 界面：进程表、双向限速输入、中文字体、统计导出
  config.rs         # 规则持久化（serde_json，按进程名，含 BOM 容错）
  capture.rs        # WinDivert 抓包/双向裁决/重注入（capture + scheduler 双线程 + 规则表 + Activity）
  process_map.rs    # GetExtendedTcpTable/UdpTable → 端口→PID 映射（1s 周期 + 查不到时即时补刷）
  limiter.rs        # 调度式令牌桶 + 延迟队列（Mutex + Condvar）
WinDivert/          # 官方 WinDivert 2.2.2 运行库（x64）
```

## 环境要求

- Rust stable（MSVC 工具链）+ Visual Studio C++ Build Tools
- WinDivert 2.2.2（本仓库已带 `WinDivert/WinDivert-2.2.2-A/`）
- 运行需要管理员权限（exe 已嵌 manifest，双击自动弹 UAC）

## WinDivert 文件放哪 / 从哪下

- 下载：<https://github.com/basil00/WinDivert/releases/download/v2.2.2/WinDivert-2.2.2-A.zip>
- **编译时**：设置环境变量 `WINDIVERT_PATH` 指向含 `WinDivert.dll/.lib/.sys` 的 `x64` 目录
  （例：`setx WINDIVERT_PATH "%CD%\WinDivert\WinDivert-2.2.2-A\x64"`，值请按你的实际目录调整）
- **运行时**：`WinDivert.dll` 和 `WinDivert64.sys` 必须与 `AppNetLimiter.exe` 同目录

## 编译

```powershell
cargo build            # 调试版
cargo build --release  # 发布版（约 190KB：opt-level="z" + lto + strip + panic="abort"）
```

## 精度优化（已实现）

| 手段 | 实现位置 | 要点 |
|---|---|---|
| 系统定时器 1ms | `main.rs` TimerRes | `timeBeginPeriod(1)` + Drop 里 `timeEndPeriod(1)`；Condvar 睡眠粒度 15.6ms → ~1ms |
| 先睡后自旋 | `limiter.rs` `pop_due` | 离到期 >1.5ms 用 Condvar 睡眠；最后窗口内 `spin_loop()` 忙等到期，亚毫秒放行；高密度（5ms 内有弹出）自旋窗口自动缩到 100µs，避免整核自旋 |
| 高精度时钟 | `limiter.rs` TokenBucket | `Instant`（底层 QPC），维护"债务清零时刻 next_free"，按 `n/rate` 精确推进 |
| burst 限幅 | `limiter.rs` 常量 | 突发窗口 15ms，上限 256KB；不设大下限（3000B 下限曾造成 8KB/s 档 ±30% 锯齿） |
| 线程优先级 | `capture.rs` | 调度线程 `THREAD_PRIORITY_HIGHEST`、抓包线程 `ABOVE_NORMAL` |
| 累计误差校正 | `limiter.rs` next_free | next_free 是绝对时间线，调度晚醒不移动它 → 少发自动补、多发自动扣，长期平均速率自动收敛、无需额外账本 |
| 驱动队列调优 | `capture.rs` tune_queue_params | 8192 包 / 8MB（默认 4096 包 / 4MB），降低单线程抓包抖动时驱动侧丢包 |
| 下载段去重 | `capture.rs` `charge_dl_seg` | 入站 TCP 段按序号记账：同一段只扣一次额度，**重传副本直接丢弃**（不放行、不占额度） |
| 流表按进程 | `capture.rs` `FlowKey` | 每条连接（本地端口 + 对端地址/端口）独立时间线；>512 条流时惰性回收空闲项 |

**下载整形的关键结论（2026-11-21，反复实测定稿）**

1. 旧版把重传副本也放行并扣额度 → 1 KB/s 档额度被重复数据吃光，稳态应用层 **0 B/s**；
2. 只去重、仍放行副本 → 1 KB/s 档跑到 **2 倍**；
3. 去重 + 副本丢弃 → 各档都贴上限（见下表）。这是最终实现的路径。
4. 试过但不采用的方案：出站 ACK 节流、改写 TCP 通告窗口（含解析 Window Scale）——
   前者一条 ACK 覆盖几十 KB、节拍过粗（实测 148 KB/s 或直接 0）；
   后者需要同时压窗口与推迟 ACK，实测 1 KB/s 只有 32%~55%、8 KB/s 仅 13%，不如入站延迟。

实测（release，逐秒计数，30 s 稳态窗口，客户端为原生 socket 直连下载）：

| 限速 | 实测 | 达成率 |
|---|---|---|
| 1 KB/s | 0.89–0.95 KB/s | 89%–95% |
| 2 KB/s | 2.11 KB/s | 106% |
| 4 KB/s | 3.94–4.03 KB/s | 98%–105% |
| 8 KB/s | 8.09–8.67 KB/s | 101%–106% |

CPU 占用 <1% 单核。

**超低速率档（≤1 KB/s）的残留现象**：WinDivert 抓到的是**已重组的 IP 包**（GRO），入站单包可达 4~9KB；
1 KB/s 时一个 1.4KB 段要排 1.4 秒，秒级统计会出现 `0 / 1.4KB / 0` 的锯齿（30 s 均值仍贴着上限）。
这是包级整形的物理下限，要彻底平滑需要流级整形（WFP callout / 内核驱动），不是用户态包 shaper 能解决的。

两处实现要点（都是实测踩出来的）：
- **丢弃必须退还额度**：`queue.push` 失败后必须 `bucket.refund(n)`，否则被丢的包仍占用时间线，`next_free` 被推远后每个新包都超限 → 全量丢包、吞吐冻结（实测 8KB/s 档偏差 -92%）。
- **重传副本不能放行**：放行就会绕过节流（1 KB/s 实测超到 2 倍）；直接丢这些副本与接收端 TCP 的行为等价，不会丢数据。

## 循环：开 X 秒 / 停 X 秒

限速可以按周期自动开关——「开」的相位限速生效，「停」的相位完全不限，如此循环。

- **GUI**：每行新增「开/停(秒)」输入框。**填一个数（如 `600`）= 限速 600 秒、停 600 秒**；
  也可以分开写 `600/30`（开 600 秒、停 30 秒）。留空 = 一直限速。
  右侧「当前相位」列实时显示 `限速中 123s` / `已暂停 45s` 倒计时。
- **CLI**：`--rule 4012:8:0:600:600`（PID:下载:上传:开秒:停秒），或在交互模式里
  `set 4012 8 0 600 600`。
- **config.json**：每条规则多了 `limit_secs` / `free_secs` 两个字段；旧配置没有这两个字段，
  读取时按 0 处理（= 不循环），不影响兼容。

实现要点（`capture.rs` `active_at` / `take_phase_change` / `limiter.rs` `purge_where`）：

1. 相位从规则生效时刻起算：`已过秒数 % (开+停) < 开` → 限速中，否则暂停。热路径每个包只做一次取模。
2. **切到「停」相位时清掉该进程排队中的积压包**：不清的话积压会继续按旧的到期时刻慢放，
   「停」相位放不干净（实测 1 KB/s + 6s开/4s停 时队列积压 190 包 / 273 KB）。
   丢掉这些包对 TCP 无害——重传会重发，而「停」相位里重传是直通的。
3. 两个方向切换时都把下载时间线归零，旧债务不带到新相位。

实测（curl，1 KB/s，30 秒窗口）：

| 配置 | curl 收到字节 | 说明 |
|---|---|---|
| 不循环（1 KB/s） | 112,634 B | 全程限速 |
| 开 5 秒 / 停 5 秒 | 1,315,462 B | 约 11.7 倍，一半时间全速 |

同一进程的 CLI 输出同时给出相位，便于核对：`限速中 16s` / `已暂停 3s`。

## 卸载与残留

程序**不写注册表、不写 AppData、没有自启动项**，所有状态只有两处：

| 位置 | 内容 | 删除目录后 |
|---|---|---|
| exe 同目录 `config.json` | 限速规则 | 随目录一起消失 |
| 系统服务 `WinDivert` + 内核驱动 | WinDivert 抓包驱动（指向 exe 同目录的 `WinDivert64.sys`） | **会残留**，见下 |

彻底卸载（三步）：

1. 退出 AppNetLimiter（窗口关闭 / 托盘退出）；
2. 运行目录里的 `uninstall.bat`（**右键 → 以管理员身份运行**）：它会结束进程、`sc stop` + `sc delete WinDivert`；
3. 删除整个程序目录。

不跑 `uninstall.bat` 直接删目录也能用，只是会留下一个指向已删除文件的 `WinDivert` 服务项（程序启动时会把它标记为删除），**重启一次即自动清除**，不影响系统和其他软件。
想立刻确认是否干净：管理员终端执行 `sc query WinDivert`，返回 `1060`（服务不存在）就是干净了。

## 保护名单（不会误伤代理）

`FlClash.exe / mihomo.exe / clash.exe / clash-meta.exe / clash-verge.exe / clash-verge-service.exe`
永不接受限速规则（UI 报错、`config.json` 里同名规则加载时被忽略、CLI `--rule` 也会拒绝）。
原因：这些进程的 TUN 会把整机流量过一遍，一旦限速或改包会直接断网。

## 游戏 / UDP 场景的能力边界

| 流量类型 | 能不能限 | 说明 |
|---|---|---|
| 游戏平台下载/更新/登录（TCP，如 Steam、Epic、WeGame 的下载器） | ✅ 准 | 与普通 TCP 一致，实测 89–106% |
| 游戏联机对战（UDP） | ⚠️ 只能"延迟/丢包" | UDP 没有流控，限速表现为**包被排队或丢弃**，不会让对端降速；限得越低越像断流 |
| 反作弊 / 驱动直接发包 | ❌ 多半限不住 | 这类包在内核态发出，`GetExtendedTcpTable` 查不到 PID → 控制台/界面里"无进程映射"计数会涨 |
| 本机回环（127.0.0.1） | ❌ 抓不到 | Windows 回环流量在 WinDivert NETWORK 层通常不可见；`APP_LIMITER_LOOPBACK=1` 只对少数场景有效（本机实测 UDP 回环 0 命中） |

**建议用法**：游戏场景只给**更新/下载进程**设限速（`steam.exe`、`EpicGamesLauncher.exe`、`*updater*.exe`、
各类平台的下载器），别给游戏主进程设限速——对战流量被排队/丢弃的代价是延迟抖动与掉线。

**自助判断有没有限住**：看界面顶栏四个计数——`限速命中` 在涨说明规则生效；
`无进程映射` 在涨说明该进程的包查不到 PID（反作弊/内核态），此时限速不会作用到它。

## 如何验证限速生效

```powershell
curl -o NUL --max-time 10 -w "speed=%{speed_download}B/s`n" https://mirrors.aliyun.com/ubuntu/ls-lR.gz
# 不开限速器先测基线；再开限速器对 curl 的 PID 限速，重测，速度应≈限速值
```

## 单元测试

```powershell
cargo test   # 令牌桶（含离线速率精度）/延迟队列/IP+TCP 解析/段去重/规则表/保护名单/配置/限速解析 共 33 项
```

> exe 内嵌了管理员 manifest，测试可执行文件同样要求提升权限。可在管理员终端里直接跑；
> 或用普通终端执行 `set __COMPAT_LAYER=RunAsInvoker`（仅当前会话生效，不弹 UAC）。
