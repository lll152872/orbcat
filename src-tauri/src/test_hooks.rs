//! 模型层 mock 集成：不访问网络，验证 chat 命令在 mock 端点下的行为形态。
//!
//! 这里**不**起真实 HTTP server（避免测试绑端口/flaky）；
//! 协议级验证见 `tests/e2e/mock_openai.py` + `run_live_flow.py --probe-http`。

use crate::llm::{ChatMessage, MessageContent};

#[test]
fn chat_message_user_plain_content() {
    let m = ChatMessage::user("你好");
    match &m.content {
        Some(MessageContent::Text(t)) => assert_eq!(t, "你好"),
        other => panic!("期望纯文本 content，得到 {other:?}"),
    }
    assert_eq!(m.role, "user");
}

#[test]
fn system_prompt_includes_approved_memory_marker() {
    // 与真实 agent-data 路径无关：用临时目录验证读盘逻辑
    let dir = std::env::temp_dir().join(format!("fa_prompt_mem_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::write(dir.join("RULES.md"), "RULES_MARK_E2E").unwrap();
    std::fs::write(dir.join("SOUL.md"), "SOUL_MARK_E2E").unwrap();
    std::fs::write(dir.join("IDENTITY.md"), "ID_MARK_E2E").unwrap();
    std::fs::write(dir.join("USER.md"), "USER_MARK_E2E").unwrap();
    std::fs::write(
        dir.join("memory").join("MEMORY.md"),
        "# 长期记忆\n\n- 批准条目：称呼用户为「用户」MEM_MARK_E2E\n",
    )
    .unwrap();

    let p = crate::agent::build_system_prompt(&dir);
    assert!(p.contains("RULES_MARK_E2E"), "RULES 应注入");
    assert!(p.contains("MEM_MARK_E2E"), "已批准长期记忆应注入 system prompt");
    assert!(
        p.contains("记忆怎么用") && p.contains("remember"),
        "system prompt 应含记忆使用规则（含 remember 指引）"
    );
    // 2026-10-07：全局候选「宁多勿漏」的取向必须写明（有审批兜底，别让模型憋着不提）
    assert!(p.contains("宁多勿漏"), "应写明全局候选宁多勿漏的取向");
    // 批准记忆排在 RULES 之后
    let r = p.find("RULES_MARK_E2E").unwrap();
    let m = p.find("MEM_MARK_E2E").unwrap();
    assert!(r < m, "RULES 必须在长期记忆之前");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_memory_file_is_not_injected() {
    let dir = std::env::temp_dir().join(format!("fa_prompt_empty_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::write(dir.join("RULES.md"), "ONLY_RULES").unwrap();
    std::fs::write(dir.join("memory").join("MEMORY.md"), "   \n").unwrap();

    let p = crate::agent::build_system_prompt(&dir);
    assert!(p.contains("ONLY_RULES"));
    assert!(!p.contains("# 长期记忆"), "空 MEMORY.md 不应产生长期记忆段落");

    let _ = std::fs::remove_dir_all(&dir);
}
