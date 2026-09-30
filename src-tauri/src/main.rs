// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // ── CLI 冒烟口（2026-10 加）─────────────────────────────────────────
    //
    // 为什么给 GUI 程序留命令行入口：隐形桌面那套能力（跑命令 / 列窗口 / 截图）
    // 全在 Windows API 里，**没法在单测里验**（单测跑在 CI/无桌面环境里必挂）。
    // 留一个 CLI 口，就能在真机上一条命令验完：
    //
    //   orbcat.exe --bgdesk-run "powershell -c '...'"
    //   orbcat.exe --bgdesk-windows
    //
    // 这也是我验"命令在隐形桌面上到底跑没跑"的唯一手段 —— 比口头保证靠谱。
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--bgdesk-run") => {
            let cmd = args.get(1).cloned().unwrap_or_default();
            if cmd.trim().is_empty() {
                eprintln!("用法: orbcat.exe --bgdesk-run \"<命令行>\"");
                std::process::exit(2);
            }
            orbcat_lib::cli_bgdesk_run(&cmd);
            return;
        }
        Some("--bgdesk-windows") => {
            orbcat_lib::cli_bgdesk_windows();
            return;
        }
        Some("--bgdesk-shot") => {
            orbcat_lib::cli_bgdesk_shot(args.get(1).cloned(), args.get(2).cloned());
            return;
        }
        _ => {}
    }
    orbcat_lib::run()
}
