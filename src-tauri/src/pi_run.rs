//! pi 事件流 → orbcat `Progress` 事件的对接层。
//!
//! ## 为什么单独一层
//!
//! pi 的事件是「协议级的」（`message_update` / `tool_execution_start` / …），
//! orbcat 前端认的是 `agent::Progress`（`Thinking` / `Delta` / `ToolCall` / …）。
//! 两套词汇不同，但**语义几乎一一对应**。硬塞进 `agent.rs` 会让那个文件
//! 同时背两套世界观 —— 而 `agent.rs` 已经是 4415 行了。
//!
//! 所以这层只做**翻译**，不碰任何业务逻辑。
//!
//! ## 翻译表
//!
//! | pi 事件 | → orbcat Progress |
//! |---|---|
//! | `turn_start` | `Thinking { iteration }` |
//! | `message_update{text_delta}` | `Delta { text: Some(..) }` |
//! | `message_update{thinking_delta}` | `Delta { reasoning: Some(..) }` |
//! | `message_update{toolcall_end}` | `ToolCall { name, args }` |
//! | `tool_execution_start` | （不发，前一条 ToolCall 已够）|
//! | `tool_execution_end` | `ToolResult { name, preview }` / `ToolError` |
//! | `auto_retry_start` | `Status { text, retry: true }` |
//! | `compaction_start` | `Status { text, retry: false }` |
//!
//! ## 三条不能丢的语义
//!
//! 1. **工具失败要单独成行**（`ToolError`），不能折进 `ToolResult` ——
//!    折进去就等于没人看得见（这是项目里反复强调过的折叠规则）。
//! 2. **`tool_execution_end` 的结果要截断** —— pi 的结果可能是几 MB 的 JSON，
//!    整条塞进事件里会撑爆前端。
//! 3. **重试要能让用户看见** —— 否则表现为「卡住了」，用户以为死了。

use crate::agent::Progress;
use crate::pi::{self, PiEvent};

/// 工具结果预览的最大**行数**（P1-2：从 1200 字符改成按行）。
///
/// ⚠️ 实际行数按工具分档，见 `preview_shape`。
/// 常量只作为「通用兜底」的那一档。
const PREVIEW_MAX_LINES: usize = 10;

/// 把一条 pi 事件翻译成 orbcat 的 `Progress`（可能没有 —— 有些事件前端不需要）。
pub fn translate(ev: &PiEvent, iteration: &mut usize) -> Option<Progress> {
    let t = ev.get("type").and_then(|x| x.as_str()).unwrap_or("");

    match t {
        "turn_start" => {
            *iteration += 1;
            Some(Progress::Thinking {
                iteration: *iteration,
            })
        }

        "message_update" => {
            let a = ev.get("assistantMessageEvent")?;
            match a.get("type").and_then(|x| x.as_str()) {
                Some("text_delta") => {
                    let d = a.get("delta").and_then(|x| x.as_str())?;
                    if d.is_empty() {
                        return None;
                    }
                    Some(Progress::Delta {
                        text: Some(d.to_string()),
                        reasoning: None,
                    })
                }
                Some("thinking_delta") => {
                    let d = a.get("delta").and_then(|x| x.as_str())?;
                    if d.is_empty() {
                        return None;
                    }
                    Some(Progress::Delta {
                        text: None,
                        reasoning: Some(d.to_string()),
                    })
                }
                Some("toolcall_end") => {
                    let (_id, name, args) = pi::tool_call(ev)?;
                    let raw = args.to_string();
                    Some(Progress::ToolCall {
                        name,
                        args: crate::agent::summarize_args_pub(&raw),
                    })
                }
                _ => None,
            }
        }

        "message_end" => {
            // pi 的**差异化错误提示**（P2-3）。
            //
            // pi 在 `message_end` 按 `stopReason` 分别给不同文案
            // （`assistant-message.ts:180-200`）：
            //   length  → 回答被截断
            //   aborted → 已中止
            //   error   → 错误：{errorMessage}
            //
            // ⚠️ **只处理这三种**：`stop` / `toolUse` / `pending` / `deferred` 都不加任何东西。
            //    尤其 `toolUse` —— 那是「模型改去调工具了」，是正常流程，
            //    加一条提示只会在时间线里多一行噪声。
            //
            // ⚠️ **这里不碰「哪条是最终答复」的判定**（那是 P1-3，最高风险：
            //    误判会把表情包 markdown 折进「过程」外壳 → 用户完全看不到图）。
            //    本分支只多发一条 Status，不影响 answer 累加与 DiscardStream 逻辑。
            let msg = ev.get("message")?;
            let stop = msg.get("stopReason").and_then(|x| x.as_str()).unwrap_or("");
            match stop {
                "length" => Some(Progress::Status {
                    text: "回答被截断（达到长度上限）".into(),
                    retry: false,
                }),
                "aborted" => Some(Progress::Status {
                    text: "已中止".into(),
                    retry: false,
                }),
                "error" => {
                    let e = msg
                        .get("errorMessage")
                        .and_then(|x| x.as_str())
                        .unwrap_or("未知错误");
                    Some(Progress::Status {
                        text: format!("错误：{e}"),
                        retry: false,
                    })
                }
                _ => None,
            }
        }

        "tool_execution_update" => {
            // pi 的**流式工具输出**（P0-1）—— bash 之类长命令执行期间的增量。
            //
            // ⚠️ 为什么这里**不做节流**：pi 那边 100ms 节流是因为终端每帧重拼整轮；
            // 我们这边前端 `paintToolTick` 是「**就地刷**最后一条 tool_call 的尾巴」
            // （不新增时间线条目），所以高频更新不会刷成流水账。
            let name = ev
                .get("toolName")
                .and_then(|x| x.as_str())
                .unwrap_or("工具")
                .to_string();
            let partial = ev.get("partialResult").cloned().unwrap_or(serde_json::Value::Null);
            Some(Progress::ToolTick {
                name,
                // 后端没有单调钟基准，秒数交给前端自己算（前端收到第一条 tick 时起表）。
                elapsed_secs: 0,
                tail: tail_of(&partial),
            })
        }

        "tool_execution_end" => {
            let (name, is_err, result) = pi::tool_result(ev)?;
            let preview = preview_of(&result, &name);
            // 工具耗时（P0-2）：pi 用 monotonic clock 测的，工具没跑就没有这个字段。
            let duration_ms = ev.get("durationMs").and_then(|x| x.as_u64());
            if is_err {
                Some(Progress::ToolError {
                    name,
                    message: preview,
                    duration_ms,
                })
            } else {
                Some(Progress::ToolResult {
                    name,
                    preview,
                    duration_ms,
                })
            }
        }

        "auto_retry_start" => {
            let attempt = ev.get("attempt").and_then(|x| x.as_u64()).unwrap_or(0);
            let max = ev.get("maxAttempts").and_then(|x| x.as_u64()).unwrap_or(0);
            let err = ev
                .get("errorMessage")
                .and_then(|x| x.as_str())
                .unwrap_or("未知错误");
            // ⚠️ **致命错误要在重试开始前就拦下** ——
            //
            // pi 的重试判定只认英文文案（`packages/ai/src/utils/retry.ts`），
            // 国内厂商的中文「额度用完」它认不出来 → 当成瞬时限流 →
            // 无限重试（实测 15 分钟，session 里全是空 assistant 消息）。
            //
            // 这里把错误原样抛出去，让 Rust 侧快速失败。
            if pi::is_fatal_error_text(err) {
                return Some(Progress::Status {
                    text: format!("模型调用失败（不可重试）：{err}"),
                    retry: false,
                });
            }
            Some(Progress::Status {
                text: format!("第 {attempt}/{max} 次重试：{}", truncate(err, 200)),
                retry: true,
            })
        }

        "auto_retry_end" => {
            let ok = ev.get("success").and_then(|x| x.as_bool()).unwrap_or(false);
            if ok {
                Some(Progress::Status {
                    text: "重试成功".into(),
                    retry: true,
                })
            } else {
                None
            }
        }

        "compaction_start" => {
            let reason = ev.get("reason").and_then(|x| x.as_str()).unwrap_or("");
            Some(Progress::Status {
                text: format!("压缩上下文（{reason}）…"),
                retry: false,
            })
        }

        _ => None,
    }
}

/// 从 pi 的工具结果里抽一段人类可读的预览。
///
/// ⚠️ **按行截断，不按字符**（P1-2，学 pi 的 `visual-truncate.ts`）：
/// 按字符一刀切会把一条压缩 JSON **从中间斩断**，头尾都不完整。
/// pi 的理由原话：按逻辑行截断会让**单条长行**（压缩 JSON）横向铺满全屏 ——
/// 所以它按**视觉行**算。我们这里是 460px 宽的面板，同样不能留超长行。
fn preview_of(result: &serde_json::Value, tool: &str) -> String {
    // pi 的结果形状：`{content:[{type:"text",text:"..."}], details:{...}}`
    let mut out = String::new();
    if let Some(arr) = result.get("content").and_then(|x| x.as_array()) {
        for b in arr {
            match b.get("type").and_then(|x| x.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
                // 图片块不能**静默消失**（P1-1）—— 用户得知道工具返回了图。
                Some("image") => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    let mime = b.get("mimeType").and_then(|x| x.as_str()).unwrap_or("图片");
                    out.push_str(&format!("[图片 {mime}]"));
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            let (max_lines, keep) = preview_shape(tool);
            return truncate_lines(&out, max_lines, keep);
        }
    }
    if let Some(s) = result.as_str() {
        let (max_lines, keep) = preview_shape(tool);
        return truncate_lines(s, max_lines, keep);
    }
    let (max_lines, keep) = preview_shape(tool);
    truncate_lines(&result.to_string(), max_lines, keep)
}

/// 这个工具的结果该看多少行、留头还是留尾。
///
/// 抄 pi 的 `renderers/*` 里的预览行数（`bash` 5 行留尾、`read` 10 行、
/// `grep` 15 行、`ls` 20 行、通用 10 行）。留尾是因为**日志和错误栈的尾巴才是重点**。
fn preview_shape(tool: &str) -> (usize, Keep) {
    match tool {
        "run_command" | "bash" | "shell" => (5, Keep::End),
        "grep" | "search" => (15, Keep::Head),
        "list_dir" | "ls" => (20, Keep::Head),
        _ => (10, Keep::Head),
    }
}

/// 截断时留头还是留尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Head,
    End,
}

/// 截断保留哪头。
pub const KEEP_DEFAULT: Keep = Keep::Head;

/// 从 pi 的流式部分结果里抽**最后一行非空文本**（心跳尾巴，单行）。
fn tail_of(partial: &serde_json::Value) -> String {
    let mut text = String::new();
    if let Some(arr) = partial.get("content").and_then(|x| x.as_array()) {
        for b in arr {
            if b.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }
    if text.is_empty() {
        if let Some(s) = partial.as_str() {
            text.push_str(s);
        }
    }
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 一行的**显示宽度**上限。
///
/// 面板是 460px 宽，工具结果用 11–12px 字体 —— 算下来一行放得下约 60 个半角字符
/// （中文按 2 宽算，就是 30 个汉字）。超了就会**横向铺满**。
///
/// pi 的原话（`visual-truncate.ts` 注释）：按逻辑行截断会让
/// **单条长行（如压缩 JSON）横向铺满全屏** —— 所以它截完行数还要再截一次宽度。
const LINE_WIDTH: usize = 60;

/// 一个字符占几个显示宽度（中文/日文/韩文等算 2）。
fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF |
        0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF |
        0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6);
    if wide { 2 } else { 1 }
}

/// 单行按**显示宽度**截断。
fn truncate_line_width(line: &str, max_width: usize) -> (String, bool) {
    let mut w = 0usize;
    let mut out = String::new();
    for ch in line.chars() {
        let cw = char_width(ch);
        if w + cw > max_width {
            out.push('…');
            return (out, true);
        }
        w += cw;
        out.push(ch);
    }
    (out, false)
}

/// 按**行**截断（带「还有 N 行」提示）。
///
/// 两级：先按**行数**，再按**显示宽度**（单行超长也要断，否则横向铺满）。
/// 提示说的是**行**不是字符 —— 用户关心的是「还有多少行没看」，
/// 且**不静默丢弃**：静默丢行用户会以为那就是全部。
pub fn truncate_lines(s: &str, max_lines: usize, keep: Keep) -> String {
    let lines: Vec<&str> = s.lines().collect();

    // 第一级：每行按宽度截
    let mut trimmed: Vec<String> = Vec::with_capacity(lines.len());
    let mut wide_cut = 0usize;
    for l in &lines {
        let (t, cut) = truncate_line_width(l, LINE_WIDTH);
        if cut {
            wide_cut += 1;
        }
        trimmed.push(t);
    }

    // 第二级：按行数截
    if lines.len() > max_lines {
        let hidden = lines.len() - max_lines;
        let kept: &[String] = match keep {
            Keep::Head => &trimmed[..max_lines],
            Keep::End => &trimmed[trimmed.len() - max_lines..],
        };
        let mut out = kept.join("\n");
        out.push('\n');
        out.push_str(&format!(
            "…（已隐藏 {hidden} 行，已截断，点开看全部）"
        ));
        return out;
    }

    let mut out = trimmed.join("\n");
    if wide_cut > 0 {
        out.push('\n');
        out.push_str(&format!("…（{wide_cut} 行过长，已截断）"));
    }
    out
}

/// 截断（带省略标记）—— **按字符**的旧口径。
///
/// ⚠️ 只给「一行内的长文本」用（比如重试原因那种一句话）。
/// 多行内容一律走 `truncate_lines`。
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…（已截断，共 {} 字符）", s.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------- P0-1 流式工具输出 ----------------

    #[test]
    fn 流式工具输出翻成ToolTick() {
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_update",
            "toolName": "run_command",
            "partialResult": {"content": [{"type": "text", "text": "第一行\n第二行\n"}]}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolTick { name, tail, .. } => {
                assert_eq!(name, "run_command");
                // 尾巴取**最后一行非空**，单行
                assert_eq!(tail, "第二行");
            }
            _ => panic!("应是 ToolTick"),
        }
    }

    #[test]
    fn 流式工具输出没有文本时尾巴为空() {
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_update",
            "toolName": "run_command",
            "partialResult": {"content": []}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolTick { tail, .. } => assert!(tail.is_empty()),
            _ => panic!("应是 ToolTick"),
        }
    }

    // ---------------- P0-2 工具耗时 ----------------

    #[test]
    fn 工具耗时从durationMs带过来() {
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_end",
            "toolName": "read_file",
            "isError": false,
            "durationMs": 1234,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolResult { duration_ms, .. } => assert_eq!(duration_ms, Some(1234)),
            _ => panic!("应是 ToolResult"),
        }
    }

    #[test]
    fn 工具没跑起来时没有耗时() {
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_end",
            "toolName": "read_file",
            "isError": false,
            "result": {"content": [{"type": "text", "text": "ok"}]}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolResult { duration_ms, .. } => assert_eq!(duration_ms, None),
            _ => panic!("应是 ToolResult"),
        }
    }

    // ---------------- P1-2 按行截断 ----------------

    #[test]
    fn 截断按行不按字符() {
        let s = "1\n2\n3\n4\n5\n6\n7\n8";
        let out = truncate_lines(s, 3, Keep::Head);
        assert!(out.starts_with("1\n2\n3"));
        assert!(out.contains("已隐藏 5 行"));
        // 行数不超就原样返回
        assert_eq!(truncate_lines("1\n2", 3, Keep::Head), "1\n2");
    }

    #[test]
    fn 留尾时保留最后几行() {
        let s = "1\n2\n3\n4\n5";
        let out = truncate_lines(s, 2, Keep::End);
        assert!(out.starts_with("4\n5"));
        assert!(out.contains("已隐藏 3 行"));
    }

    #[test]
    fn 截断不静默丢行_带提示() {
        let out = truncate_lines("a\nb\nc", 1, Keep::Head);
        assert!(out.contains("点开看全部"));
    }

    // ---------------- P1-1 图片块不能静默消失 ----------------

    #[test]
    fn 图片块产出占位行() {
        let result = json!({"content": [{"type": "image", "mimeType": "image/png"}]});
        let p = preview_of(&result, "screenshot");
        assert!(p.contains("[图片 image/png]"), "实际={p}");
    }

    #[test]
    fn 工具结果预览按工具分档() {
        assert_eq!(preview_shape("run_command"), (5, Keep::End));
        assert_eq!(preview_shape("grep"), (15, Keep::Head));
        assert_eq!(preview_shape("list_dir"), (20, Keep::Head));
        assert_eq!(preview_shape("whatever"), (10, Keep::Head));
    }

    // ---------------- P2-3 错误差异化提示 ----------------

    #[test]
    fn 回答被截断给专门提示() {
        let mut it = 0;
        let ev = json!({
            "type": "message_end",
            "message": {"stopReason": "length"}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::Status { text, retry } => {
                assert!(text.contains("截断"), "实际={text}");
                assert!(!retry);
            }
            _ => panic!("应是 Status"),
        }
    }

    #[test]
    fn 错误类型带出错误信息() {
        let mut it = 0;
        let ev = json!({
            "type": "message_end",
            "message": {"stopReason": "error", "errorMessage": "额度用完"}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::Status { text, .. } => assert!(text.contains("额度用完"), "实际={text}"),
            _ => panic!("应是 Status"),
        }
    }

    #[test]
    fn 改调工具不加提示() {
        // ⚠️ toolUse 是正常流程（模型转去调工具），加提示只会在时间线里多一行噪声
        let mut it = 0;
        assert!(translate(
            &json!({"type": "message_end", "message": {"stopReason": "toolUse"}}),
            &mut it
        )
        .is_none());
        assert!(translate(
            &json!({"type": "message_end", "message": {"stopReason": "stop"}}),
            &mut it
        )
        .is_none());
    }

    // ---------------- 既有行为 ----------------

    #[test]
    fn turn_start_递增轮次() {
        let mut it = 0;
        let p = translate(&json!({"type": "turn_start"}), &mut it).expect("应翻译");
        match p {
            Progress::Thinking { iteration } => assert_eq!(iteration, 1),
            _ => panic!("应是 Thinking"),
        }
        let p2 = translate(&json!({"type": "turn_start"}), &mut it).expect("应翻译");
        match p2 {
            Progress::Thinking { iteration } => assert_eq!(iteration, 2),
            _ => panic!("应是 Thinking"),
        }
    }

    #[test]
    fn 文本增量翻译成Delta() {
        let mut it = 0;
        let ev = json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "delta": "你好"}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::Delta { text, reasoning } => {
                assert_eq!(text.as_deref(), Some("你好"));
                assert!(reasoning.is_none());
            }
            _ => panic!("应是 Delta"),
        }
    }

    #[test]
    fn 思考增量走reasoning字段() {
        let mut it = 0;
        let ev = json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_delta", "delta": "在想"}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::Delta { text, reasoning } => {
                assert!(text.is_none());
                assert_eq!(reasoning.as_deref(), Some("在想"));
            }
            _ => panic!("应是 Delta"),
        }
    }

    #[test]
    fn 工具失败必须是ToolError而不是ToolResult() {
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_end",
            "toolName": "bash",
            "isError": true,
            "result": {"content": [{"type": "text", "text": "权限被拒"}]}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolError { name, message, .. } => {
                assert_eq!(name, "bash");
                assert!(message.contains("权限被拒"));
            }
            _ => panic!("必须是 ToolError —— 折进 ToolResult 就等于没人看得见"),
        }
    }

    #[test]
    fn 重试要带retry标记() {
        let mut it = 0;
        let ev = json!({
            "type": "auto_retry_start",
            "attempt": 1, "maxAttempts": 3,
            "errorMessage": "429 overloaded"
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::Status { text, retry } => {
                assert!(retry, "重试必须标 retry，否则前端不会显示");
                assert!(text.contains("1/3"));
            }
            _ => panic!("应是 Status"),
        }
    }

    #[test]
    fn 超长结果被截断() {
        let long = "x".repeat(5000);
        let mut it = 0;
        let ev = json!({
            "type": "tool_execution_end",
            "toolName": "read_file",
            "isError": false,
            "result": {"content": [{"type": "text", "text": long}]}
        });
        match translate(&ev, &mut it).expect("应翻译") {
            Progress::ToolResult { preview, .. } => {
                assert!(preview.chars().count() < 2000, "预览必须被截断");
                assert!(preview.contains("已截断"));
            }
            _ => panic!("应是 ToolResult"),
        }
    }

    #[test]
    fn 无关事件返回None() {
        let mut it = 0;
        assert!(translate(&json!({"type": "agent_start"}), &mut it).is_none());
        assert!(translate(&json!({"type": "queue_update"}), &mut it).is_none());
    }
}
