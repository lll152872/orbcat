//! 命令执行器 —— 通过 PowerShell 跑命令并回收输出。
//!
//! ## 解释器选择（用户拍板）
//!
//! 优先 `pwsh`（**PowerShell 7+**，本机已装）；只有 PATH 上找不到才回退
//! `powershell`（Windows PowerShell 5.1）并打一次 warning。
//! 之所以要 7：解析更现代、AI 写命令更稳（5.1 的老行为容易让模型踩坑）。
//!
//! ## 两条硬规矩
//!
//! 1. **轮询**：起子进程后持续等它结束（读干 stdout/stderr + `wait`），
//!    不设捕获上限的进程会因管道写满而死锁。
//! 2. **超时杀整棵进程树**：到点先 `taskkill /F /T` 连子进程一起收，
//!    再 `start_kill` 收尸。只杀直接子进程会漏掉 `npm`→`node` 这类孙子进程。
//!
//! 调用形如：
//! ```text
//! pwsh -NoProfile -NonInteractive -Command "<cmd>"
//! ```
//! 不开 profile（避免用户 rc 干扰）、不开交互（避免卡在提示符）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 单次命令输出上限（stdout / stderr 各自）
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// 命令执行期间的心跳回调：`(已运行秒数, 输出尾巴)`。
///
/// 为什么需要（2026-09-25 用户报「界面卡在 run_com 步骤不动」后加）：
/// 长命令（编译/安装包/测试，最长 600s）执行期间**一个事件都没有**，
/// 用户看到的就是界面冻住 —— 其实命令一直在跑。每 2 秒推一次
/// 「已运行 N 秒 + 最新输出」，界面就有了呼吸感。
pub type TickFn = Arc<dyn Fn(u64, String) + Send + Sync>;

/// 命令执行期的**取消探针**：返回 true 表示用户已请求停止本轮。
///
/// 为什么需要（2026-09-26 用户报「点停止没用」）：
/// 在这之前，cancel 只在「每轮开始」「工具全跑完」两个检查点被读；命令执行
/// 期间（最长 600s）**没有任何取消通道** —— 用户点了停止，得等命令自己跑完
/// （或撞满 600s 超时）才有反应，观感就是"停止按钮坏了"。
/// 这个探针让 `run_powershell_tick` 的 select 多一个分支：一旦置位立即
/// `kill_tree` 收掉整棵进程树。
///
/// 用 `Arc<dyn Fn() -> bool>` 而不是 `&AtomicBool`：命令执行是跨 await 的，
/// 而 agent loop 那边要能从 `CHAT_CANCEL`（一个 `'static` 全局）里读 —— 闭包
/// 捕获引用活不够长，所以包一层 `Arc` 传值进来。
pub type CancelFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// 命令执行结果。
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// 被用户「停止」掐掉的（与超时区分：超时是到点自己断，取消是用户主动断）。
    /// `true` 时 `exit_code` 恒为 None（进程树被强杀，退出码没有意义）。
    pub cancelled: bool,
    pub interpreter: String,
}

impl CmdOutput {
    /// 命令是否成功（未超时、未取消且退出码 0）。
    #[allow(dead_code)]
    pub fn success(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit_code == Some(0)
    }
}

/// 解析要用的 PowerShell 解释器。返回 `(可执行文件路径, 是否 pwsh 7+)`。
///
/// 不做版本探测（起子进程问版本太慢，每条命令都付一次代价没意义）——
/// 只要 `pwsh` 在 PATH 上就认为它是 7+（`pwsh` 这个名字本身就是 PS7 的标记；
/// 5.1 的可执行名是 `powershell`）。
///
/// ⚠️ **返回的是全路径，不是裸名**（2026-10 修）：
/// 裸 API `CreateProcessW` **不做 PATH 搜索**，传 `"pwsh"` 会直接
/// `GetLastError=2（找不到文件）`。而 `std::process::Command::new("pwsh")`
/// 会自动解析 PATH —— 这个差异让"隐形桌面跑命令"一上手就失败，
/// 却看不出是路径问题（普通路径明明好好的）。
pub fn resolve_interpreter() -> (String, bool) {
    if let Some(p) = find_in_path("pwsh.exe").or_else(|| find_in_path("pwsh")) {
        (p.to_string_lossy().to_string(), true)
    } else if let Some(p) = find_in_path("powershell.exe").or_else(|| find_in_path("powershell")) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "[orbcat] ⚠️ PATH 上找不到 pwsh（PowerShell 7+），\
                 回退 Windows PowerShell 5.1。建议安装 PS7 以获得更稳的解析。"
            );
        });
        (p.to_string_lossy().to_string(), false)
    } else {
        // 连 powershell 都找不到：只能交回裸名，让错误信息里带上原始名字
        ("powershell".to_string(), false)
    }
}

/// 在 PATH 里找一个可执行文件（不依赖外部 `which`）。
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 在**隐形桌面**上跑一条命令（用户看不见、不抢焦点）。
///
/// 与 [`run_powershell_tick`] 的关系：
///
/// | | 普通路径 | 本函数 |
/// |---|---|---|
/// | 子进程跑在哪 | 用户桌面（`CREATE_NO_WINDOW`，无控制台窗口） | **隐形桌面**（`CreateDesktop`） |
/// | 会不会闪 GUI 窗口 | 会（应用自己弹的窗口就弹在用户屏幕上） | **不会** |
/// | 会不会抢焦点 | 会 | **不会** |
/// | 心跳 / 取消 | 有（tick + cancel） | **暂无**（走阻塞线程，取消是后续项） |
///
/// 用 `spawn_blocking` 包住 `win32desk::run_blocking`：那条路是阻塞式
/// `CreateProcessW + ReadFile`，直接在 async 上下文里跑会占死一个 worker 线程。
///
/// ⚠️ 拿不到画面/输入的边界见 `win32desk` 模块头：**没有合成键鼠**，
/// 需要点界面的程序在这个模式下驱动不了。
#[cfg(windows)]
pub async fn run_on_desktop(
    cmd: &str,
    cwd: &Path,
    timeout_secs: u64,
) -> Result<CmdOutput, String> {
    let (exe, _is7) = resolve_interpreter();
    // 注意：`cmd` 原样传进去。**不要**在这里拼 `-Command "…"` ——
    // `win32desk::run_blocking` 会把它落成临时 `.ps1` 再 `-File` 执行，
    // 正是为了绕开 `CreateProcessW` 吃嵌套双引号那个坑（见那个模块的注释）。
    let cwd_s = cwd.to_string_lossy().to_string();
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 3600));

    let started = std::time::Instant::now();
    // `exe` 要 move 进闭包，但返回的 `interpreter` 字段还要用它的名字 → 先留一份
    let exe_label = exe.clone();
    let cmd_owned = cmd.to_string();
    let out = tokio::task::spawn_blocking(move || {
        crate::win32desk::run_blocking(&exe, &cmd_owned, Some(&cwd_s), timeout)
    })
    .await
    .map_err(|e| format!("后台桌面任务 panic：{e}"))??;

    eprintln!(
        "[orbcat] 后台桌面命令完成 pid={} 用时 {:?} 超时={}",
        out.pid,
        started.elapsed(),
        out.timed_out
    );

    Ok(CmdOutput {
        stdout: out.stdout,
        stderr: out.stderr,
        exit_code: out.exit_code,
        timed_out: out.timed_out,
        cancelled: false,
        interpreter: format!("{exe_label}（隐形桌面 pid {}）", out.pid),
    })
}

/// 非 Windows 平台的同名占位 —— macOS 没有「隐形桌面」这个概念
/// （`CreateDesktopW` 是 Win32 独有）。
///
/// 存在的唯一理由是**让 `tools.rs` 的分支能编译**：那里的
/// `use_bg_desk` 在非 Windows 上恒为 `false`，但 Rust 的类型检查不看运行期
/// 常量，`if use_bg_desk { … run_on_desktop(…) }` 里的调用仍要被解析。
///
/// 真的走到这里说明逻辑出错了（开关在非 Windows 上不可能为 true），
/// 所以返回错误而不是悄悄降级成普通执行 —— 那样会掩盖 bug。
#[cfg(not(windows))]
pub async fn run_on_desktop(
    _cmd: &str,
    _cwd: &Path,
    _timeout_secs: u64,
) -> Result<CmdOutput, String> {
    Err("隐形桌面后台执行仅支持 Windows（macOS 无此机制）".to_string())
}

/// 等待结果：子进程结束 / 超时 / 用户取消
enum Waited {
    Done(std::io::Result<std::process::ExitStatus>),
    Timeout,
    Cancelled,
}

/// 跑一条**已展开成 argv 的**命令（不经任何 shell 解释器）。
///
/// ## 为什么需要它（而不是把 argv 拼成字符串交给 PowerShell）
/// 拼成字符串再交给 pwsh = 把参数边界**重新交还给 shell 解析**。
/// 那样 `permPreset` / 项目脚本里精心保持的"一个参数就是一个参数"的保证
/// 会当场失效 —— 用户传 `a; rm -rf /` 就真的会被当两条命令跑。
///
/// 这里用 `Command::new(argv[0]).args(argv[1..])` 直接起进程：
/// **argv 里每个元素都是一个字面参数**，`;` `|` `$()` 全是普通字符。
///
/// 与 [`run_powershell_tick`] 的关系：
/// - 心跳 / 取消 / 超时杀进程树 / 输出留头留尾 —— **全部一致**（同一套实现）
/// - 差别只在"怎么起进程"：这里不做 UTF-8 包裹（那是 PowerShell 的坑，
///   非 PowerShell 程序不需要，乱加反而会污染 argv）
pub async fn run_argv(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    tick: Option<TickFn>,
    cancel: Option<CancelFn>,
) -> Result<CmdOutput, String> {
    let Some((exe, rest)) = argv.split_first() else {
        return Err("argv 为空".into());
    };

    let mut c = Command::new(exe);
    c.args(rest)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = c
        .spawn()
        .map_err(|e| format!("启动 {exe} 失败: {e}（argv={argv:?}）"))?;
    let pid = child.id();

    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_task = tokio::spawn(read_into(stdout, out_buf.clone()));
    let mut err_task = tokio::spawn(read_into(stderr, err_buf.clone()));

    let timeout = Duration::from_secs(timeout_secs.clamp(1, 3600));
    let deadline = tokio::time::Instant::now() + timeout;
    let started = std::time::Instant::now();

    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;

    let exe_label = exe.clone();

    let waited = loop {
        let cancel_probe = async {
            match cancel.as_ref() {
                Some(f) => loop {
                    if f() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                },
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            r = child.wait() => break Waited::Done(r),
            _ = interval.tick(), if tick.is_some() => {
                if let Some(cb) = tick.as_ref() {
                    cb(started.elapsed().as_secs(), tail_of(&out_buf, &err_buf));
                }
            }
            _ = tokio::time::sleep_until(deadline) => break Waited::Timeout,
            _ = cancel_probe => break Waited::Cancelled,
        }
    };

    match waited {
        Waited::Done(Ok(status)) => {
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
            let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_bytes),
                stderr: truncate(&err_bytes),
                exit_code: status.code(),
                timed_out: false,
                cancelled: false,
                interpreter: exe_label,
            })
        }
        Waited::Done(Err(e)) => {
            let _ = child.start_kill();
            Err(format!("等待 {exe_label} 结束失败: {e}"))
        }
        Waited::Timeout => {
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
            let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_bytes),
                stderr: truncate(&err_bytes),
                exit_code: None,
                timed_out: true,
                cancelled: false,
                interpreter: exe_label,
            })
        }
        Waited::Cancelled => {
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
            let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_bytes),
                stderr: truncate(&err_bytes),
                exit_code: None,
                timed_out: false,
                cancelled: true,
                interpreter: exe_label,
            })
        }
    }
}

/// 跑一条 PowerShell 命令（无心跳、无取消通道）。
///
/// `timeout_secs` 会被夹到 `1..=600`。超时**不算失败** —— 返回
/// `timed_out = true` 的结果，由上层决定怎么告诉模型。
///
/// ⚠️ **仅供测试**（2026-09-30 标注）：生产路径一律走
/// [`run_powershell_tick`]，因为它要挂心跳（长命令每 2s 推一次输出尾巴）
/// 与取消探针（用户点「停止」要能掐断整棵进程树）。这个无参版本留着是为了
/// 让单测不必构造两个 `None` 回调 —— 它不是"忘了接线的回退壳"。
#[cfg(test)]
pub async fn run_powershell(cmd: &str, cwd: &Path, timeout_secs: u64) -> Result<CmdOutput, String> {
    run_powershell_tick(cmd, cwd, timeout_secs, None, None).await
}

/// 跑一条 PowerShell 命令，执行期间每 2 秒回调一次心跳（见 [`TickFn`]），
/// 并响应取消探针（见 [`CancelFn`]）。
///
/// `tick = None` 时与 [`run_powershell`] 完全等价（普通工具不需要心跳）。
/// `cancel = None` 时没有取消通道（测试与不关心中断的场景）。
pub async fn run_powershell_tick(
    cmd: &str,
    cwd: &Path,
    timeout_secs: u64,
    tick: Option<TickFn>,
    cancel: Option<CancelFn>,
) -> Result<CmdOutput, String> {
    let (exe, _is7) = resolve_interpreter();
    let wrapped = wrap_utf8(cmd);

    let mut c = Command::new(&exe);
    c.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(&wrapped)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 兜底：无论怎么退出，别留孤儿直接子进程（进程树另由 kill_tree 收）
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        // tokio::process::Command 在 Windows 上自带 creation_flags（无需 CommandExt trait）
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        c.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = c.spawn().map_err(|e| format!("启动 {exe} 失败: {e}"))?;
    let pid = child.id();

    // 先把两条管道拿走，交给独立任务**边读边写**共享缓冲 ——
    // 不读的话子进程输出一多就写满管道死锁；写共享缓冲则是心跳能拿到输出尾巴的前提。
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_task = tokio::spawn(read_into(stdout, out_buf.clone()));
    let mut err_task = tokio::spawn(read_into(stderr, err_buf.clone()));

    // 上限 3600s（1 小时）：构建/装包这类长任务要放得下（用户实报「build 4-5 分钟
    // 不输出很正常」）。这里的 clamp 是**最后一道**，`run_command` 那边还有一次。
    let timeout = Duration::from_secs(timeout_secs.clamp(1, 3600));
    let deadline = tokio::time::Instant::now() + timeout;
    let started = std::time::Instant::now();

    // 等子进程结束，同时按 2s 间隔推心跳、响应取消；到超时线就退出去杀进程树。
    // `child.wait()` 是 cancel-safe 的（tokio 文档保证），select 丢弃它无副作用。
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一次 tick 立即触发 —— 吞掉，心跳从"跑了 2 秒"开始
    interval.tick().await;

    /// 等待结果：子进程结束 / 超时 / 用户取消
    enum Waited {
        Done(std::io::Result<std::process::ExitStatus>),
        Timeout,
        Cancelled,
    }

    let waited = loop {
        // 取消探针每 2s 轮询一次即可 —— 与心跳同频，用户感知上就是"点下去马上停"。
        let cancel_probe = async {
            match cancel.as_ref() {
                Some(f) => {
                    loop {
                        if f() {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
                // 没有取消探针 → 永不触发的 future
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            r = child.wait() => break Waited::Done(r),
            _ = interval.tick(), if tick.is_some() => {
                if let Some(cb) = tick.as_ref() {
                    let tail = tail_of(&out_buf, &err_buf);
                    cb(started.elapsed().as_secs(), tail);
                }
            }
            _ = tokio::time::sleep_until(deadline) => break Waited::Timeout,
            _ = cancel_probe => break Waited::Cancelled,
        }
    };

    match waited {
        Waited::Done(Ok(status)) => {
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
            let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_bytes),
                stderr: truncate(&err_bytes),
                exit_code: status.code(),
                timed_out: false,
                cancelled: false,
                interpreter: exe,
            })
        }
        Waited::Done(Err(e)) => {
            let _ = child.start_kill();
            Err(format!("等待 {exe} 结束失败: {e}"))
        }
        Waited::Timeout => {
            // ---- 超时：杀整棵进程树 ----
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // 进程死了 → 管道 EOF → 读取任务自然收尾（有界等，防残留孙子进程占着句柄）
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            Ok(CmdOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: true,
                cancelled: false,
                interpreter: exe,
            })
        }
        Waited::Cancelled => {
            // ---- 用户点「停止」：与超时同样杀整棵进程树，但**已经收到的输出保留** ----
            // 保留输出很重要：用户点了停止，但"命令跑到哪一步"本身是有效信息。
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            drain_reader(&mut out_task).await;
            drain_reader(&mut err_task).await;
            let out_bytes = out_buf.lock().map(|g| g.clone()).unwrap_or_default();
            let err_bytes = err_buf.lock().map(|g| g.clone()).unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_bytes),
                stderr: truncate(&err_bytes),
                exit_code: None,
                timed_out: false,
                cancelled: true,
                interpreter: exe,
            })
        }
    }
}

/// 边读边追加进共享缓冲（有上限，防止失控命令把内存吃光）。
async fn read_into(pipe: Option<impl AsyncReadExt + Unpin>, buf: Arc<Mutex<Vec<u8>>>) {
    const CAP: usize = 256 * 1024;
    if let Some(mut p) = pipe {
        let mut chunk = [0u8; 8192];
        loop {
            match p.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut g) = buf.lock() {
                        if g.len() < CAP {
                            g.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        }
    }
}

/// 有界地等读任务收尾 —— **绝不无条件 `join`**。
///
/// ⚠️ 这是 2026-09-26 用户实报「任务卡死 / 心跳永远停在 4s」的根因：
///   命令若是 `Start-Process` 这类**派生常驻子进程**的，孙子进程会继承
///   stdout/stderr 管道句柄。pwsh 自己已经退出（`child.wait()` 返回），
///   但管道写端还开着 —— 读任务永远等不到 EOF，`out_task.await` 就永久挂住。
///   同时因为心跳循环已随 `child.wait()` 退出，秒数停在最后一拍（"已运行 4s"不动）。
///
/// 做法：给 400ms 宽限让正常输出收尾，超时就以**已读到的缓冲**为准，把读任务掐掉。
/// 输出缓冲本身有 256KB 上限（见 [`read_into`]），就算读任务没被及时掐掉也不吃内存。
async fn drain_reader(h: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_millis(400), &mut *h).await.is_err() {
        h.abort();
    }
}

/// 输出尾巴（给心跳展示）：两条流合并后取最后 200 字符，压成单行。
fn tail_of(out_buf: &Arc<Mutex<Vec<u8>>>, err_buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let mut merged = Vec::new();
    if let Ok(g) = out_buf.lock() {
        merged.extend_from_slice(&g);
    }
    if let Ok(g) = err_buf.lock() {
        merged.extend_from_slice(&g);
    }
    if merged.is_empty() {
        return String::new();
    }
    let s = String::from_utf8_lossy(&merged);
    let tail: String = s.chars().rev().take(200).collect::<String>().chars().rev().collect();
    tail.replace('\n', " ").replace('\r', " ")
}

/// 收掉整棵进程树（含子进程）。失败不致命 —— 后面还有 `start_kill` 兜底。
async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = Command::new("taskkill");
        c.arg("/F").arg("/T").arg("/PID").arg(pid.to_string());
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.creation_flags(CREATE_NO_WINDOW);
        let _ = c.status().await;
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .await;
    }
}

/// 强制把输出编码设成 UTF-8，否则中文输出在重定向时可能是 GBK 乱码。
fn wrap_utf8(cmd: &str) -> String {
    format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {cmd}")
}

/// 截断 + UTF-8 lossy 解码。
///
/// ⚠️ 超限时**留头也留尾**（各一半）：只留头是坑 —— 编译 / 构建 / 测试这类命令的
/// 报错和结论恰恰在**末尾**，只留头等于把真正要看的东西截掉，模型只能看到一堆
/// `Compiling xxx`（2026-09-26 用户提「build 4-5 分钟」时顺带发现）。
fn truncate(buf: &[u8]) -> String {
    if buf.len() <= MAX_OUTPUT_BYTES {
        return String::from_utf8_lossy(buf).to_string();
    }
    let half = MAX_OUTPUT_BYTES / 2;
    let head = &buf[..half];
    let tail = &buf[buf.len() - half..];
    let mut s = String::from_utf8_lossy(head).into_owned();
    s.push_str(&format!(
        "\n… [输出共 {} 字节，中间省略 {} 字节；以下是末尾] …\n",
        buf.len(),
        buf.len() - MAX_OUTPUT_BYTES
    ));
    s.push_str(&String::from_utf8_lossy(tail));
    s
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 真起一个 pwsh/powershell 跑一条无害命令。
    /// 环境不允许起进程时（受限沙箱）直接跳过，避免 CI 误红。
    #[tokio::test]
    async fn runs_trivial_command() {
        let dir = std::env::temp_dir();
        let out = match run_powershell("Write-Output 'hello_from_float'", &dir, 30).await {
            Ok(o) => o,
            Err(_) => return, // 起不了进程 → 跳过
        };
        assert_eq!(
            out.exit_code,
            Some(0),
            "退出码异常: {:?}, stderr={}",
            out.exit_code,
            out.stderr
        );
        assert!(
            out.stdout.contains("hello_from_float"),
            "stdout 未包含预期文本: {:?}",
            out.stdout
        );
        assert!(!out.timed_out);
    }

    /// 超时必须真的把长命令掐掉，且标记 `timed_out`。
    #[tokio::test]
    async fn timeout_kills_long_command() {
        let dir = std::env::temp_dir();
        let out = match run_powershell("Start-Sleep -Seconds 30", &dir, 1).await {
            Ok(o) => o,
            Err(_) => return,
        };
        assert!(out.timed_out, "应在 1 秒后超时");
        assert!(!out.cancelled, "超时不等于取消");
    }

    /// 取消探针置位后必须**立刻**杀掉长命令（而不是等它自己跑完）。
    ///
    /// 这是 2026-09-26 用户报「点停止没用」的回归测试：命令跑 30s，
    /// 探针 300ms 后就置位，整个调用必须在 ~2s 内返回且 `cancelled = true`。
    #[tokio::test]
    async fn cancel_kills_long_command_promptly() {
        let dir = std::env::temp_dir();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let cancel: CancelFn = std::sync::Arc::new(move || {
            flag.load(std::sync::atomic::Ordering::Relaxed)
        });

        let started = std::time::Instant::now();
        let out = match run_powershell_tick(
            "Start-Sleep -Seconds 30",
            &dir,
            600,
            None,
            Some(cancel),
        )
        .await
        {
            Ok(o) => o,
            Err(_) => return, // 起不了进程 → 跳过
        };
        assert!(out.cancelled, "应被标记为取消");
        assert!(!out.timed_out, "取消不是超时");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消要立刻生效（实际耗时 {:?}）",
            started.elapsed()
        );
    }

    /// 回归：命令**派生出常驻子进程**时，`run_powershell_tick` 必须按时返回。
    ///
    /// 2026-09-26 用户实报「任务卡死」+「心跳永远停在 4s」：
    ///   `Start-Process` 起的孙子进程继承了 stdout/stderr 管道句柄，pwsh 自己已退出、
    ///   管道写端却还开着 → 读任务永远等不到 EOF → 无条件 `join` 读任务把整个工具调用
    ///   挂死；同时心跳循环随 `child.wait()` 退出，秒数就冻在最后一拍。
    ///
    /// 断言：孙进程还活着的时候调用就必须返回（< 5s），且父进程自己的输出没丢。
    #[tokio::test]
    async fn detached_child_does_not_hang_tool_call() {
        let dir = std::env::temp_dir();
        // 孙进程自己活 8 秒后退出（测试不收它，但也不会永久泄漏）
        let cmd = "Write-Output 'BEFORE'; \
                   Start-Process -FilePath 'powershell.exe' \
                     -ArgumentList '-NoProfile','-NonInteractive','-c','Start-Sleep 8' \
                     -WindowStyle Hidden; \
                   Write-Output 'AFTER'";
        let started = std::time::Instant::now();
        let out = match run_powershell_tick(cmd, &dir, 600, None, None).await {
            Ok(o) => o,
            Err(_) => return, // 起不了进程 → 跳过
        };
        let dt = started.elapsed();
        assert!(
            dt < Duration::from_secs(5),
            "派生常驻子进程时不该挂死（实际等了 {dt:?}）"
        );
        assert!(
            out.stdout.contains("BEFORE") && out.stdout.contains("AFTER"),
            "父进程自己的输出不应丢: {:?}",
            out.stdout
        );
    }

    /// 超长输出**留头也留尾**。
    ///
    /// 2026-09-26 用户提「build 4-5 分钟」时顺带发现：以前只留头，
    /// 而编译/构建的报错与结论都在**末尾** —— 等于把要看的东西截掉。
    #[test]
    fn truncate_keeps_head_and_tail() {
        let mut big = Vec::new();
        big.extend_from_slice(b"HEAD_MARK ");
        big.extend(std::iter::repeat(b'x').take(MAX_OUTPUT_BYTES * 2));
        big.extend_from_slice(b" TAIL_MARK");
        let s = truncate(&big);
        assert!(s.contains("HEAD_MARK"), "要留头，实际 len={}", s.len());
        assert!(s.contains("TAIL_MARK"), "要留尾（报错在末尾），实际 len={}", s.len());
        assert!(s.contains("中间省略"), "要有省略说明: {}", s.chars().rev().take(60).collect::<String>());
    }

    /// 解释器必须解析成**可用路径**。
    ///
    /// ⚠️ 2026-10 修：以前这里断言 `exe == "pwsh" || exe == "powershell"`（裸名）。
    /// 裸名对 `std::process::Command` 够用（它会搜 PATH），但对
    /// `CreateProcessW` **不够** —— 那个 API 不搜 PATH，传裸名直接
    /// `GetLastError=2`。隐形桌面这条路走的正是 `CreateProcessW`，
    /// 所以现在断言"是个存在的文件"。
    #[cfg(windows)]
    // 这些用例硬编码了 Windows 路径（`D:\…`）与 Windows 的大小写不敏感语义，
    // 在 macOS/Linux 上 `D:\` 只是一个相对文件名，断言必然失败 —— 不是产品 bug。
    // 2026-10-10 由 macos-14 runner 抓出。
    #[test]
    fn interpreter_is_resolvable() {
        let (exe, _) = resolve_interpreter();
        assert!(
            std::path::Path::new(&exe).is_file(),
            "解释器必须是可用的绝对路径（裸名会让 CreateProcessW 报 error 2）: {exe}"
        );
        let lower = exe.to_ascii_lowercase();
        assert!(
            lower.ends_with("pwsh.exe") || lower.ends_with("powershell.exe"),
            "未知解释器: {exe}"
        );
    }
}
