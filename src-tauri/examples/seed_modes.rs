//! 一次性工具：把内置模式定义落到指定的 `agent-data/modes.json`。
//!
//! 用法：`cargo run --example seed_modes -- "<仓库根>/agent-data"`
//!
//! 为什么需要它：`modes.json` 正常由「初始化」写（`bootstrap::bootstrap_agent_data`），
//! 但**已经初始化过的**数据目录不会重跑初始化 —— 于是升级后用户看不到这份文件，
//! 也就无从发现自己可以改模式。这个 example 就是补写那一次。
//!
//! 铁律与 bootstrap 一致：**已存在则不覆盖**。

use std::path::PathBuf;

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("用法: cargo run --example seed_modes -- <agent-data 目录>");
            std::process::exit(2);
        })
        .into();

    if !dir.is_dir() {
        eprintln!("❌ 不是目录: {}", dir.display());
        std::process::exit(1);
    }

    let p = orbcat_lib_path(&dir);
    if p.exists() {
        println!("已存在，跳过（不覆盖用户改动）: {}", p.display());
        return;
    }
    match std::fs::write(&p, orbcat_modes_text()) {
        Ok(()) => println!("✅ 已写入 {}", p.display()),
        Err(e) => {
            eprintln!("❌ 写入 {} 失败: {e}", p.display());
            std::process::exit(1);
        }
    }
}

fn orbcat_lib_path(dir: &std::path::Path) -> PathBuf {
    dir.join("modes.json")
}

fn orbcat_modes_text() -> String {
    // 直接复用 crate 里的生成函数，保证与「初始化」写出来的一模一样
    orbcat_lib::seed_modes_text_for_example()
}
