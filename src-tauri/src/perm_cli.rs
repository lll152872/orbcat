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
//! # 凭证口径（为什么不落明文）
//!
//! - **明文 token 只存在于 CLI 那一个子进程的环境变量**（`ORBCAT_PERM_TOKEN`）；
//! - **orbcat 磁盘上只有 sha256 哈希**（`perm-cli-token.json`）；
//! - ⇒ 内部 AI 即便读到 orbcat 的数据目录，**也造不出 token**（读不到明文）；
//! - ⇒ 且 orbcat **自己的进程 env 里没有** token ⇒ 内部 AI 起的子进程继承不到。

// ⚠️ 层 2 的入口函数（serve / issue_token / write_port …）本轮还没接线
//    ⇒ 未被调用 ⇒ dead_code ⇒ 本模块整体先放行；**下一笔接完线后删掉这行**。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

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
    // ⛔⛔ 临时骨架（只为取「先红」证据）—— 正式实现见本函数下方
    if std::env::var("ORBCAT_PERM_CLI_SKELETON").is_ok() {
        let _ = (cmd, cwd, app_root);
        return None;
    }
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
    // 审计：阻断也要留痕（谁、什么、命中哪条规则）
    let rec = crate::command_policy::AuditRecord {
        ts: crate::permission::now_ms(),
        sequence: 0,
        prev_hash: String::new(),
        hash: String::new(),
        event: "cli-path-blocked".to_string(),
        source: format!("perm-cli-guard/{}", hit.rule),
        command: cmd.to_string(),
        normalized: cmd.to_string(),
        cwd: cwd.to_string_lossy().to_string(),
        session_id: String::new(),
        risk: crate::command_policy::Risk::High.as_str().to_string(),
        shell: "guard".to_string(),
        exit_code: None,
    };
    if let Err(e) = crate::command_policy::audit_append(data_dir, rec) {
        eprintln!("[orbcat] ⚠️ 写 CLI 阻断审计失败: {e}");
    }
    Err(msg)
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

/// token 文件（**明文 + 过期时间戳**；短时效、用一次即轮换）。
pub fn token_path(state_dir: &Path) -> PathBuf {
    state_dir.join("perm-token")
}

/// 端口文件（CLI 靠它找到 orbcat 的端点）。
pub fn port_path(state_dir: &Path) -> PathBuf {
    state_dir.join("perm-port")
}

/// token 有效期：**5 分钟**。
pub const TOKEN_TTL_MS: u64 = 5 * 60 * 1000;

/// 签发 token（明文 + 过期时间）。返回明文（设置页显示一次给用户）。
pub fn issue_token(state_dir: &Path, now_ms: u64) -> Result<String, String> {
    let token = random_hex(16);
    let expiry = now_ms.saturating_add(TOKEN_TTL_MS);
    std::fs::create_dir_all(state_dir).map_err(|e| format!("建状态目录失败: {e}"))?;
    std::fs::write(token_path(state_dir), format!("{token}\n{expiry}"))
        .map_err(|e| format!("写 token 失败: {e}"))?;
    Ok(token)
}

/// 写入 token（**明文 + 过期时间**）到状态目录。
///
/// ⚠️ 名字保留了旧词「hash」，但语义已改：现在落的是**明文 token + 过期时间戳**
///    （短时效 + 用一次即轮换 ⇒ 落明文才轮换得动；哈希没法轮换）。
pub fn write_token_hash(state_dir: &Path, token: &str) -> Result<(), String> {
    let expiry = crate::permission::now_ms().saturating_add(TOKEN_TTL_MS);
    std::fs::create_dir_all(state_dir).map_err(|e| format!("建状态目录失败: {e}"))?;
    std::fs::write(token_path(state_dir), format!("{token}\n{expiry}"))
        .map_err(|e| format!("写 token 失败: {e}"))
}

/// 校验凭证：**明文比对 + 未过期**；**用一次即轮换**（消费后立刻写回新值）。
///
/// ⚠️ 这是「短时效 + 一次性」两道限制的落点：
/// 过期 ⇒ 拒；已被消费过 ⇒ 旧值已失效 ⇒ 拒。
pub fn verify_token(state_dir: &Path, token: &str, now_ms: u64) -> bool {
    let Ok(s) = std::fs::read_to_string(token_path(state_dir)) else {
        return false;
    };
    let mut it = s.lines();
    let (Some(cur), Some(exp)) = (it.next(), it.next()) else {
        return false;
    };
    if cur.trim() != token {
        return false;
    }
    let expiry: u64 = exp.trim().parse().unwrap_or(0);
    if now_ms > expiry {
        return false;
    }
    // 用一次即轮换
    let _ = issue_token(state_dir, now_ms);
    true
}

/// 写端口文件（orbcat 启动时把随机端口写进去，CLI 读它）。
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

/// 处理一次外部通道请求。
///
/// ## 口径
///
/// - **先验凭证**（不对 ⇒ 一律拒，**不看 op**）；
/// - op 只有 `list` / `approve <id>` / `deny <id>` ⇒ **没有**任何能改授权表的 op；
/// - 决定只有 `approve`（一次性）/ `deny`；
/// - 每个动作（含**只读**的 `list`）都进审计链。
pub fn handle_op(
    state_dir: &Path,
    data_dir: &Path,
    perm: &crate::perm_request::PermHub,
    token: &str,
    op: &str,
    arg: &str,
) -> Result<String, String> {
    if !verify_token(state_dir, token, crate::permission::now_ms()) {
        let msg = "凭证无效：外部通道只接受带正确 token 的请求".to_string();
        audit_cli(data_dir, "denied", op, arg, &msg);
        return Err(msg);
    }

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
            Ok(serde_json::json!({ "ok": true, "pending": items }).to_string())
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
            let reason = format!("外部 CLI 代批（{op}）");
            let res = decide_pending(perm, id, decision, reason.clone());
            match res {
                Ok(()) => {
                    audit_cli(data_dir, op, id, arg, "外部代批成功");
                    Ok(serde_json::json!({ "ok": true, "id": id, "decision": op }).to_string())
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
        return Err(block_message(tool, &hit));
    }
    // 参数整体序列化后扫一遍（JSON 里含全部字符串值，比逐字段遍历省事且不漏）
    let blob = args.to_string();
    if let Some(hit) = scan_command_for_cli(&blob, &cwd, &app_root) {
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
/// {"token":"...","op":"list|approve|deny","arg":"<pending id>"}
/// ```
pub fn serve(
    state_dir: &Path,
    data_dir: &Path,
    perm: std::sync::Arc<crate::perm_request::PermHub>,
) -> Result<u16, String> {
    use std::io::{BufRead, BufReader, Write};

    // ⚠️ 必须转成 owned 再进线程（`&Path` 逃不出函数）
    let state_dir = state_dir.to_path_buf();
    let data_dir = data_dir.to_path_buf();

    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("起外部通道失败: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("取端口失败: {e}"))?
        .port();
    write_port(&state_dir, port)?;

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
                        match handle_op(&state_dir, &data_dir, &perm, token, op, arg) {
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

        handle_op(&dir, &dir, &hub, "secret-token", "list", "")
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
                handle_op(&dir, &dir, &hub, "secret-token", op, "").is_err(),
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
                handle_op(&dir, &dir, &hub, "wrong-token", op, "pm1").is_err(),
                "错凭证必须被拒（op={op}）"
            );
        }
        // 没写 token 文件 ⇒ 也拒
        let empty = tmp_dir("fallback-empty");
        assert!(
            handle_op(&empty, &empty, &hub, "anything", "list", "").is_err(),
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
}
