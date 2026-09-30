//! 后台桌面（隐形桌面）—— 「在用户看不见的地方跑东西」
//!
//! ## 这是什么
//!
//! 用 `CreateDesktopW` 在 `WinSta0` 里**再建一张桌面**。这张桌面不会被切换到
//! 屏幕上，所以跑在上面的 GUI 程序：
//!
//! - ✅ 用户**看不见**（不闪窗口、不抢焦点、不进 Alt-Tab）
//! - ✅ 照样**真实渲染**（不是隐藏窗口那种"停摆"状态）
//! - ✅ 我们**抓得到画面**（`PrintWindow` + `PW_RENDERFULLCONTENT`）
//! - ✅ 完整的 GUI 应用跑得起来（实测 Word/WPS COM 打开 docx → 导出 PDF）
//! - ❌ **合成键鼠进不去**（见下）
//!
//! ## ⚠️ 最重要的限制：没有合成输入
//!
//! 实测结论（探针脚本见 `tmp/_probe_input*.ps1`，报告见
//! `docs/02-BACKGROUND-MODE-FINDINGS.md`）：
//!
//! | 尝试 | 结果 |
//! |---|---|
//! | 本进程线程 attach 到目标线程 | ❌ `GetThreadDesktop` 就 `err=5 拒绝访问` |
//! | 驱动脚本也跑到那张桌面上再 attach | ✅ attach 成功，但 `SendInput` 返回 **0 事件** |
//! | 窗口可见 + 抢到前台再 `SendInput` | ❌ 事件发出去了，窗口收不到 |
//!
//! 也就是说**"看不见"和"戳得到"是同一套隔离机制的两面**。
//! 所以后台桌面**只负责"在哪儿跑 + 怎么看"**，输入必须走**应用自己的接口**
//! （COM / CLI 无头参数 / UIA Pattern）。遇到只能靠点界面的程序，
//! 这里要**明说不支持**，让用户切回可见模式 —— 不要假装能做（用户拍板）。
//!
//! ## 抓图为什么必须带 `PW_RENDERFULLCONTENT`
//!
//! 三种抓法的实测对比（同一张隐形桌面上的同一个窗口）：
//!
//! | 方法 | 返回值 | 非零像素 |
//! |---|---|---|
//! | `PrintWindow(hwnd, dc, 0)` | `TRUE` | **0 / 166400（全黑）** |
//! | `PrintWindow(hwnd, dc, 2)` | `TRUE` | **158530 / 166400 ✅** |
//! | `BitBlt(GetWindowDC(hwnd), …)` | `FALSE` | 0 / 166400 |
//!
//! ⚠️ 不带 flag 的 `PrintWindow` **返回成功却抓到全黑** —— 这是最容易
//! 把"方案不可行"误判出来的坑，所以这里把它写成常量并有断言测试。

#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Win32 FFI
// ---------------------------------------------------------------------------

type Handle = *mut c_void;
type Hdc = *mut c_void;
type Hbitmap = *mut c_void;
type Hgdiobj = *mut c_void;

/// 桌面访问权限（= `GENERIC_ALL`，`CreateDesktop` 的 `dwDesiredAccess`）
const DESKTOP_ALL_ACCESS: u32 = 0x000F_01FF;

/// `PrintWindow` 的 `PW_RENDERFULLCONTENT`。
///
/// 见模块头「抓图为什么必须带它」：不带会**返回成功 + 抓到全黑**。
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: Handle,
    h_std_output: Handle,
    h_std_error: Handle,
}

impl Default for StartupInfoW {
    fn default() -> Self {
        Self {
            cb: std::mem::size_of::<StartupInfoW>() as u32,
            lp_reserved: std::ptr::null_mut(),
            lp_desktop: std::ptr::null_mut(),
            lp_title: std::ptr::null_mut(),
            dw_x: 0,
            dw_y: 0,
            dw_x_size: 0,
            dw_y_size: 0,
            dw_x_count_chars: 0,
            dw_y_count_chars: 0,
            dw_fill_attribute: 0,
            dw_flags: 0,
            w_show_window: 0,
            cb_reserved2: 0,
            lp_reserved2: std::ptr::null_mut(),
            h_std_input: std::ptr::null_mut(),
            h_std_output: std::ptr::null_mut(),
            h_std_error: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
struct ProcessInformation {
    h_process: Handle,
    h_thread: Handle,
    dw_process_id: u32,
    dw_thread_id: u32,
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 3],
}

#[link(name = "user32")]
extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *mut c_void,
        devmode: *mut c_void,
        flags: u32,
        access: u32,
        sa: *mut c_void,
    ) -> Handle;
    fn CloseDesktop(h: Handle) -> i32;
    fn EnumDesktopWindows(desktop: Handle, cb: extern "system" fn(Handle, isize) -> i32, p: isize)
        -> i32;
    fn GetWindowTextW(hwnd: Handle, buf: *mut u16, max: i32) -> i32;
    fn GetWindowRect(hwnd: Handle, r: *mut Rect) -> i32;
    fn IsWindowVisible(hwnd: Handle) -> i32;
    fn PrintWindow(hwnd: Handle, dc: Hdc, flags: u32) -> i32;
    fn GetWindowDC(hwnd: Handle) -> Hdc;
    fn ReleaseDC(hwnd: Handle, dc: Hdc) -> i32;
    fn PostMessageW(hwnd: Handle, msg: u32, w: usize, l: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: Handle, pid: *mut u32) -> u32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: Hdc) -> Hdc;
    fn CreateCompatibleBitmap(dc: Hdc, w: i32, h: i32) -> Hbitmap;
    fn SelectObject(dc: Hdc, obj: Hgdiobj) -> Hgdiobj;
    fn DeleteObject(obj: Hgdiobj) -> i32;
    fn DeleteDC(dc: Hdc) -> i32;
    fn GetDIBits(
        dc: Hdc,
        bmp: Hbitmap,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        bmi: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateProcessW(
        app: *const u16,
        cmdline: *mut u16,
        pa: *mut c_void,
        ta: *mut c_void,
        inherit: i32,
        flags: u32,
        env: *mut c_void,
        cwd: *const u16,
        si: *mut StartupInfoW,
        pi: *mut ProcessInformation,
    ) -> i32;
    fn TerminateProcess(h: Handle, code: u32) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn GetLastError() -> u32;
    fn CreatePipe(read: *mut Handle, write: *mut Handle, sa: *mut SecurityAttributes, size: u32)
        -> i32;
    fn SetHandleInformation(h: Handle, mask: u32, flags: u32) -> i32;
    fn ReadFile(
        h: Handle,
        buf: *mut u8,
        to_read: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
    // 只被 `kill_process` 用；那个函数目前未接线，故一并标 allow（见其注释）。
    #[allow(dead_code)]
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// 桌面对象
// ---------------------------------------------------------------------------

/// 一张隐形桌面。`Drop` 时关掉句柄（进程退出也会自动回收，这里只是及时一点）。
pub struct BackgroundDesk {
    handle: Handle,
    name: String,
}

// 句柄本身是内核对象、跨线程可用；`CloseDesktop` 只在 `Drop` 里调一次。
unsafe impl Send for BackgroundDesk {}
unsafe impl Sync for BackgroundDesk {}

impl BackgroundDesk {
    /// 建一张新桌面。名字为空时自动生成一个唯一名。
    pub fn create(name: Option<&str>) -> Result<Self, String> {
        let name = match name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => format!("orbcat_bg_{}", std::process::id()),
        };
        let wname = wide(&name);
        let handle = unsafe {
            CreateDesktopW(
                wname.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                DESKTOP_ALL_ACCESS,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            return Err(format!(
                "创建隐形桌面失败（GetLastError={err}，桌面名 {name}）"
            ));
        }
        Ok(Self { handle, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 在**这张桌面上**启动一个进程。
    ///
    /// ⚠️ `cmdline` 必须是可写的（`CreateProcessW` 会就地改它）—— 这个要求
    /// 被 Rust 的 `*mut u16` 表达出来了，别改成 `&[u16]`。
    ///
    /// ⚠️ 命令行里**不要塞带引号/中文的参数**：实测这一层会把它们吃掉
    /// （探针为此连挂三次）。要传数据就写进临时文件、或用环境块。
    // 未接线：目前后台执行走 `run_blocking`（它自带管道读取 + 超时杀树）。
    // 这个"只起进程、不等结果"的原语留给后续「后台任务常驻」用，先保留不删。
    #[allow(dead_code)]
    pub fn spawn(&self, exe: &str, args: &[&str], cwd: Option<&str>) -> Result<u32, String> {
        let mut cmdline: Vec<u16> = Vec::new();
        cmdline.extend(wide(&format!("\"{exe}\"")));
        cmdline.pop(); // 去掉 wide() 补的 NUL，后面还要接参数
        for a in args {
            cmdline.push(' ' as u16);
            // 参数一律加引号（路径带空格是常态）
            cmdline.extend(format!("\"{a}\"").encode_utf16());
        }
        cmdline.push(0);

        let wexe = wide(exe);
        let wdesk = wide(&self.name);
        let wcwd = cwd.map(wide);

        let mut si = StartupInfoW::default();
        // ⭐ 关键一行：子进程的窗口都建在这张隐形桌面上
        si.lp_desktop = wdesk.as_ptr() as *mut u16;
        let mut pi = ProcessInformation {
            h_process: std::ptr::null_mut(),
            h_thread: std::ptr::null_mut(),
            dw_process_id: 0,
            dw_thread_id: 0,
        };

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ok = unsafe {
            CreateProcessW(
                wexe.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                CREATE_NO_WINDOW,
                std::ptr::null_mut(),
                wcwd.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                &mut si,
                &mut pi,
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            return Err(format!("在隐形桌面上启动 {exe} 失败（GetLastError={err}）"));
        }
        unsafe {
            CloseHandle(pi.h_thread);
            CloseHandle(pi.h_process);
        }
        Ok(pi.dw_process_id)
    }

    /// 枚举这张桌面上的顶层窗口（`(hwnd, 标题, 尺寸, 是否可见)`）。
    pub fn windows(&self) -> Vec<BgWindow> {
        let mut out: Vec<BgWindow> = Vec::new();
        let ptr = &mut out as *mut Vec<BgWindow> as isize;
        unsafe {
            EnumDesktopWindows(self.handle, collect_window, ptr);
        }
        out
    }
}

impl Drop for BackgroundDesk {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseDesktop(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }
}

/// 隐形桌面上的一个窗口
#[derive(Debug, Clone)]
pub struct BgWindow {
    pub hwnd: usize,
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub visible: bool,
    pub pid: u32,
}

extern "system" fn collect_window(hwnd: Handle, param: isize) -> i32 {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return 1; // 没标题的（IME/工具提示之类）跳过
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        let mut r = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        GetWindowRect(hwnd, &mut r);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let list = &mut *(param as *mut Vec<BgWindow>);
        list.push(BgWindow {
            hwnd: hwnd as usize,
            title,
            width: r.right - r.left,
            height: r.bottom - r.top,
            visible: IsWindowVisible(hwnd) != 0,
            pid,
        });
        1 // 继续枚举
    }
}

/// 抓一个窗口的画面。
///
/// ⚠️ **必须把 `PW_RENDERFULLCONTENT` 传进 `PrintWindow`** —— 见模块头。
/// 不带的话：`PrintWindow` 返回 1（成功）、`GetDIBits` 也成功，但整张图是黑的。
///
/// 返回 RGBA 图。窗口尺寸为 0 时返回 `Err`（最小化的窗口没有可抓的表面）。
pub fn capture_window(hwnd: usize) -> Result<image::RgbaImage, String> {
    let h = hwnd as Handle;
    unsafe {
        let mut r = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(h, &mut r) == 0 {
            return Err("GetWindowRect 失败".into());
        }
        let (w, hgt) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || hgt <= 0 {
            return Err(format!("窗口尺寸非法：{w}x{hgt}（最小化/未就绪的窗口抓不到）"));
        }

        let wdc = GetWindowDC(h);
        if wdc.is_null() {
            return Err("GetWindowDC 失败".into());
        }
        let mdc = CreateCompatibleDC(wdc);
        let bmp = CreateCompatibleBitmap(wdc, w, hgt);
        if mdc.is_null() || bmp.is_null() {
            ReleaseDC(h, wdc);
            return Err("创建兼容 DC/Bitmap 失败".into());
        }
        let old = SelectObject(mdc, bmp);

        // ⚠️ 这里刻意写成一行、参数顺序贴着头文件注释：
        //    PrintWindow(hwnd, hdc, flags) —— 第一个参数是**窗口句柄**，不是 DC。
        //    传反了会"返回 0"或"返回成功但全黑"，所以不接受任何"顺手简化"。
        //    `hdc` 必须是内存 DC（`mdc`），窗口 DC 不能当目标。
        let ok = PrintWindow(h, mdc, PW_RENDERFULLCONTENT);

        let mut bmi = BitmapInfo {
            bmi_header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: w,
                bi_height: -hgt, // top-down
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: BI_RGB,
                bi_size_image: 0,
                bi_x_pels_per_meter: 0,
                bi_y_pels_per_meter: 0,
                bi_clr_used: 0,
                bi_clr_important: 0,
            },
            bmi_colors: [0; 3],
        };

        let mut buf = vec![0u8; (w as usize) * (hgt as usize) * 4];
        let got = GetDIBits(
            mdc,
            bmp,
            0,
            hgt as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(mdc, old);
        DeleteObject(bmp);
        DeleteDC(mdc);
        ReleaseDC(h, wdc);

        if ok == 0 {
            return Err("PrintWindow 失败（窗口可能已销毁）".into());
        }
        if got == 0 {
            return Err("GetDIBits 失败".into());
        }

        // 全黑检测：不带 PW_RENDERFULLCONTENT 时正是"成功但全黑"，
        // 这里兜一层，免得把一张纯黑图当成"截图成功"喂给模型。
        let non_zero = buf
            .chunks_exact(4)
            .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
            .count();
        if non_zero == 0 {
            return Err(
                "抓到的画面全黑（PrintWindow 未生效 / 窗口停摆）；\
                 确认用了 PW_RENDERFULLCONTENT"
                    .into(),
            );
        }

        // BGRA → RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        image::RgbaImage::from_raw(w as u32, hgt as u32, buf)
            .ok_or_else(|| "构造图像失败".to_string())
    }
}

/// 关掉一个窗口（`WM_CLOSE`，让应用自己走正常退出流程）
pub fn close_window(hwnd: usize) -> bool {
    const WM_CLOSE: u32 = 0x0010;
    unsafe { PostMessageW(hwnd as Handle, WM_CLOSE, 0, 0) != 0 }
}

/// 杀掉一个进程（`WM_CLOSE` 不理会时的兜底）
///
/// 未接线：`run_blocking` 超时后自己 `TerminateProcess`，暂不需要外部再杀一次。
/// 留给后续「面板上手动终止某个后台任务」用。
#[allow(dead_code)]
pub fn kill_process(pid: u32) -> bool {
    // `OpenProcess` 声明在文件顶部的 kernel32 块里（那里是唯一的 FFI 出口，
    // 别在这里再写一份 —— 重复声明会报 E0428，而且两处容易漂移）
    const PROCESS_TERMINATE: u32 = 0x0001;
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

// ---------------------------------------------------------------------------
// 当前进程的"后台执行模式"（进程级开关，不落盘）
// ---------------------------------------------------------------------------

/// 进程级：命令是否跑在隐形桌面上。
///
/// 为什么不落进 `AgentSettings`：这是一次性会话开关（用户明确拍板"后台执行
/// 独立于 agent 模式"），落盘会让人下次开机莫名其妙"命令都看不见了"。
static BG_ENABLED: OnceLock<Mutex<bool>> = OnceLock::new();

fn enabled_cell() -> &'static Mutex<bool> {
    BG_ENABLED.get_or_init(|| Mutex::new(false))
}

pub fn is_enabled() -> bool {
    *enabled_cell().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_enabled(on: bool) {
    *enabled_cell().lock().unwrap_or_else(|e| e.into_inner()) = on;
}

/// 惰性持有的那张桌面：整个进程共用一张。
///
/// ⚠️ 用 `OnceLock<BackgroundDesk>` 而不是 `Mutex<Option<...>>` 再 `Arc::from_raw`：
/// 后者是**未定义行为**（`from_raw` 会接管所有权，跟原 `Option` 里的那份一起
/// 在 Drop 时双重释放）。这里靠 `OnceLock` 的借用语义把"活到进程结束"表达清楚。
static DESK: OnceLock<Result<BackgroundDesk, String>> = OnceLock::new();

/// 拿到（必要时创建）共用的隐形桌面。
///
/// 只借出不复制：`BackgroundDesk` 的 `Drop` 会 `CloseDesktop`，
/// 谁都不能拿它的所有权（拿走了别人就用不成）。
pub fn shared_desk() -> Result<&'static BackgroundDesk, String> {
    let cell = DESK.get_or_init(|| BackgroundDesk::create(Some("orbcat_bg")));
    match cell {
        Ok(d) => Ok(d),
        Err(e) => Err(e.clone()),
    }
}

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

/// 管道读端**不可继承**（只给父进程），写端可继承（给子进程当 stdout/stderr）
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
// 未接线：`run_blocking` 用 `WaitForSingleObject` 后只比对 `WAIT_TIMEOUT`，
// 成功路径不再区分 WAIT_OBJECT_0 / WAIT_ABANDONED。留给后续细化用。
#[allow(dead_code)]
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 把一段 PowerShell 命令写成临时 `.ps1`，返回 `(路径, 要传给 pwsh 的参数)`。
///
/// ## 为什么不直接 `-Command "<命令>"`
///
/// 实测踩坑（2026-10）：`CreateProcessW` 的 `lpCommandLine` 是**单个字符串**，
/// 它自己按 MSVCRT 规则拆参数 —— 命令里嵌套的双引号会被吃掉。
/// 实际现象：
///
/// ```text
///   传入:  [Guid]::NewGuid().ToString() + " | PID=" + $PID
///   子进程收到: [Guid]::NewGuid().ToString() +  | PID= + $PID
///                                        ^^^^^^^^ 引号没了
///   → ParserError: You must provide a value expression following the '+' operator.
/// ```
///
/// 这个坑极难从报错反推（看起来像"用户的命令写错了"）。
/// 写成 `.ps1` 再 `-File` 就彻底绕开：命令行里只有一个**文件路径**，
/// 没有需要转义的内容。
///
/// ## 顺带解决编码
/// 脚本头部先设 `OutputEncoding` 为 UTF-8 —— 与 `shell::wrap_utf8` 同一手法。
/// 不设的话中文输出是 GBK，收回来是乱码（实测 `时刻` → `ʱ��`）。
fn write_temp_script(cmd: &str) -> Result<(PathBuf, Vec<String>), String> {
    let dir = std::env::temp_dir().join("orbcat-bgdesk");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录 {} 失败: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("cmd-{stamp}.ps1"));

    // 与 shell.rs 的 wrap_utf8 同源：先钉住输出编码，再跑用户命令
    let body = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
         $OutputEncoding=[System.Text.Encoding]::UTF8; \
         {cmd}"
    );
    std::fs::write(&path, body).map_err(|e| format!("写临时脚本 {} 失败: {e}", path.display()))?;

    let args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        // -ExecutionPolicy Bypass：临时目录的脚本默认可能被策略拦下
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-File".to_string(),
        path.to_string_lossy().to_string(),
    ];
    Ok((path, args))
}

/// 临时脚本的清理守卫：无论正常返回还是提前 `?` 退出，都把文件删掉。
struct TempScriptGuard(PathBuf);

impl Drop for TempScriptGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 在隐形桌面上跑一条 **PowerShell 命令**，回收 stdout/stderr 与退出码。
///
/// ## 为什么要手写这套，而不是用 `std::process::Command`
/// `Command` 没有"指定 `lpDesktop`"这个能力 —— 而"跑在隐形桌面上"正是本模块的
/// 全部意义。所以这里直接 `CreateProcessW`，顺手用匿名管道把两路输出接回来
/// （与 `shell.rs` 的 `run_powershell_tick` 同一个目标，只是那条路走的是 tokio）。
///
/// ## 命令为什么落成临时 `.ps1` 而不是 `-Command "…"`
/// 见 [`write_temp_script`]：`CreateProcessW` 会吃掉命令里嵌套的双引号，
/// 表现为莫名其妙的 `ParserError`。落成脚本文件就绕开了整个转义问题。
///
/// ## 阻塞式，别在 async 上下文里直接调
/// 它内部 `ReadFile` + `WaitForSingleObject` 都是阻塞调用。调用方要用
/// `tokio::task::spawn_blocking` 包起来（见 `shell::run_on_desktop`）。
///
/// ## 超时处理
/// 到点先 `TerminateProcess`，再 `WM_CLOSE` 桌面上的窗口 —— 顺序不能反：
/// 先关窗口的话，某些应用会弹"确认关闭"对话框，反而把管道一直占着。
pub fn run_blocking(
    exe: &str,
    cmd: &str,
    cwd: Option<&str>,
    timeout: std::time::Duration,
) -> Result<BgCmdOutput, String> {
    let desk = shared_desk()?;
    let (script_path, args) = write_temp_script(cmd)?;
    // 脚本用完就删（失败也不影响结果，只是留个临时文件）。
    // 绑定到具名变量而不是 `_`：`_` 会**立刻**析构，文件在子进程还没读之前就被删了。
    let _cleanup = TempScriptGuard(script_path);

    // ---- 建两根管道（stdout / stderr）----
    let mut out_r: Handle = std::ptr::null_mut();
    let mut out_w: Handle = std::ptr::null_mut();
    let mut err_r: Handle = std::ptr::null_mut();
    let mut err_w: Handle = std::ptr::null_mut();
    let mut sa = SecurityAttributes {
        n_length: std::mem::size_of::<SecurityAttributes>() as u32,
        lp_security_descriptor: std::ptr::null_mut(),
        b_inherit_handle: 1, // 子进程要能继承写端
    };
    unsafe {
        if CreatePipe(&mut out_r, &mut out_w, &mut sa, 0) == 0 {
            return Err(format!("CreatePipe(stdout) 失败：{}", GetLastError()));
        }
        if CreatePipe(&mut err_r, &mut err_w, &mut sa, 0) == 0 {
            CloseHandle(out_r);
            CloseHandle(out_w);
            return Err(format!("CreatePipe(stderr) 失败：{}", GetLastError()));
        }
        // 读端留在父进程、不给子进程（否则子进程也握着读端，管道永远不 EOF）
        SetHandleInformation(out_r, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(err_r, HANDLE_FLAG_INHERIT, 0);
    }

    // ---- 组命令行 ----
    // 参数里只有**文件路径**（见 write_temp_script），没有需要转义的内容
    let mut cmdline: Vec<u16> = Vec::new();
    cmdline.extend(format!("\"{exe}\"").encode_utf16());
    for a in &args {
        cmdline.push(' ' as u16);
        cmdline.extend(format!("\"{a}\"").encode_utf16());
    }
    cmdline.push(0);
    let wexe = wide(exe);
    let wdesk = wide(desk.name());
    let wcwd = cwd.map(wide);

    let mut si = StartupInfoW::default();
    si.lp_desktop = wdesk.as_ptr() as *mut u16;
    si.dw_flags = STARTF_USESTDHANDLES;
    si.h_std_output = out_w;
    si.h_std_error = err_w;
    // stdin 给 null：命令不该等输入（对齐 shell.rs 的 Stdio::null）
    si.h_std_input = std::ptr::null_mut();

    let mut pi = ProcessInformation {
        h_process: std::ptr::null_mut(),
        h_thread: std::ptr::null_mut(),
        dw_process_id: 0,
        dw_thread_id: 0,
    };

    let ok = unsafe {
        CreateProcessW(
            wexe.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1, // inherit handles
            CREATE_NO_WINDOW,
            std::ptr::null_mut(),
            wcwd.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            &mut si,
            &mut pi,
        )
    };
    // 无论成败，父进程都要放掉写端 —— 留着的话读端永远等不到 EOF
    unsafe {
        CloseHandle(out_w);
        CloseHandle(err_w);
    }
    if ok == 0 {
        let err = unsafe { GetLastError() };
        unsafe {
            CloseHandle(out_r);
            CloseHandle(err_r);
        }
        return Err(format!(
            "在隐形桌面上启动失败（GetLastError={err}）：{exe} {}",
            args.join(" ")
        ));
    }

    // ---- 读两路输出 ----
    // 顺序很关键：**先把输出读干净再 wait**。
    // 反过来的话，输出超过管道缓冲（默认 64KB）时子进程会卡在写管道上，
    // 而我们在等它退出 → 双向死锁（shell.rs 顶部那条硬规矩同一个道理）。
    //
    // 读要放到独立线程：`ReadFile` 是阻塞的，两条管道得同时读，
    // 在主线程顺序读会在"另一条管道写满"时互相卡住。
    //
    // ⚠️ 必须先包成 `PipeHandle`（`Send`）**再** move 进闭包：
    //    直接在闭包里 `PipeHandle(out_r)` 的话，闭包捕获的仍是裸指针 `out_r`
    //    → 报 `*mut c_void cannot be sent between threads safely`。
    let out_pipe = PipeHandle(out_r);
    let err_pipe = PipeHandle(err_r);
    let out_thread = std::thread::spawn(move || read_all(out_pipe));
    let err_thread = std::thread::spawn(move || read_all(err_pipe));

    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let waited = unsafe { WaitForSingleObject(pi.h_process, ms) };
    let timed_out = waited == WAIT_TIMEOUT;

    let finished = std::time::Instant::now();
    if timed_out {
        unsafe {
            TerminateProcess(pi.h_process, 1);
        }
        // 顺手把这张桌面上的窗口也关掉：`TerminateProcess` 只杀主进程，
        // 有些应用（Office/WPS）会留一个常驻的托盘进程。
        for w in desk.windows() {
            close_window(w.hwnd);
        }
    }

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();

    let mut code: u32 = 0;
    unsafe {
        GetExitCodeProcess(pi.h_process, &mut code);
        CloseHandle(pi.h_process);
        CloseHandle(pi.h_thread);
    }
    let _ = finished;

    Ok(BgCmdOutput {
        pid: pi.dw_process_id,
        stdout,
        stderr,
        exit_code: if timed_out { None } else { Some(code as i32) },
        timed_out,
        duration: finished.elapsed(),
    })
}

/// 管道读端的句柄包装。
///
/// 为什么需要它：`*mut c_void`（裸指针）不实现 `Send`，而读管道必须放到
/// 独立线程里去（两条管道不能在同一线程里顺序阻塞读 —— 会在"另一条写满"时死锁）。
///
/// # Safety
/// 每个 `PipeHandle` 独占一个句柄：只被 `read_all` 用一次、读完立刻 `CloseHandle`，
/// 不存在两个线程同时操作同一个句柄的情况。
struct PipeHandle(Handle);
unsafe impl Send for PipeHandle {}

/// 读干一个管道，直到 EOF 或出错
fn read_all(h: PipeHandle) -> String {
    let h = h.0;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || got == 0 {
            break;
        }
        // 上限与 `shell.rs` 一致（64KB），免得一条命令刷爆内存
        let room = 64 * 1024usize;
        if out.len() < room {
            let take = ((room - out.len()).min(got as usize)) as usize;
            out.extend_from_slice(&buf[..take]);
        }
    }
    unsafe {
        CloseHandle(h);
    }
    // 中文路径/中文输出在 Windows 下多为 GBK，这里交给调用方按需转；
    // 与 shell.rs 走 UTF-8 包装脚本的口径不同，所以标注在文档里。
    String::from_utf8_lossy(&out).to_string()
}

/// 隐形桌面上一次命令执行的结果
#[derive(Debug, Clone)]
pub struct BgCmdOutput {
    pub pid: u32,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// 从 `WaitForSingleObject` 返回后到收完管道的墙钟耗时。
    ///
    /// 未接线：目前没有任何调用方读它（`shell.rs` 自己算耗时）。
    /// 保留是因为它是"这条命令到底跑了多久"的唯一权威来源，
    /// 后续要做「后台任务耗时统计」时不必回头再改 FFI 层。
    #[allow(dead_code)]
    pub duration: std::time::Duration,
}

/// 给 `run_command` 用：隐形桌面上的命令也得有个工作目录
///
/// 未接线：当前调用方直接把自己的 `cwd` 传进 `run_blocking`，
/// 这个函数是留给「隐形桌面需要独立 cwd 策略」的接缝。
#[allow(dead_code)]
pub fn bg_cwd(fallback: &std::path::Path) -> PathBuf {
    fallback.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PW_RENDERFULLCONTENT` 必须 == 2。这条测试存在的唯一理由是：
    /// 哪天有人"顺手"把它改成 0（或不带 flag 的 PrintWindow），
    /// 表现是**调用成功 + 全黑图**，没有报错、极难发现。
    #[test]
    fn pw_render_full_content_flag_is_two() {
        assert_eq!(PW_RENDERFULLCONTENT, 0x2);
    }

    /// `slash_variant` 的同类：这里锁"窗口尺寸非法要报错"而不是返回空图
    #[test]
    fn capture_rejects_bogus_handle() {
        let r = capture_window(0);
        assert!(r.is_err(), "空句柄必须报错，不能返回空图：{r:?}");
    }

    /// 开关是进程级的，读回要一致
    #[test]
    fn enabled_toggle_round_trips() {
        let before = is_enabled();
        set_enabled(true);
        assert!(is_enabled());
        set_enabled(false);
        assert!(!is_enabled());
        set_enabled(before);
    }
}
