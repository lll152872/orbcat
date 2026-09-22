//! 复现 float-agent 的 reqwest 调用：小请求 vs 大请求
use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = "https://ark.cn-beijing.volces.com/api/plan/v3/chat/completions";
    let key = "ark-d2d7db33-5eeb-4a2d-8f3f-b3fae4da569e-3de34";

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("build client");

    // small
    let small = serde_json::json!({
        "model": "deepseek-v4.1-flash",
        "messages": [{"role":"user","content":"test"}],
        "max_tokens": 8
    });
    probe(&client, url, key, "small", &small).await;

    // large system ~200k chars
    let big = "x".repeat(200_000);
    let large = serde_json::json!({
        "model": "deepseek-v4.1-flash",
        "messages": [
            {"role":"system","content": big},
            {"role":"user","content":"test"}
        ],
        "max_tokens": 8
    });
    probe(&client, url, key, "large-200k", &large).await;

    // tools + history-like
    let mut messages = vec![serde_json::json!({
        "role":"system",
        "content": "RULES ".repeat(5000)
    })];
    for i in 0..8 {
        messages.push(serde_json::json!({"role":"user","content": format!("问题{i} {}", "内容".repeat(150))}));
        messages.push(serde_json::json!({"role":"assistant","content": format!("回答{i} {}", "详细说明".repeat(300))}));
    }
    let tools: Vec<_> = (0..15)
        .map(|i| {
            serde_json::json!({
                "type":"function",
                "function":{
                    "name": format!("tool_{i}"),
                    "description": format!("{}{}", "读文件工具说明，包含权限网关与超时等。".repeat(20), i),
                    "parameters": {"type":"object","properties":{"path":{"type":"string"}}}
                }
            })
        })
        .collect();
    let heavy = serde_json::json!({
        "model": "deepseek-v4.1-flash",
        "messages": messages,
        "max_tokens": 8,
        "tools": tools,
        "tool_choice": "auto",
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    probe(&client, url, key, "heavy-stream", &heavy).await;

    // pure TLS TCP connect detail
    match client.get("https://ark.cn-beijing.volces.com/").send().await {
        Ok(r) => println!("GET / => {}", r.status()),
        Err(e) => {
            println!("GET / FAIL: {e}");
            let mut s = std::error::Error::source(&e);
            while let Some(x) = s {
                println!("  source: {x}");
                s = x.source();
            }
        }
    }
}

async fn probe(client: &reqwest::Client, url: &str, key: &str, label: &str, body: &serde_json::Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    println!("==== {label} bytes={} ====", bytes.len());
    let t0 = std::time::Instant::now();
    match client
        .post(url)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            println!(
                "OK {status} in {}ms body_head={}",
                t0.elapsed().as_millis(),
                text.chars().take(160).collect::<String>()
            );
        }
        Err(e) => {
            println!("FAIL in {}ms: {e}", t0.elapsed().as_millis());
            let mut s = std::error::Error::source(&e);
            while let Some(x) = s {
                println!("  source: {x}");
                s = x.source();
            }
        }
    }
}
