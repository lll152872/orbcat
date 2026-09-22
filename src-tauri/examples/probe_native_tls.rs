//! native-tls（Windows SChannel）对 volces 大 body 的表现
use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = "https://ark.cn-beijing.volces.com/api/plan/v3/chat/completions";
    let key = "ark-d2d7db33-5eeb-4a2d-8f3f-b3fae4da569e-3de34";
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(40))
        .build()
        .unwrap();

    for size_kb in [4usize, 16, 32, 50, 86] {
        let pad = "x".repeat(size_kb * 1024);
        let body = serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role":"system","content": pad},
                {"role":"user","content":"ping"}
            ],
            "max_tokens": 4
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let t0 = std::time::Instant::now();
        match client
            .post(url)
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await
        {
            Ok(r) => println!("native-tls {size_kb}KB OK {} {}ms", r.status(), t0.elapsed().as_millis()),
            Err(e) => {
                let mut chain = e.to_string();
                let mut s = std::error::Error::source(&e);
                while let Some(x) = s {
                    chain.push_str(&format!(" ← {x}"));
                    s = x.source();
                }
                println!("native-tls {size_kb}KB FAIL {}ms {chain}", t0.elapsed().as_millis());
            }
        }
    }
}
