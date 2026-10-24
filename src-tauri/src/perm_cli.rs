//! 内置 CLI「外部代批」的**隔离**与**独立通道**。
//!
//! # 用户的要求（2026-10-24）
//!
//! > 「这个 cli 是固定的访问这个直接停，只要发现访问这个直接停」
//! > 「那就是检测到这个 cli 被调用直接停，就是说检测到 xxx-cli.exe 直接停」
//!
//! ⇒ 内部 AI（pi 内核 / 自研内核）**碰这个 CLI 一律直接停**；
//! ⇒ 外部 AI（用户自己 shell 里的 Codex / Claude Code）**照常可用**。
//!
//! # 两层边界（**缺一不可**，不许只做一层）
//!
//! | 层 | 挡什么 | 挡不住什么 |
//! |---|---|---|
//! | **L1 路径阻断**（本模块 [`scan_command_for_cli`] / [`block_if_cli`]） | 命令/工具参数里**出现** CLI 的 exe 名或路径 ⇒ 直接拒、不执行、不弹卡、记审计 | **运行时拼出来的路径**（`$p='…'+'.exe'; & $p`）—— 词法层看不见 |
//! | **L2 通道鉴权**（本模块 [`verify_token`] / [`handle_op`]） | 任何**没带正确凭证**的请求 ⇒ 一律拒 | —— |
//!
//! ⇒ **L1 挡「明面调用」，L2 挡「绕开 L1 之后的实际能力」**。
//! ⇒ L1 失效时（运行时拼路径），内部 AI 仍然**调不到「批准权限」这个能力**
//!    —— 因为那个能力只在 L2 的独立通道里，而通道要凭证。
//!
//! # 凭证口径与**文件格式定稿**（2026-10-24 收尾）
//!
//! ## 文件格式（**定稿，不再改**）
//!
//! 状态目录 = [`cli_state_dir`]：
//!
//! | 平台 | 状态目录 |
//! |---|---|
//! | Windows | `%LOCALAPPDATA%\orbcat` |
//! | 其它 | `$XDG_STATE_HOME/orbcat` 或 `~/.local/state/orbcat` |
//!
//! ⚠️ **状态目录故意不在 `agent-data/` 里** —— `D:\orbcat\agent-data`
//!    一字节都不许动，且真数据目录也不该混进凭证文件。
//!    可用 `ORBCAT_PERM_STATE_DIR` 覆盖（测试 / 便携场景）。
//!
//! ### ① token 文件：`<状态目录>\perm-token`
//!
//! **两行 UTF-8 文本**：
//!
//! ```text
//! <明文 token>          ← 第 1 行：128..256 位小写 hex（random_hex(16)）
//! <过期时间戳>          ← 第 2 行：Unix **毫秒**（u64）
//! ```
//!
//! - 有效期 [`TOKEN_TTL_MS`] = **5 分钟**；
//! - **用一次即轮换**：[`verify_token`] 比对成功后立刻 [`issue_token`]
//!   写回新值 ⇒ 旧 token 立刻作废；
//! - **明文落盘**（不是哈希）：因为要「用一次即轮换」，哈希没法轮换 —
//!   哈希只能验「是不是这一枚」，没法换出下一枚。
//! - ⚠️ 落明文 ⇒ **拿到这个文件就等于拿到当前凭证**。这份暴露面由
//!   「状态目录不在 agent-data 里」+「内部 AI 读文件走网关（被拒）」收口，
//!   但**不是**靠「文件内容保密」——别在注释里写成后者。
//!
//! ### ② 端口文件：`<状态目录>\perm-port`
//!
//! **一行**：十进制端口号（u16，纯 ASCII，无 BOM）。
//!
//! - orbcat 的外部通道监听 `127.0.0.1:0`（**随机端口**），
//!   起服务时 [`write_port`] 写进来；
//! - CLI 读它去连端点（读时 `trim()` ⇒ 有无换行都行）。
//!
//! ## 凭证怎么用
//!
//! - 明文 token 经 **环境变量 `ORBCAT_PERM_TOKEN`** 交给 CLI 进程
//!   （只存在于那一个子进程的环境里）；
//! - orbcat **自己的进程 env 里没有** token ⇒ 内部 AI 起的子进程继承不到；
//! - 外部通道**只认 `approve`（一次性）/ `deny`** ⇒ 绝不扩大授权。
//!
//! ## ✅ 已接上（2026-10-24，用户拍板「接上通道 + 提醒 + 可选删除」）
//!
//! ① **token 自动签发** —— 宿主启动时 [`serve`] 直接签发（用户不用手动开）；
//!    另有**续期线程**每 `TTL/2` 重新签发 ⇒ 文件里的 token 不会过期。
//! ② **`serve()` 在宿主进程起**（`lib.rs` 的 `setup` 里）—— **不是**在 pi 运行时起
//!    （那条是工具桥，内部 AI 够得着）。
//! ③ **链式续期** —— 每次**成功**响应都带**新 token**（旧的作废）⇒ 外部 AI
//!    每次调用都拿到新的 ⇒ **不新增任何子命令**（命令面仍是三条）。
//! ④ **代批提醒** —— 决定被采纳时发 [`EVENT_DECISION`]（`perm-cli-decision`）事件
//!    给前端，approve 与 deny **都发**。
//! ⑤ **状态查询** —— [`status_at`] / [`status_current`]：CLI 文件在不在、通道起没起。
//!
//! ## ⚠️ 残余风险（**交付说明必须照实写**）
//!
//! 内部 AI 与外部 AI **同用户、同完整性级别 ⇒ 共享同一安全主体** ⇒ 内部 AI
//! **理论上**也能读到 token 文件并自己直调端点（层 1 只挡「直接调用 CLI」）。
//! ⇒ 本版按用户拍板**不做内核级隔离**，用「**每次代批都有界面提醒 + CLI 可随时删除**」兜底。

// ⚠️ 曾经这里有 `#![allow(dead_code)]`（层 2 还没接线时加的）——
//    现在通道**已接上**（宿主 setup 调 serve）⇒ 那行整体放行已收窄为
//    下面几个函数的 `#[cfg(test)]`（它们只被用例用，非测试构建里没人调）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// CLI 可执行文件名（不含扩展名）。
///
/// ⚠️ 部署态 = `<包根>\bin\orbcat-perm.exe`；绝对路径随安装位置变
/// ⇒ 判定**不硬编码绝对路径**，只看文件名（+ 可选地看是否在 `<包根>\bin\` 下）。
pub const CLI_STEM: &str = "orbcat-perm";

/// CLI 可执行名的合法扩展名。
const CLI_EXTS: &[&str] = &["exe", "cmd", "bat", "ps1", "com"];

/// 命中的规则。
#[derive(Debug, Clone, PartialEq)]
pub struct CliHit {
    /// `exe-name` = 文件名命中 · `short-name` = 8.3 短名命中 · `bin-dir` = 位于 `<包根>\bin\`
    pub rule: &'static str,
    /// 命中的那段文本（原样，便于写进审计）
    pub evidence: String,
}

// ---------------------------------------------------------------------------
// L1 · 路径阻断
// ---------------------------------------------------------------------------

/// 这个 token 的文件名是不是 CLI（大小写不敏感、忽略已知扩展名）。
pub fn is_cli_basename(token: &str) -> bool {
    let t = token.trim().trim_matches(|c| c == '"' || c == '\'');
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    let stem = match base.rsplit_once('.') {
        Some((b, ext)) if CLI_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => b,
        _ => base,
    };
    stem.eq_ignore_ascii_case(CLI_STEM)
}

/// 8.3 短名形态（`ORBCAT~1.EXE`）—— Windows 上短名是这个形状。
pub fn is_cli_short_basename(token: &str) -> bool {
    let t = token.trim().trim_matches(|c| c == '"' || c == '\'');
    let base = t.rsplit(['/', '\\']).next().unwrap_or(t);
    let (stem, ext) = match base.rsplit_once('.') {
        Some((b, e)) => (b, e.to_ascii_lowercase()),
        None => (base, String::new()),
    };
    if !CLI_EXTS.contains(&ext.as_str()) {
        return false;
    }
    // `orbcat~1` / `orbcat~12` 这种形状
    let lower = stem.to_ascii_lowercase();
    match lower.split_once('~') {
        Some((head, num)) => {
            head.eq_ignore_ascii_case(CLI_STEM)
                && !num.is_empty()
                && num.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// 归一化一个路径 token：统一分隔符、去引号、**消解 `..`**、转小写。
///
/// 相对路径按 `cwd` 展开 ⇒ `..\bin\orbcat-perm.exe` 这类写法才落得到同一个绝对位置。
pub fn normalize_path_token(tok: &str, cwd: &Path) -> String {
    let t = tok.trim().trim_matches(|c| c == '"' || c == '\'');
    let mut segs: Vec<String> = Vec::new();
    let rooted = t.starts_with('/') || t.starts_with('\\') || t.contains(':');
    for seg in t.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s.to_ascii_lowercase()),
        }
    }
    let mut out = segs.join("/");
    if rooted {
        out.insert(0, '/');
    } else {
        let base = cwd.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        out = format!("{base}/{out}");
    }
    out
}

/// 这个归一化路径是不是在 `<包根>\bin\` 下面。
pub fn is_under_bin_dir(norm: &str, app_root: &Path) -> bool {
    let root = normalize_path_token(&app_root.to_string_lossy(), app_root);
    let root = root.trim_end_matches('/');
    norm.starts_with(&format!("{root}/bin/"))
}

/// 在一条命令里找 CLI 引用。
///
/// ⚠️ **必须放在已有的词法归一化之后** —— 复用
/// [`crate::command_policy::normalize`] 的分段结果（它已经处理了
/// 复合命令拆分、`bash -c`、转义、`$()`、反引号等），**不另写一套解析**。
///
/// ## 已知挡不住（**边界，必须如实知道**）
///
/// 运行时才拼出来的路径，命令串里没有完整 exe 名 ⇒ 这里看不见：
/// ```text
/// powershell -c "$p='D:\orbcat\bin\orbcat-perm'+'.exe'; & $p"
/// ```
/// ⇒ 这类由 **L2 通道鉴权**兜底（见模块头）。
pub fn scan_command_for_cli(cmd: &str, cwd: &Path, app_root: &Path) -> Option<CliHit> {
    let norm = crate::command_policy::normalize(cmd);

    // ① 归一化后的分段（已做过复合命令拆分与各种转义处理）
    for seg in &norm.segments {
        let text = seg.normalized();
        for tok in text.split_whitespace() {
            if let Some(hit) = hit_token(tok, cwd, app_root) {
                return Some(hit);
            }
        }
    }

    // ② 原始串再扫一遍（归一化可能丢掉某些形态，例如带引号的整串路径）
    for tok in cmd.split_whitespace() {
        if let Some(hit) = hit_token(tok, cwd, app_root) {
            return Some(hit);
        }
    }
    None
}

/// 单个 token 的命中判定。
fn hit_token(tok: &str, cwd: &Path, app_root: &Path) -> Option<CliHit> {
    if is_cli_basename(tok) {
        let norm = normalize_path_token(tok, cwd);
        let rule = if is_under_bin_dir(&norm, app_root) {
            "bin-dir"
        } else {
            "exe-name"
        };
        return Some(CliHit {
            rule,
            evidence: tok.to_string(),
        });
    }
    if is_cli_short_basename(tok) {
        return Some(CliHit {
            rule: "short-name",
            evidence: tok.to_string(),
        });
    }
    None
}

/// 阻断一次工具/命令调用：**fail-closed**（不执行、不弹卡、不询问）。
///
/// 命中 ⇒ 记审计（谁、什么命令、命中哪条规则）并返回 `Err`。
pub fn block_if_cli(
    cmd: &str,
    cwd: &Path,
    app_root: &Path,
    data_dir: &Path,
    tool: &str,
) -> Result<(), String> {
    let Some(hit) = scan_command_for_cli(cmd, cwd, app_root) else {
        return Ok(());
    };
    let msg = format!(
        "[硬阻断] 内部路径引用了外部代批 CLI（规则 {}：{}）—— 该能力只允许用户侧的外部 AI 使用",
        hit.rule, hit.evidence
    );
    // 审计：阻断也要留痕（谁 = 工具名、什么 = 命令、命中哪条规则）
    audit_block(data_dir, tool, cmd, cwd, &hit);
    Err(msg)
}

/// 阻断的审计落点（**两条阻断路径共用**）。
///
/// ⚠️ 为什么要抽出来：`block_if_cli`（命令路径）与 `block_tool_args`（工具网关路径）
///    是**两条独立的入口**。以前只有前者写审计 ⇒ 工具网关那条**静默拦下不留痕**
///    ⇒ 事后没法回答「内部 AI 今天试过几次 CLI」。
fn audit_block(data_dir: &Path, tool: &str, what: &str, cwd: &Path, hit: &CliHit) {
    let rec = crate::command_policy::AuditRecord {
        ts: crate::permission::now_ms(),
        sequence: 0,
        prev_hash: String::new(),
        hash: String::new(),
        event: "cli-path-blocked".to_string(),
        source: format!("perm-cli-guard/{}/{}", tool, hit.rule),
        command: what.to_string(),
        normalized: what.to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        session_id: String::new(),
        risk: crate::command_policy::Risk::High.as_str().to_string(),
        shell: "guard".to_string(),
        exit_code: None,
    };
    if let Err(e) = crate::command_policy::audit_append(data_dir, rec) {
        eprintln!("[orbcat] ⚠️ 写 CLI 阻断审计失败: {e}");
    }
}

// ---------------------------------------------------------------------------
// L2 · 独立通道（凭证 + 只允许一次性决定）
// ---------------------------------------------------------------------------

/// 外部通道**唯一**允许的决定。
///
/// ⚠️ **故意只有 approve / deny 两个一次性决定**。
/// ⚠️ `always` / `full` / `prefix` / `narrow` 一律**不**在这里出现 ——
///    `crate::perm_request::Decision::parse` 会把 `"always"` 映射成 `ApproveFull`
///    （那是**永久授权**，会写 `mcp_grants.json` / `command_grants.json`），
///    而用户原话是「更不能允许用这个 cli 通过用户没同意的命令」
///    ⇒ 外部通道**绝不**复用 `Decision::parse`，只认本函数这张白名单。
pub fn external_decision(op: &str) -> Option<crate::perm_request::Decision> {
    match op {
        "approve" => Some(crate::perm_request::Decision::Approve),
        "deny" => Some(crate::perm_request::Decision::Deny),
        _ => None,
    }
}

/// 外部通道状态目录（**故意不放 `agent-data`** —— 真数据目录一字节都不许动）。
///
/// - Windows：`%LOCALAPPDATA%\orbcat`
/// - 其它：`$XDG_STATE_HOME/orbcat` 或 `~/.local/state/orbcat`
pub fn cli_state_dir() -> PathBuf {
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

/// token 文件路径：`<状态目录>\perm-token`。
///
/// 格式（**定稿**）：**两行** —— 第 1 行 = 明文 token（128..256 位小写 hex），
/// 第 2 行 = 过期时间戳（Unix 毫秒）。详见模块头「凭证口径与文件格式定稿」。
pub fn token_path(state_dir: &Path) -> PathBuf {
    state_dir.join("perm-token")
}

/// 端口文件路径：`<状态目录>\perm-port`。
///
/// 格式（**定稿**）：**一行** = 十进制端口号（u16，纯 ASCII）。
pub fn port_path(state_dir: &Path) -> PathBuf {
    state_dir.join("perm-port")
}

/// token 有效期：**5 分钟**。
pub const TOKEN_TTL_MS: u64 = 5 * 60 * 1000;

/// 签发 token（明文 + 过期时间）。返回明文（设置页显示一次给用户）。
///
/// ⚠️ **签发入口 = 宿主启动时自动签发**（见 [`serve`]）—— 用户不用手动开。
pub fn issue_token(state_dir: &Path, now_ms: u64) -> Result<String, String> {
    let token = random_hex(16);
    let expiry = now_ms.saturating_add(TOKEN_TTL_MS);
    std::fs::create_dir_all(state_dir).map_err(|e| format!("建状态目录失败: {e}"))?;
    write_atomic(&token_path(state_dir), &format!("{token}\n{expiry}"))?;
    Ok(token)
}

/// **原子写**（先写临时文件再 rename）。
///
/// ⚠️ 为什么必须原子：token 文件同时被**三个**角色写 ——
///    ① 启动时自动签发 ② 每次成功请求后的轮换 ③ 续期线程。
///    `fs::write` 是「截断 → 写」⇒ 并发时读者可能读到**空文件或半截**，
///    于是「明明有 token 却被判无效」。
///    rename 是原子的 ⇒ 读者要么看到旧的完整内容，要么看到新的完整内容。
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("写临时文件失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换文件失败: {e}"))?;
    Ok(())
}
#[cfg(test)]

/// 写入 token（**明文 + 过期时间**）到状态目录。
///
/// ⚠️ 名字保留了旧词「hash」，但语义已改：现在落的是**明文 token + 过期时间戳**
///    （短时效 + 用一次即轮换 ⇒ 落明文才轮换得动；哈希没法轮换）。
///    文件格式（定稿）= 两行：`{token}\n{expiry_ms}`。
pub fn write_token_hash(state_dir: &Path, token: &str) -> Result<(), String> {
    let expiry = crate::permission::now_ms().saturating_add(TOKEN_TTL_MS);
    std::fs::create_dir_all(state_dir).map_err(|e| format!("建状态目录失败: {e}"))?;
    write_atomic(&token_path(state_dir), &format!("{token}\n{expiry}"))
}

/// 校验凭证**并轮换**：成功时返回**新的**明文 token，失败返回 `None`。
///
/// ⚠️ 这是「短时效 + 一次性 + 链式续期」三道限制的落点：
/// - 错凭证 / 已过期 / 文件不在 ⇒ `None`（一律拒，**不看 op**）；
/// - 对 ⇒ **用一次即轮换**（立刻签发新 token 写回文件）。
///
/// ## 链式续期（为什么不加 `renew` 子命令）
///
/// 外部 AI 每次成功调用都拿到**新 token**（响应里带回）⇒ 下一次继续用新的
/// ⇒ token 链不断。**不需要**新增任何子命令，命令面仍是
/// `list` / `approve <id>` / `deny <id>` 三条。
pub fn verify_token_rotate(state_dir: &Path, token: &str, now_ms: u64) -> Option<String> {
    let Ok(s) = std::fs::read_to_string(token_path(state_dir)) else {
        return None;
    };
    let mut it = s.lines();
    let (Some(cur), Some(exp)) = (it.next(), it.next()) else {
        return None;
    };
    if cur.trim() != token {
        return None;
    }
    let expiry: u64 = exp.trim().parse().unwrap_or(0);
    if now_ms > expiry {
        return None;
    }
    // 用一次即轮换 —— 签发失败也**不**放行（拿不到新 token ⇒ 链断了 ⇒ 拒）
    issue_token(state_dir, now_ms).ok()
}
#[cfg(test)]

/// 只要一个 bool 的薄包装（不关心新 token 的调用方 / 旧用例用）。
pub fn verify_token(state_dir: &Path, token: &str, now_ms: u64) -> bool {
    verify_token_rotate(state_dir, token, now_ms).is_some()
}

/// 读当前 token 文件里的**过期时间戳**（Unix 毫秒）。文件不在/格式不对 ⇒ `None`。
pub fn token_expiry(state_dir: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(token_path(state_dir)).ok()?;
    s.lines().nth(1)?.trim().parse::<u64>().ok()
}

/// 当前 token 是否还在有效期内（**不消费**它 ⇒ 只是状态查询）。
pub fn token_is_valid(state_dir: &Path, now_ms: u64) -> bool {
    match token_expiry(state_dir) {
        Some(exp) => now_ms <= exp,
        None => false,
    }
}

/// 写端口文件（orbcat 把随机端口写进去，CLI 读它）。
///
/// 格式（**定稿**）：一行十进制端口号。
/// ⚠️ **本轮没有调用点** —— 谁在什么时候起通道（见模块头）还没定。
pub fn write_port(state_dir: &Path, port: u16) -> Result<(), String> {
    std::fs::create_dir_all(state_dir).map_err(|e| format!("建状态目录失败: {e}"))?;
    std::fs::write(port_path(state_dir), port.to_string()).map_err(|e| format!("写端口失败: {e}"))
}

/// 随机 token（无 `rand` 依赖 ⇒ 用 `RandomState` 的随机种子 + 时间 + pid 拼）。
pub fn random_hex(nbytes: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    let mut out = String::new();
    let mut cur = h.finish();
    for _ in 0..nbytes {
        out.push_str(&format!("{cur:08x}"));
        cur = cur.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) ^ (cur >> 7);
    }
    out
}

/// 外部代批决定的**事件名**（前端 `listen('perm-cli-decision', …)`）。
///
/// ⚠️ 命名沿用既有约定（`perm-request` / `perm-resolved` 都是 kebab-case）。
pub const EVENT_DECISION: &str = "perm-cli-decision";

/// CLI 在包根下的子目录（与 `tools/package.mjs` 的 `CLI_SUBDIR` 逐字对应）。
pub const CLI_SUBDIR: &str = "bin";

/// 代批决定事件（= 前端「代批提醒」）。
///
/// ## 字段（**camelCase**，前端按这些名字渲染）
///
/// | 字段 | 类型 | 含义 |
/// |---|---|---|
/// | `pendingId` | string | 被批的那条 pending 的 id |
/// | `kind` | string | pending 的 kind（`command` / `mcp_tool` / …） |
/// | `command` | string | 命令原文；MCP / pi 类则是工具名 / 调用摘要 |
/// | `decision` | string | **只有** `approve` / `deny` |
/// | `cliInstance` | string | CLI 实例标识（`orbcat-perm/<pid>`）——**仅展示用，不是安全边界** |
/// | `at` | number | Unix 毫秒（决定被采纳的那一刻） |
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermCliDecision {
    pub pending_id: String,
    pub kind: String,
    pub command: String,
    pub decision: String,
    pub cli_instance: String,
    pub at: u64,
}

/// 外部代批 CLI 的状态（前端设置页用）。
///
/// ⚠️ `available` = 「CLI 文件在」**且**「通道起着」——
///    用户删掉 `bin\orbcat-perm.exe` 之后，这里必须**如实报不可用**，
///    而不是继续显示「可用」让人以为还能用。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermCliStatus {
    /// CLI 文件在不在（`<包根>/bin/orbcat-perm.exe`）
    pub cli_present: bool,
    /// 通道起没起（**以内存里的端口为准**，不看端口文件 ——
    /// 端口文件可能是上一轮进程留下的 ⇒ 文件在 ≠ 通道在）
    pub channel_up: bool,
    /// 两者都满足才可用
    pub available: bool,
    /// CLI 的预期路径
    pub cli_path: String,
    /// 状态目录（token / 端口文件所在）
    pub state_dir: String,
    /// 当前通道端口
    pub port: Option<u16>,
    /// token 是否还在有效期内
    pub token_valid: bool,
    /// token 的过期时间戳
    pub token_expires_at: Option<u64>,
}

/// CLI 的预期安装位置：`<包根>\bin\orbcat-perm.exe`。
pub fn cli_install_path(app_root: &Path) -> PathBuf {
    app_root.join(CLI_SUBDIR).join(format!("{CLI_STEM}.exe"))
}

/// 查状态（路径显式传 ⇒ **可测**）。
pub fn status_at(app_root: &Path, state_dir: &Path) -> PermCliStatus {
    let cli_path = cli_install_path(app_root);
    let cli_present = cli_path.is_file();
    let port = channel_port();
    let channel_up = channel_up();
    PermCliStatus {
        cli_present,
        channel_up,
        available: cli_present && channel_up,
        cli_path: cli_path.to_string_lossy().to_string(),
        state_dir: state_dir.to_string_lossy().to_string(),
        port,
        token_valid: token_is_valid(state_dir, crate::permission::now_ms()),
        token_expires_at: token_expiry(state_dir),
    }
}

/// 查状态（用**运行时的** exe 目录 + 默认状态目录）—— 给 Tauri 命令用。
pub fn status_current() -> PermCliStatus {
    let app_root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    status_at(&app_root, &cli_state_dir())
}

/// 处理一次外部通道请求。
///
/// ## 口径
///
/// - **先验凭证**（不对 ⇒ 一律拒，**不看 op**）；
/// - op 只有 `list` / `approve <id>` / `deny <id>` ⇒ **没有**任何能改授权表的 op；
/// - 决定只有 `approve`（一次性）/ `deny`；
/// - 每个动作（含**只读**的 `list`）都进审计链；
/// - **成功**响应里带**新 token**（链式续期）⇒ 外部 AI 下次用新的。
pub fn handle_op(
    state_dir: &Path,
    data_dir: &Path,
    perm: &crate::perm_request::PermHub,
    token: &str,
    op: &str,
    arg: &str,
    client: &str,
) -> Result<String, String> {
    // 凭证对了就**轮换**（旧的立刻作废）⇒ 成功响应里把新的带回去
    let Some(new_token) = verify_token_rotate(state_dir, token, crate::permission::now_ms()) else {
        let msg = "凭证无效：外部通道只接受带正确 token 的请求".to_string();
        audit_cli(data_dir, "denied", op, arg, &msg);
        return Err(msg);
    };

    match op {
        "list" => {
            let views = perm.pending_views();
            let items: Vec<serde_json::Value> = views
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "id": v.id,
                        "kind": v.kind,
                        "command": v.command,
                        "normalized": v.normalized,
                        "path": v.path,
                        "reason": v.reason,
                        "sessionId": v.session_id,
                        "createdAt": v.created_at,
                        "timeoutMs": v.timeout_ms,
                    })
                })
                .collect();
            audit_cli(data_dir, "listed", op, arg, "列出 pending");
            Ok(serde_json::json!({
                "ok": true,
                "pending": items,
                "token": new_token,
            })
            .to_string())
        }
        "approve" | "deny" => {
            let Some(decision) = external_decision(op) else {
                let msg = format!("外部通道不支持该决定：{op}");
                audit_cli(data_dir, "denied", op, arg, &msg);
                return Err(msg);
            };
            let id = arg.trim();
            if id.is_empty() {
                let msg = "缺少 pending id".to_string();
                audit_cli(data_dir, "denied", op, arg, &msg);
                return Err(msg);
            }
            // 先取这条 pending 的 kind / command（提醒事件要用）——
            // ⚠️ 取不到**不阻断**决定（提醒信息不全 ≠ 不该批）。
            let view = perm.pending_views().into_iter().find(|v| v.id == id);
            let kind = view.as_ref().map(|v| v.kind.clone()).unwrap_or_default();
            let command = view.as_ref().map(|v| v.command.clone()).unwrap_or_default();
            let reason = format!("外部 CLI 代批（{op}）");
            let res = decide_pending(perm, id, decision, reason.clone());
            match res {
                Ok(()) => {
                    audit_cli(data_dir, op, id, arg, "外部代批成功");
                    // ── 代批提醒（用户要的「检测提醒」）──
                    // approve 与 deny **都发** ⇒ 拒绝也是用户该知道的事。
                    let ev = PermCliDecision {
                        pending_id: id.to_string(),
                        kind,
                        command,
                        decision: op.to_string(),
                        cli_instance: client.to_string(),
                        at: crate::permission::now_ms(),
                    };
                    if let Ok(v) = serde_json::to_value(&ev) {
                        perm.emit_event(EVENT_DECISION, v);
                    }
                    Ok(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "decision": op,
                        "token": new_token,
                    })
                    .to_string())
                }
                Err(e) => {
                    audit_cli(data_dir, "failed", id, arg, &e);
                    Err(e)
                }
            }
        }
        other => {
            let msg = format!("外部通道没有这个操作：{other}");
            audit_cli(data_dir, "denied", other, arg, &msg);
            Err(msg)
        }
    }
}

/// 按 id 前缀路由到对应的决定入口（口径与前端 `perm_request_decide` 一致）。
///
/// ⚠️ `pc*` / `pm*` / `pa*` / `pp*` 都在 `pending_cmds` 里 ⇒ 必须走 `decide_command`。
fn decide_pending(
    perm: &crate::perm_request::PermHub,
    id: &str,
    decision: crate::perm_request::Decision,
    reason: String,
) -> Result<(), String> {
    if id.starts_with("pc") || id.starts_with("pm") || id.starts_with("pa") || id.starts_with("pp") {
        perm.decide_command(id, decision, reason)
    } else {
        perm.decide(id, decision, reason)
    }
}

/// 外部通道的审计（谁 = 客户端 pid + 通道；批了哪条 id；决定了什么）。
fn audit_cli(data_dir: &Path, event: &str, target: &str, arg: &str, note: &str) {
    let rec = crate::command_policy::AuditRecord {
        ts: crate::permission::now_ms(),
        sequence: 0,
        prev_hash: String::new(),
        hash: String::new(),
        event: format!("perm-cli-{event}"),
        source: format!("perm-cli/{}", std::process::id()),
        command: format!("{target} {arg}").trim().to_string(),
        normalized: note.to_string(),
        cwd: String::new(),
        session_id: String::new(),
        risk: crate::command_policy::Risk::High.as_str().to_string(),
        shell: "perm-cli".to_string(),
        exit_code: None,
    };
    if let Err(e) = crate::command_policy::audit_append(data_dir, rec) {
        eprintln!("[orbcat] ⚠️ 写外部 CLI 审计失败: {e}");
    }
}

// ---------------------------------------------------------------------------
// sha256（自实现：Cargo.toml 里没有 sha2 依赖）
// ---------------------------------------------------------------------------
#[cfg(test)]

/// sha256 十六进制摘要。
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for v in h.iter() {
        out.push_str(&format!("{v:08x}"));
    }
    out
}


/// 运行时目录：**cwd**（相对路径的基准）与**包根**（exe 同目录）。
fn runtime_dirs() -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let app_root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| cwd.clone());
    (cwd, app_root)
}

/// 工具网关入口的阻断：**工具名 + 全部参数**里出现 CLI ⇒ 拒。
///
/// ⚠️ 命中即 **fail-closed**：不执行、不弹卡、不询问，并记审计。
pub fn block_tool_args(
    tool: &str,
    args: &serde_json::Value,
    data_dir: &Path,
) -> Result<(), String> {
    let (cwd, app_root) = runtime_dirs();
    if let Some(hit) = hit_token(tool, &cwd, &app_root) {
        audit_block(data_dir, tool, tool, &cwd, &hit);
        return Err(block_message(tool, &hit));
    }
    // 参数整体序列化后扫一遍（JSON 里含全部字符串值，比逐字段遍历省事且不漏）
    let blob = args.to_string();
    if let Some(hit) = scan_command_for_cli(&blob, &cwd, &app_root) {
        // ⚠️ 这条路径以前**只拒不留痕** —— 现在补上：工具网关拦下的也进审计链。
        audit_block(data_dir, tool, &blob, &cwd, &hit);
        return Err(block_message(tool, &hit));
    }
    Ok(())
}

/// 阻断时的错误文案（**要说清是策略硬阻断**，不是执行失败）。
fn block_message(tool: &str, hit: &CliHit) -> String {
    format!(
        "[硬阻断] 内部路径引用了外部代批 CLI（工具 {tool}，规则 {}，命中「{}」）—— \
         该能力只允许用户侧的外部 AI 使用，内部 AI 不许碰",
        hit.rule, hit.evidence
    )
}

/// 外部通道的本地端点：**独立**于 pi 的工具桥（内部 AI 够不着那条路）。
///
/// - 只绑 `127.0.0.1`（随机端口）；
/// - 端口写进 `<状态目录>\perm-port`，CLI 读它；
/// - 每个请求都必须带**有效 token**（短时效 + 用一次即轮换）。
///
/// 协议：一行 JSON 进、一行 JSON 出。
/// ```json
/// {"token":"...","op":"list|approve|deny","arg":"<pending id>","client":"orbcat-perm/1234"}
/// ```
/// - `client` = **CLI 实例标识**（仅用于提醒里显示「谁批的」，**不是安全边界** ——
///   客户端可自称）。
/// - 响应里带 `token` = **轮换后的新 token**（链式续期，见 [`verify_token_rotate`]）。
///
/// ## ⚠️ 已知弱点（**写清楚，别当成没有**）
///
/// 端口文件与 token 文件**同用户可读** ⇒ **内部 AI 理论上能读到 token 并自己直调端点**
/// （层 1 的路径阻断只挡「直接调用 CLI」）。这是**用户 2026-10-24 拍板后接受的残余风险**：
/// 本版**不做内核级隔离**，用「① 每次代批都有界面提醒 ② CLI 可随时删掉」兜底。
/// 要彻底封死需下一版做匿名 Job Object / 命名管道那一层。
///
/// ## 启动即自动签发 + 续期
///
/// - 启动时**自动签发**一个 token（用户不用手动开）；
/// - 后台线程每 `TTL/2` **重新签发** ⇒ 文件里的 token 不会过期
///   ⇒ 外部 AI 隔很久再来也能用（否则 5 分钟后链就断了）。
pub fn serve(
    state_dir: &Path,
    data_dir: &Path,
    perm: std::sync::Arc<crate::perm_request::PermHub>,
) -> Result<u16, String> {
    use std::io::{BufRead, BufReader, Write};

    // ⚠️ 必须转成 owned 再进线程（`&Path` 逃不出函数）
    let state_dir = state_dir.to_path_buf();
    let data_dir = data_dir.to_path_buf();

    // ① 启动即自动签发 token（不签发 ⇒ 外部 AI 一上来就全拒）
    match issue_token(&state_dir, crate::permission::now_ms()) {
        Ok(t) => eprintln!(
            "[orbcat] 外部代批通道：已自动签发 token（{}…）",
            &t[..8.min(t.len())]
        ),
        Err(e) => eprintln!("[orbcat] ⚠️ 外部代批通道：自动签发 token 失败: {e}"),
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("起外部通道失败: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("取端口失败: {e}"))?
        .port();
    write_port(&state_dir, port)?;
    CHANNEL_PORT.store(port, std::sync::atomic::Ordering::Relaxed);

    // ② 续期线程：每 TTL/2 重新签发 ⇒ 文件里的 token 不会过期
    {
        let sd = state_dir.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        register_serve(&state_dir, stop.clone());
        std::thread::spawn(move || {
            let step = 200u64;
            let mut waited = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(step));
                waited += step;
                if waited >= TOKEN_TTL_MS / 2 {
                    waited = 0;
                    if let Err(e) = issue_token(&sd, crate::permission::now_ms()) {
                        eprintln!("[orbcat] ⚠️ 外部代批通道：token 续期失败: {e}");
                    }
                }
            }
        });
    }

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state_dir = state_dir.to_path_buf();
            let data_dir = data_dir.to_path_buf();
            let perm = perm.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                });
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    return;
                }
                let resp = match serde_json::from_str::<serde_json::Value>(line.trim()) {
                    Ok(v) => {
                        let token = v.get("token").and_then(|x| x.as_str()).unwrap_or("");
                        let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("");
                        let arg = v.get("arg").and_then(|x| x.as_str()).unwrap_or("");
                        // CLI 实例标识（**仅展示**，客户端可自称 ⇒ 不是安全边界）
                        let client = v.get("client").and_then(|x| x.as_str()).unwrap_or("unknown");
                        match handle_op(&state_dir, &data_dir, &perm, token, op, arg, client) {
                            Ok(s) => s,
                            Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
                        }
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": format!("请求格式错: {e}") })
                        .to_string(),
                };
                let _ = writeln!(stream, "{resp}");
                let _ = stream.flush();
            });
        }
    });

    Ok(port)
}

/// 当前运行中的通道端口（0 = 没起）。
static CHANNEL_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// 通道起没起（**以内存里的端口为准**，不看端口文件 ——
/// 端口文件可能是上一轮进程留下的 ⇒ 文件在 ≠ 通道在）。
pub fn channel_up() -> bool {
    channel_port().is_some()
}

/// 当前通道端口（没起 ⇒ `None`）。
pub fn channel_port() -> Option<u16> {
    match CHANNEL_PORT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        p => Some(p),
    }
}

/// 已启动的通道（按状态目录索引）—— 用来停掉续期线程。
static SERVES: std::sync::OnceLock<
    std::sync::Mutex<Vec<(PathBuf, Arc<std::sync::atomic::AtomicBool>)>>,
> = std::sync::OnceLock::new();

fn register_serve(state_dir: &Path, stop: Arc<std::sync::atomic::AtomicBool>) {
    let v = SERVES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut g) = v.lock() {
        g.push((state_dir.to_path_buf(), stop));
    }
}
#[cfg(test)]

/// 停掉某个状态目录上的**续期线程**（主要是给测试用：
/// 测试结束不收线程 ⇒ 它会一直往已删的临时目录里写 token）。
pub fn stop_serve(state_dir: &Path) {
    if let Some(v) = SERVES.get() {
        if let Ok(g) = v.lock() {
            for (d, stop) in g.iter() {
                if d == state_dir {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录：**pid + 进程内原子序号**（并行跑不许互相删）。
    fn tmp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "orbcat_permcli_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个「包根」，里面放 `bin\orbcat-perm.exe`。
    fn app_with_cli(tag: &str) -> (PathBuf, PathBuf) {
        let root = tmp_dir(tag);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("orbcat-perm.exe"), b"fake").unwrap();
        (root, bin)
    }

    // -------------------------------------------------------------------
    // 守卫用例 ①
    // -------------------------------------------------------------------

    /// ① 内部 AI 路径撞 CLI ⇒ **硬阻断**：拒绝、不执行、不弹卡，且记审计。
    #[test]
    fn 内部AI引用CLI路径_直接硬阻断不弹卡() {
        let dir = tmp_dir("block");
        let (root, bin) = app_with_cli("block");
        let exe = bin.join("orbcat-perm.exe");
        let cmd = format!("\"{}\" list", exe.display());

        let err = block_if_cli(&cmd, &root, &root, &dir, "run_command")
            .expect_err("引用 CLI 路径必须被硬阻断");
        assert!(
            err.contains("硬阻断"),
            "错误文案要说清是策略硬阻断：{err}"
        );

        // 审计必须留下一条（谁、什么命令、命中哪条规则）
        let audit = std::fs::read_to_string(crate::command_policy::audit_path(&dir)).unwrap_or_default();
        assert!(
            audit.contains("cli-path-blocked"),
            "阻断必须进审计链：{audit}"
        );
        assert!(audit.contains("orbcat-perm"), "审计里要能看到命中的目标：{audit}");
    }

    // -------------------------------------------------------------------
    // 守卫用例 ②
    // -------------------------------------------------------------------

    /// ② 绕过字面路径也拦得住（相对路径 / 大小写 / 分隔符 / 引号）。
    #[test]
    fn 绕过字面路径的变体_同样命中阻断() {
        let (root, _bin) = app_with_cli("variants");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        for cmd in [
            r"..\bin\orbcat-perm.exe list",
            r"BIN\ORBCAT-PERM.EXE list",
            r"bin/orbcat-perm.exe list",
            r#""bin\orbcat-perm.exe" list"#,
            r"'bin\orbcat-perm.exe' list",
            r".\bin\orbcat-perm.exe list",
            r"orbcat-perm.exe list",
            r"orbcat-perm list",
        ] {
            assert!(
                scan_command_for_cli(cmd, &cwd, &root).is_some(),
                "这个变体漏了：{cmd}"
            );
        }
    }

    // -------------------------------------------------------------------
    // 守卫用例 ③
    // -------------------------------------------------------------------

    /// ③ 外部 CLI **不能**放大授权：只允许一次性决定。
    #[test]
    fn 外部通道只允许一次性决定_扩大授权的全被拒() {
        use crate::perm_request::Decision;
        assert_eq!(external_decision("approve"), Some(Decision::Approve));
        assert_eq!(external_decision("deny"), Some(Decision::Deny));
        for op in [
            "always",
            "full",
            "prefix",
            "narrow",
            "approve-always",
            "approve-full",
            "approve-prefix",
            "remember",
            "总是允许",
        ] {
            assert!(
                external_decision(op).is_none(),
                "外部通道不该接受 {op}（那是扩大授权）"
            );
        }
    }

    // -------------------------------------------------------------------
    // 守卫用例 ④
    // -------------------------------------------------------------------

    /// ④ 外部通道的每个动作都进审计链（**连只读的 list 也要**）。
    #[test]
    fn 外部通道每个动作都进审计链() {
        let dir = tmp_dir("audit");
        let hub = crate::perm_request::PermHub::new();
        write_token_hash(&dir, "secret-token").unwrap();

        handle_op(&dir, &dir, &hub, "secret-token", "list", "", "test")
            .expect("凭证正确应当放行");

        let audit = std::fs::read_to_string(crate::command_policy::audit_path(&dir)).unwrap_or_default();
        assert!(
            audit.contains("perm-cli-listed"),
            "list 也必须进审计链：{audit}"
        );
        // hash 链要连上
        let recs: Vec<crate::command_policy::AuditRecord> = audit
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(recs.len(), 1, "应当恰好一条审计：{audit}");
        assert!(!recs[0].hash.is_empty(), "审计记录要有 hash：{audit}");
    }

    // -------------------------------------------------------------------
    // 守卫用例 ⑤
    // -------------------------------------------------------------------

    /// ⑤ CLI 只能看到当前 pending：不能改授权表、不能删历史、不能碰别的组。
    #[test]
    fn CLI只能看到当前pending_不能改授权表不能删历史() {
        let dir = tmp_dir("scope");
        let hub = crate::perm_request::PermHub::new();
        write_token_hash(&dir, "secret-token").unwrap();

        for op in [
            "grant-always",
            "set-grants",
            "delete-history",
            "revoke-all",
            "approve-always",
            "clear-audit",
        ] {
            assert!(
                handle_op(&dir, &dir, &hub, "secret-token", op, "", "test").is_err(),
                "外部通道不该有 {op} 这个操作"
            );
        }

        // 授权表一个都不该出现
        assert!(
            !dir.join("command_grants.json").exists(),
            "外部通道不许写命令授权表"
        );
        assert!(
            !dir.join("mcp_grants.json").exists(),
            "外部通道不许写 MCP 授权表"
        );
    }

    // -------------------------------------------------------------------
    // 守卫用例 ⑥
    // -------------------------------------------------------------------

    /// ⑥ 归一化之后仍能命中（相对路径 / 大小写 / 分隔符 / 引号 / 复合命令）。
    #[test]
    fn 归一化后仍能命中_各种变体() {
        let (root, _bin) = app_with_cli("norm");
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        for cmd in [
            r#"powershell -Command "& '.\bin\orbcat-perm.exe' list""#,
            r"cmd /c bin\orbcat-perm.exe list",
            r"echo hi && .\bin\orbcat-perm.exe list",
            r"echo hi; BIN\ORBCAT-PERM.EXE list",
            r#"powershell -c ".\bin\orbcat-perm.exe""#,
        ] {
            assert!(
                scan_command_for_cli(cmd, &cwd, &root).is_some(),
                "归一化后仍应命中：{cmd}"
            );
        }
    }

    // -------------------------------------------------------------------
    // 守卫用例 ⑦
    // -------------------------------------------------------------------

    /// ⑦ 兜底层：路径检测**失效**时，通道鉴权那一层仍必须拒绝。
    ///
    /// 词法层挡不住运行时拼路径（已知缺口），但「批准权限」这个能力
    /// 只在 L2 通道里 ⇒ 没有凭证就什么也做不成。
    #[test]
    fn 路径检测失效时_通道鉴权仍然拒绝() {
        let dir = tmp_dir("fallback");
        let hub = crate::perm_request::PermHub::new();
        write_token_hash(&dir, "right-token").unwrap();

        // 一个词法层看不见的输入（运行时拼路径）
        let sneaky = r#"powershell -c "$p='D:\orbcat\bin\orbcat-perm'+'.exe'; & $p""#;
        // ⚠️ 不断言「一定扫不到」—— 扫到了更好；这里只断言**通道层**独立拒绝。
        let _ = scan_command_for_cli(sneaky, &dir, &dir);

        // 错凭证 ⇒ 一律拒（且**不看 op**）
        for op in ["list", "approve", "deny"] {
            assert!(
                handle_op(&dir, &dir, &hub, "wrong-token", op, "pm1", "test").is_err(),
                "错凭证必须被拒（op={op}）"
            );
        }
        // 没写 token 文件 ⇒ 也拒
        let empty = tmp_dir("fallback-empty");
        assert!(
            handle_op(&empty, &empty, &hub, "anything", "list", "", "test").is_err(),
            "没配 token 时必须全拒"
        );
    }

    // -------------------------------------------------------------------
    // 附：sha256 自查（标准测试向量）
    // -------------------------------------------------------------------

    #[test]
    fn sha256_标准测试向量() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // -------------------------------------------------------------------
    // 守卫用例 ⑧ ~ ⑭（2026-10-24 收尾：通道接上 + 代批提醒 + 可选删除）
    // -------------------------------------------------------------------

    /// 造一条待办（`ask_user` 是 async ⇒ 放后台线程跑），返回 pending id。
    fn spawn_pending(
        hub: &std::sync::Arc<crate::perm_request::PermHub>,
        q: &str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let hub2 = hub.clone();
        let q = q.to_string();
        let h = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ = hub2
                    .ask_user(q, vec!["是".to_string()], String::new(), "s-test")
                    .await;
            });
        });
        let mut id = String::new();
        for _ in 0..300 {
            if let Some(v) = hub.pending_views().first() {
                id = v.id.clone();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!id.is_empty(), "待办应当冒出来");
        (id, h)
    }

    /// 读一个**代批提醒**事件（跳过 `perm-request` 之类的其它事件）。
    fn recv_decision(
        rx: &std::sync::mpsc::Receiver<(String, serde_json::Value)>,
    ) -> serde_json::Value {
        for _ in 0..20 {
            let (ev, payload) = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("必须收到事件");
            if ev == EVENT_DECISION {
                return payload;
            }
        }
        panic!("没等到代批提醒事件（{EVENT_DECISION}）");
    }

    /// ⑧ 通道在宿主起来后**确实能连**（不是「函数写好了」而已）。
    #[test]
    fn 外部代批通道在宿主起来后端点可连() {
        use std::io::{BufRead, BufReader, Write};
        let dir = tmp_dir("serve");
        let hub = std::sync::Arc::new(crate::perm_request::PermHub::new());
        let port = serve(&dir, &dir, hub).expect("通道应当起得来");
        assert!(port > 0, "端口得是真的");

        // 端口文件必须写出来（CLI 靠它找端点）
        let port_file = std::fs::read_to_string(port_path(&dir)).unwrap_or_default();
        assert_eq!(
            port_file.trim().parse::<u16>().unwrap(),
            port,
            "端口文件里的值必须等于真实端口"
        );

        // 用**自动签发**的 token 真连一次
        let raw = std::fs::read_to_string(token_path(&dir)).expect("启动应当自动签发 token");
        let token = raw.lines().next().unwrap().trim().to_string();

        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("端点应当能连");
        let req = format!(
            "{{\"token\":\"{token}\",\"op\":\"list\",\"arg\":\"\",\"client\":\"test\"}}\n"
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("响应不是 JSON：{line}（{e}）"));
        assert_eq!(v["ok"], true, "带正确 token 的 list 应当成功：{line}");
        assert!(
            v.get("token").and_then(|x| x.as_str()).is_some(),
            "成功响应必须带新 token（链式续期）：{line}"
        );
        stop_serve(&dir);
    }

    /// ⑨ token **自动签发**，落盘格式 = 明文 + 过期戳（已定稿）。
    #[test]
    fn token自动签发_落盘是明文加过期戳() {
        let dir = tmp_dir("autotoken");
        let now = crate::permission::now_ms();
        let token = issue_token(&dir, now).expect("应当签发得出来");

        let raw = std::fs::read_to_string(token_path(&dir)).expect("token 文件应当存在");
        let mut lines = raw.lines();
        let l1 = lines.next().unwrap().trim().to_string();
        let l2 = lines.next().unwrap().trim().to_string();
        assert_eq!(l1, token, "第 1 行 = 明文 token");
        assert!(
            (128..=256).contains(&l1.len()),
            "token = random_hex(16) ⇒ 16 段 × (8..16) 位小写 hex，实际 {}",
            l1.len()
        );
        assert!(
            l1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "token 只能是小写 hex：{l1}"
        );
        let expiry: u64 = l2.parse().expect("第 2 行 = 过期时间戳（数字）");
        assert_eq!(expiry, now + TOKEN_TTL_MS, "过期时间 = 签发时刻 + TTL");
        assert!(lines.next().is_none(), "只有两行，不许多写");
    }

    /// ⑩ approve 响应里带**新 token** ⇒ 旧 token 作废 ⇒ 链式续期可用。
    #[test]
    fn approve响应带新token_旧token作废_链式续期可用() {
        let dir = tmp_dir("chain");
        let hub = std::sync::Arc::new(crate::perm_request::PermHub::new());
        let (id, _h) = spawn_pending(&hub, "要不要装这个包");
        let old = issue_token(&dir, crate::permission::now_ms()).unwrap();

        let resp =
            handle_op(&dir, &dir, &hub, &old, "approve", &id, "orbcat-perm/1").expect("应当成功");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let new_token = v["token"]
            .as_str()
            .expect("成功响应必须带新 token")
            .to_string();
        assert_ne!(new_token, old, "新 token 必须与旧的不同");

        // 旧 token 作废（用一次即轮换）
        assert!(
            !verify_token(&dir, &old, crate::permission::now_ms()),
            "旧 token 必须立刻作废"
        );
        // 新 token 可用 ⇒ 链没断
        let resp2 = handle_op(&dir, &dir, &hub, &new_token, "list", "", "orbcat-perm/1").unwrap();
        let v2: serde_json::Value = serde_json::from_str(&resp2).unwrap();
        assert_eq!(v2["ok"], true);
        assert!(
            v2.get("token").and_then(|x| x.as_str()).is_some(),
            "list 也要带新 token（否则调用方手里的 token 会作废）"
        );
    }

    /// ⑪ 外部通道**仍然只允许一次性决定**（always / prefix / full / narrow 全拒）。
    #[test]
    fn 外部通道仍只允许一次性决定_always等全拒() {
        for op in [
            "always",
            "full",
            "prefix",
            "narrow",
            "approve-always",
            "approve-full",
            "approve-prefix",
            "remember",
            "总是允许",
        ] {
            assert!(
                external_decision(op).is_none(),
                "外部通道不该接受 {op}（那是扩大授权）"
            );
        }
        // 通道层也要拒（不是只在白名单函数里拒）
        let dir = tmp_dir("oneshot");
        let hub = std::sync::Arc::new(crate::perm_request::PermHub::new());
        for op in ["always", "full", "prefix", "narrow"] {
            // 每次都要重签（成功请求会轮换 token）
            let t = issue_token(&dir, crate::permission::now_ms()).unwrap();
            let err = handle_op(&dir, &dir, &hub, &t, op, "", "test")
                .expect_err("扩大授权的决定必须被拒");
            assert!(
                err.contains(op) || err.contains("不支持该决定"),
                "错误信息要说清是哪个 op 被拒：{err}"
            );
        }
    }

    /// ⑫ 代批提醒事件会发（approve 与 deny **都发**，字段齐全）。
    #[test]
    fn 代批提醒事件会发_approve与deny都发且字段齐全() {
        let dir = tmp_dir("notify");
        let hub = std::sync::Arc::new(crate::perm_request::PermHub::new());
        let (tx, rx) = std::sync::mpsc::channel::<(String, serde_json::Value)>();
        hub.attach_emitter(Box::new(move |ev, payload| {
            let _ = tx.send((ev.to_string(), payload));
        }));

        // ① approve ⇒ 必须有提醒
        let (id1, _h1) = spawn_pending(&hub, "第一条申请");
        let t1 = issue_token(&dir, crate::permission::now_ms()).unwrap();
        handle_op(&dir, &dir, &hub, &t1, "approve", &id1, "orbcat-perm/42").unwrap();
        let p1 = recv_decision(&rx);
        assert_eq!(p1["pendingId"], id1.as_str(), "pendingId 要对得上");
        assert_eq!(p1["decision"], "approve");
        assert_eq!(p1["cliInstance"], "orbcat-perm/42", "CLI 实例标识要带上来");
        assert!(
            !p1["kind"].as_str().unwrap_or("").is_empty(),
            "kind 字段不能空：{p1}"
        );
        assert!(p1["at"].as_u64().unwrap_or(0) > 0, "时间戳要有：{p1}");

        // ② deny ⇒ **也**必须有提醒（拒绝也是用户该知道的事）
        let (id2, _h2) = spawn_pending(&hub, "第二条申请");
        let t2 = issue_token(&dir, crate::permission::now_ms()).unwrap();
        handle_op(&dir, &dir, &hub, &t2, "deny", &id2, "orbcat-perm/43").unwrap();
        let p2 = recv_decision(&rx);
        assert_eq!(p2["pendingId"], id2.as_str());
        assert_eq!(p2["decision"], "deny", "deny 也要发提醒");
    }

    /// ⑬ CLI 文件缺失时，状态查询**如实报不可用**。
    #[test]
    fn CLI文件缺失时状态查询如实报不可用() {
        let root = tmp_dir("ncli"); // 空的包根（没有 bin\orbcat-perm.exe）
        let state = tmp_dir("ncli-state");
        let st = status_at(&root, &state);
        assert!(!st.cli_present, "没有 CLI 文件 ⇒ cli_present 必须为 false");
        assert!(!st.available, "CLI 缺失 ⇒ 必须如实报不可用");
        assert!(
            st.cli_path.ends_with("orbcat-perm.exe"),
            "路径要指向预期位置：{}",
            st.cli_path
        );
        assert!(!st.token_valid, "没签发 token ⇒ token 必然无效");

        // 反过来：文件在 ⇒ cli_present = true
        let (root2, _bin) = app_with_cli("ncli-ok");
        let st2 = status_at(&root2, &state);
        assert!(st2.cli_present, "文件在就该报在");
        // available = 「文件在」且「通道起」⇒ 不变式
        assert_eq!(st2.available, st2.cli_present && st2.channel_up);
    }

    /// ⑭ 代批决定**照旧进审计链**（hash 链完整）。
    #[test]
    fn 代批决定照旧进审计链_hash链完整() {
        let dir = tmp_dir("audit2");
        let hub = std::sync::Arc::new(crate::perm_request::PermHub::new());

        let (id1, _h1) = spawn_pending(&hub, "第一条");
        let t1 = issue_token(&dir, crate::permission::now_ms()).unwrap();
        handle_op(&dir, &dir, &hub, &t1, "approve", &id1, "orbcat-perm/7").unwrap();

        let (id2, _h2) = spawn_pending(&hub, "第二条");
        let t2 = issue_token(&dir, crate::permission::now_ms()).unwrap();
        handle_op(&dir, &dir, &hub, &t2, "deny", &id2, "orbcat-perm/7").unwrap();

        let audit =
            std::fs::read_to_string(crate::command_policy::audit_path(&dir)).unwrap_or_default();
        assert!(audit.contains("perm-cli-approve"), "代批必须进审计链：{audit}");
        assert!(audit.contains("perm-cli-deny"), "拒绝也要进审计链：{audit}");

        let recs: Vec<crate::command_policy::AuditRecord> = audit
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert!(recs.len() >= 2, "至少两条：{audit}");
        assert!(
            recs.iter().all(|r| !r.hash.is_empty()),
            "每条都要有 hash：{audit}"
        );
        // hash 链：后一条的 prev_hash 指向上一条的 hash
        for w in recs.windows(2) {
            assert_eq!(w[1].prev_hash, w[0].hash, "hash 链必须连上：{audit}");
        }
    }
}
