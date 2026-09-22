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
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 单次命令输出上限（stdout / stderr 各自）
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// 命令执行结果。
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub interpreter: String,
}

impl CmdOutput {
    /// 命令是否成功（未超时且退出码 0）。
    #[allow(dead_code)]
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// 解析要用的 PowerShell 解释器。返回 `(可执行名, 是否 pwsh 7+)`。
///
/// 不做版本探测（起子进程问版本太慢，每条命令都付一次代价没意义）——
/// 只要 `pwsh` 在 PATH 上就认为它是 7+（`pwsh` 这个名字本身就是 PS7 的标记；
/// 5.1 的可执行名是 `powershell`）。
pub fn resolve_interpreter() -> (String, bool) {
    if find_in_path("pwsh.exe").is_some() || find_in_path("pwsh").is_some() {
        ("pwsh".to_string(), true)
    } else {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "[float-agent] ⚠️ PATH 上找不到 pwsh（PowerShell 7+），\
                 回退 Windows PowerShell 5.1。建议安装 PS7 以获得更稳的解析。"
            );
        });
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

/// 跑一条 PowerShell 命令。
///
/// `timeout_secs` 会被夹到 `1..=600`。超时**不算失败** —— 返回
/// `timed_out = true` 的结果，由上层决定怎么告诉模型。
pub async fn run_powershell(cmd: &str, cwd: &Path, timeout_secs: u64) -> Result<CmdOutput, String> {
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

    // 先把两条管道拿走，交给独立任务读干 —— 不读的话子进程输出一多就写满管道死锁。
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(read_all(stdout));
    let err_task = tokio::spawn(read_all(stderr));

    let timeout = Duration::from_secs(timeout_secs.clamp(1, 600));

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let out_buf = out_task.await.unwrap_or_default();
            let err_buf = err_task.await.unwrap_or_default();
            Ok(CmdOutput {
                stdout: truncate(&out_buf),
                stderr: truncate(&err_buf),
                exit_code: status.code(),
                timed_out: false,
                interpreter: exe,
            })
        }
        Ok(Err(e)) => {
            let _ = child.start_kill();
            Err(format!("等待 {exe} 结束失败: {e}"))
        }
        Err(_) => {
            // ---- 超时：杀整棵进程树 ----
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // 进程死了 → 管道 EOF → 读取任务自然收尾
            let _ = out_task.await;
            let _ = err_task.await;
            Ok(CmdOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: true,
                interpreter: exe,
            })
        }
    }
}

/// 读干一个可选管道。
async fn read_all(pipe: Option<impl AsyncReadExt + Unpin>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf).await;
    }
    buf
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
fn truncate(buf: &[u8]) -> String {
    let over = buf.len() > MAX_OUTPUT_BYTES;
    let slice = if over { &buf[..MAX_OUTPUT_BYTES] } else { buf };
    let mut s = String::from_utf8_lossy(slice).to_string();
    if over {
        s.push_str("\n… [输出超过 64KB，已截断]");
    }
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
    }

    #[test]
    fn interpreter_is_resolvable() {
        let (exe, _) = resolve_interpreter();
        assert!(exe == "pwsh" || exe == "powershell", "未知解释器: {exe}");
    }
}
