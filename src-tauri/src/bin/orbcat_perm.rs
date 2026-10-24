//! `orbcat-perm` —— **外部 AI 代批 CLI**（只给用户侧的外部 AI 用）。
//!
//! # 用途
//!
//! 用户在自己的 shell 里跑 Codex / Claude Code 等**外部 AI**，它们要能**代用户**
//! 处理 orbcat 的权限申请（批准 / 拒绝 / 查看 pending）——
//! 而 orbcat **内部**的 AI（pi 内核 / 自研内核）**绝对不许**碰这个能力。
//!
//! # 命令面（**故意只有这三个**）
//!
//! | 命令 | 作用 |
//! |---|---|
//! | `orbcat-perm list` | 列出当前 pending（id / kind / 命令 / 参数摘要 / 会话 id） |
//! | `orbcat-perm approve <id>` | **代批一次**（一次性！不写任何永久授权） |
//! | `orbcat-perm deny <id>` | 拒绝（可选带说明） |
//!
//! ⚠️ **没有** `approve-always` / `remember` / `set-grants` 之类会**扩大授权**的子命令
//!    —— 用户原话：「更不能允许用这个 cli 通过用户没同意的命令」。
//!
//! # 凭证
//!
//! - **token 在环境变量** `ORBCAT_PERM_TOKEN` 里 —— **只存在于本进程的环境**；
//! - orbcat 侧短时效（5 分钟）+ **用一次即轮换**；
//! - 端点地址在 `<状态目录>\perm-port`：
//!   - Windows：`%LOCALAPPDATA%\orbcat`
//!   - 可用 `ORBCAT_PERM_STATE_DIR` 覆盖（测试/便携场景）。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// 状态目录（与宿主侧 `perm_cli::cli_state_dir()` 同口径）。
fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ORBCAT_PERM_STATE_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("orbcat"))
            .unwrap_or_else(|| std::env::temp_dir().join("orbcat-perm-cli"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(|p| PathBuf::from(p).join("orbcat"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/orbcat"))
            })
            .unwrap_or_else(|| std::env::temp_dir().join("orbcat-perm-cli"))
    }
}

fn die(msg: &str) -> ! {
    eprintln!("orbcat-perm: {msg}");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(op) = args.first().map(|s| s.as_str()) else {
        eprintln!("用法: orbcat-perm <list|approve <id>|deny <id>>");
        std::process::exit(2);
    };

    // ⚠️ 命令面白名单：**只有这三个**（不许有扩大授权的子命令）
    let (op, arg) = match op {
        "list" => ("list", ""),
        "approve" | "deny" => {
            let Some(id) = args.get(1) else {
                die(&format!("{op} 需要一个 pending id"));
            };
            (op, id.as_str())
        }
        other => {
            eprintln!(
                "orbcat-perm: 没有这个命令「{other}」\n\
                 可用: list | approve <id> | deny <id>"
            );
            std::process::exit(2);
        }
    };

    let token = std::env::var("ORBCAT_PERM_TOKEN")
        .unwrap_or_else(|_| die("缺少环境变量 ORBCAT_PERM_TOKEN（凭证只应出现在本进程环境里）"));

    let port_file = state_dir().join("perm-port");
    let port: u16 = std::fs::read_to_string(&port_file)
        .unwrap_or_else(|e| die(&format!("读端口文件失败（{}）：{e}", port_file.display())))
        .trim()
        .parse()
        .unwrap_or_else(|_| die("端口文件内容不合法"));

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| die(&format!("连 orbcat 外部通道失败（127.0.0.1:{port}）：{e}")));
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();

    // 手写最小 JSON（只三个字符串字段，避免给 bin 拖 serde 依赖）
    let req = format!(
        "{{\"token\":{},\"op\":{},\"arg\":{}}}",
        json_str(&token),
        json_str(op),
        json_str(arg)
    );
    stream
        .write_all(format!("{req}\n").as_bytes())
        .unwrap_or_else(|e| die(&format!("写请求失败：{e}")));

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .unwrap_or_else(|e| die(&format!("读响应失败：{e}")));
    let resp = line.trim().to_string();
    println!("{resp}");
    if resp.contains("\"ok\":false") {
        std::process::exit(1);
    }
}

/// 极简 JSON 字符串转义。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
