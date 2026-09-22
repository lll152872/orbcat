//! 对比 rustls/http1/http2 对不同 body 体积的发送结果
use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = "https://ark.cn-beijing.volces.com/api/plan/v3/chat/completions";
    let key = "ark-d2d7db33-5eeb-4a2d-8f3f-b3fae4da569e-3de34";

    let mk = |http1: bool| {
        let b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(25));
        let b = if http1 { b.http1_only() } else { b };
        b.build().unwrap()
    };

    let c_h1 = mk(true);
    let c_def = mk(false);

    for size_kb in [4usize, 16, 32, 50, 64, 86, 128] {
        let pad = "x".repeat(size_kb * 1024);
        let body = serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role":"system","content": pad},
                {"role":"user","content":"ping"}
            ],
            "max_tokens": 4
        });
        probe(&c_def, url, key, &format!("default/{size_kb}KB"), &body).await;
        probe(&c_h1, url, key, &format!("http1/{size_kb}KB"), &body).await;
    }
}

async fn probe(client: &reqwest::Client, url: &str, key: &str, label: &str, body: &serde_json::Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let t0 = std::time::Instant::now();
    let res = client
        .post(url)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await;
    match res {
        Ok(r) => println!("{label} bytes OK {} {}ms", r.status(), t0.elapsed().as_millis()),
        Err(e) => {
            let mut chain = e.to_string();
            let mut s = std::error::Error::source(&e);
            while let Some(x) = s {
                chain.push_str(&format!(" ← {x}"));
                s = x.source();
            }
            println!("{label} bytes FAIL {}ms {chain}", t0.elapsed().as_millis());
        }
    }
}
