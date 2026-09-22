//! 隔离测火山：区分「上传耗时」和「等响应耗时」，并对比内容类型
use std::time::{Duration, Instant};

fn mk_body(pad_kind: &str, size_kb: usize) -> String {
    let unit = match pad_kind {
        "ascii" => "x".repeat(1024),
        "cjk" => "规则：所有文件操作必须走权限网关，越权会被拒绝。".repeat(64),
        "realish" => {
            let mut s = String::from("你是悬浮助手。\n# 行为红线\n");
            while s.len() < size_kb * 1024 {
                s.push_str("规则：文件操作走权限网关；危险命令硬阻断；改文件优先 edit_file。\n");
            }
            s
        }
        _ => "x".repeat(1024),
    };
    let pad: String = std::iter::repeat(unit)
        .take(size_kb)
        .collect::<String>()
        .chars()
        .take(size_kb * 1024)
        .collect();
    serde_json::json!({
        "model": "deepseek-v4.1-flash",
        "messages": [
            {"role":"system","content": pad},
            {"role":"user","content":"用一个词回答：1+1=?"}
        ],
        "max_tokens": 8,
        "stream": true
    })
    .to_string()
}

#[tokio::main]
async fn main() {
    let url = "https://ark.cn-beijing.volces.com/api/plan/v3/chat/completions";
    let key = "ark-d2d7db33-5eeb-4a2d-8f3f-b3fae4da569e-3de34";

    // 每次新建 client，避免连接池/会话状态干扰
    let cases: Vec<(&str, usize)> = vec![
        ("ascii", 2),
        ("ascii", 8),
        ("ascii", 16),
        ("ascii", 24),
        ("ascii", 32),
        ("cjk", 16),
        ("cjk", 32),
        ("realish", 16),
        ("realish", 32),
    ];

    for (kind, kb) in cases {
        let body = mk_body(kind, kb);
        let bytes = body.into_bytes();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .http1_only() // 变量控制：先固定 HTTP/1.1
            .build()
            .unwrap();

        let t0 = Instant::now();
        let send_fut = async {
            client
                .post(url)
                .bearer_auth(key)
                .header("content-type", "application/json")
                .body(bytes.clone())
                .send()
                .await
        };
        let resp = tokio::time::timeout(Duration::from_secs(90), send_fut).await;
        match resp {
            Err(_) => println!("{kind}/{kb}KB bytes={} TIMEOUT@90s (headers never returned)", bytes.len()),
            Ok(Err(e)) => {
                let mut chain = e.to_string();
                let mut s = std::error::Error::source(&e);
                while let Some(x) = s {
                    chain.push_str(&format!(" ← {x}"));
                    s = x.source();
                }
                println!(
                    "{kind}/{kb}KB bytes={} FAIL@{}ms {chain}",
                    bytes.len(),
                    t0.elapsed().as_millis()
                );
            }
            Ok(Ok(r)) => {
                let status = r.status();
                let headers_at = t0.elapsed().as_millis();
                // 读首包
                match r.bytes().await {
                    Ok(b) => println!(
                        "{kind}/{kb}KB bytes={} OK {status} headers@{headers_at}ms body@{}ms head={}",
                        bytes.len(),
                        t0.elapsed().as_millis(),
                        String::from_utf8_lossy(&b[..b.len().min(80)])
                    ),
                    Err(e) => println!(
                        "{kind}/{kb}KB bytes={} headers@{headers_at}ms but body read fail: {e}",
                        bytes.len()
                    ),
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // 对照：同一 32KB ascii 连打 3 次，看是否稳定失败
    println!("---- repeat ascii/32KB x3 ----");
    for i in 1..=3 {
        let body = mk_body("ascii", 32);
        let bytes = body.into_bytes();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        let t0 = Instant::now();
        match client
            .post(url)
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
        {
            Ok(r) => println!("repeat{i} OK {} {}ms", r.status(), t0.elapsed().as_millis()),
            Err(e) => {
                let mut chain = e.to_string();
                let mut s = std::error::Error::source(&e);
                while let Some(x) = s {
                    chain.push_str(&format!(" ← {x}"));
                    s = x.source();
                }
                println!("repeat{i} FAIL {}ms {chain}", t0.elapsed().as_millis());
            }
        }
    }
}
