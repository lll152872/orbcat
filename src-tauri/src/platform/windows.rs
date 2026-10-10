//! Windows 原生窗口样式处理。
//!
//! 核心目标：让悬浮球**点击时不抢焦点**（`WS_EX_NOACTIVATE`）。
//!
//! 为什么这很重要：
//!   悬浮球浮在别的应用上面。如果点击它会把焦点从用户正在用的应用抢走，
//!   那么"选中文本 → 点悬浮球问一句 → 回到原应用继续写"这个流程就断了。
//!   加了 `WS_EX_NOACTIVATE` 后，点击悬浮球不会改变前台窗口。
//!
//! 但展开成面板（要输入文字）时必须**取消**这个样式，否则输入框拿不到焦点。

#![cfg(windows)]

use std::ffi::c_void;

pub type Hwnd = *mut c_void;

const GWL_EXSTYLE: i32 = -20;

const WS_EX_NOACTIVATE: isize = 0x0800_0000;
/// 工具窗口：不出现在 Alt+Tab 里
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;

const HWND_TOPMOST: Hwnd = -1isize as Hwnd;

const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;

/// 查询进程完整路径只需要这个权限（Vista+，普通用户即可）
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[link(name = "user32")]
extern "system" {
    fn GetWindowLongPtrW(hwnd: Hwnd, nindex: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: Hwnd, nindex: i32, dwnewlong: isize) -> isize;
    fn SetWindowPos(
        hwnd: Hwnd,
        hwnd_insert_after: Hwnd,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn GetForegroundWindow() -> Hwnd;
    fn GetWindowTextW(hwnd: Hwnd, lp_string: *mut u16, n_max_count: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, lpdw_process_id: *mut u32) -> u32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn BringWindowToTop(hwnd: Hwnd) -> i32;
    fn SetFocus(hwnd: Hwnd) -> Hwnd;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, f_attach: i32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Hwnd;
    fn CloseHandle(handle: Hwnd) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn QueryFullProcessImageNameW(
        process: Hwnd,
        flags: u32,
        exe_name: *mut u16,
        size: *mut u32,
    ) -> i32;
}

const HKEY_CURRENT_USER: Hwnd = 0x8000_0001usize as Hwnd;
/// 只接受 REG_SZ
const RRF_RT_REG_SZ: u32 = 0x02;

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
    hkey: Hwnd,
    lp_subkey: *const u16,
    lp_value: *const u16,
    dw_flags: u32,
    pdw_type: *mut u32,
    pv_data: *mut c_void,
    pcb_data: *mut u32,
    ) -> i32;
}

/// 写 HKCU 下的 REG_SZ 值。成功返回 Ok。
pub fn write_hkcu_sz(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let data_w: Vec<u16> = data.encode_utf16().chain(std::iter::once(0)).collect();
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            REG_SZ,
            data_w.as_ptr() as *const c_void,
            (data_w.len() * 2) as u32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("RegSetKeyValueW 失败（错误码 {rc}）"))
    }
}

/// 删除 HKCU 下的某个值。值本来就不存在也算成功（幂等）。
pub fn delete_hkcu_value(subkey: &str, value: &str) -> Result<(), String> {
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let rc = unsafe { RegDeleteValueW(HKEY_CURRENT_USER, subkey_w.as_ptr(), value_w.as_ptr()) };
    // 2 = ERROR_FILE_NOT_FOUND（值不存在）
    if rc == 0 || rc == 2 {
        Ok(())
    } else {
        Err(format!("RegDeleteValueW 失败（错误码 {rc}）"))
    }
}

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(
        hkey: Hwnd,
        lp_subkey: *const u16,
        lp_value_name: *const u16,
        dw_type: u32,
        lp_data: *const c_void,
        cb_data: u32,
    ) -> i32;
    fn RegDeleteValueW(hkey: Hwnd, lp_subkey: *const u16, lp_value_name: *const u16) -> i32;
}

const REG_SZ: u32 = 1;

/// 读 HKCU 下某个 REG_SZ 注册表值。存在返回 Some(内容)，不存在/失败返回 None。
///
/// 为什么不用 `reg query` 子进程：GUI 进程（windows_subsystem=windows）同步等待
/// console 子进程，在杀软钩住 reg.exe 或控制台句柄异常时会挂死 —— 曾把设置页
/// 卡成永久"加载中"。API 调用是微秒级、无子进程、无这个风险。
pub fn read_hkcu_sz(subkey: &str, value: &str) -> Option<String> {
    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let mut ty: u32 = 0;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            value_w.as_ptr(),
            RRF_RT_REG_SZ,
            &mut ty,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if rc != 0 {
        return None; // ERROR_FILE_NOT_FOUND 等：值不存在
    }
    let n = (size as usize / 2).min(buf.len());
    // 去掉结尾 NUL
    let end = buf[..n].iter().position(|&u| u == 0).unwrap_or(n);
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// 从 `.lnk` 快捷方式字节里解析目标完整路径（[MS-SHLLINK] 格式）。
///
/// 为什么纯手解不引 crate：只需要 LinkInfo 的 LocalBasePath / Unicode 段，
/// 20 行字节读取搞定；为一个功能加 `lnk` 依赖不值得。
/// 解析失败（网络路径 / 特殊 folder 目标）返回 None，调用方自行兜底。
pub fn lnk_target(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 80 || bytes[..4] != [0x4C, 0, 0, 0] {
        return None; // 不是 .lnk 的 magic
    }
    let flags = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
    let mut off = 76usize;
    if flags & 0x1 != 0 {
        // 有 ItemID 列表，先跳过
        let idsize = u16::from_le_bytes(bytes[76..78].try_into().ok()?) as usize;
        off = 78 + idsize;
    }
    if flags & 0x2 == 0 {
        return None; // 没有 LinkInfo
    }
    let li = off;
    if li + 28 > bytes.len() {
        return None;
    }
    let li_len = u32::from_le_bytes(bytes[li..li + 4].try_into().ok()?) as usize;
    let li_hdr = u32::from_le_bytes(bytes[li + 4..li + 8].try_into().ok()?) as usize;
    let li_flags = u32::from_le_bytes(bytes[li + 8..li + 12].try_into().ok()?);
    if li_flags & 0x1 == 0 {
        return None; // VolumeAndLocalBasePath 位没开
    }
    let lbp = u32::from_le_bytes(bytes[li + 16..li + 20].try_into().ok()?) as usize;
    let end = (li + li_len).min(bytes.len());

    // 优先 Unicode 路径（header >= 0x24 才有）
    if li_hdr >= 0x24 {
        let lub = u32::from_le_bytes(bytes[li + 24..li + 28].try_into().ok()?) as usize;
        if li + lub < end {
            let w = utf16_cstr(&bytes[li + lub..end]);
            if !w.is_empty() {
                return Some(w);
            }
        }
    }
    if li + lbp < end {
        let s = ansi_cstr(&bytes[li + lbp..end]);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn utf16_cstr(b: &[u8]) -> String {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        let u = u16::from_le_bytes([b[i], b[i + 1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    String::from_utf16_lossy(&units)
}

fn ansi_cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&x| x == 0).unwrap_or(b.len());
    decode_ansi(&b[..n])
}

/// 把一段字节按系统 ACP（中文机器 = GBK）解码 —— 供 `tools::grep_files`
/// 读取 GBK 编码的 .md/.txt 用。
///
/// 与 [`decode_ansi`] 的区别：这里**不做 UTF-8 优先判断**（调用方已经确认
/// 严格 UTF-8 解不开才来）。解不出来返回空串，调用方据此判为二进制。
pub fn decode_acp(b: &[u8]) -> String {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    extern "system" {
        fn MultiByteToWideChar(
            cp: u32,
            flags: u32,
            mb: *const u8,
            cb: i32,
            wc: *mut u16,
            cc: i32,
        ) -> i32;
    }
    unsafe {
        // CP_ACP = 0
        let n = MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::new();
        }
        let mut wbuf = vec![0u16; n as usize];
        MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, wbuf.as_mut_ptr(), n);
        OsString::from_wide(&wbuf).to_string_lossy().into_owned()
    }
}

/// 解码 lnk ANSI 段路径。
///
/// 为什么不能无脑 CP_ACP：系统 ACP 是 GBK（中文机器），但**新式程序写 lnk 时
/// ANSI 段经常直接塞 UTF-8 字节** —— GBK 解 UTF-8 大多也能"成功"，产出
/// mojibake（实测 Typora 文件名「灝他口錙乂'堄.md」就是 4 个汉字的 UTF-8
/// 被按 GBK 拆的）。所以先用严格 UTF-8 校验：合法直接用；不合法才回落 ACP。
/// （GBK 字节串极少能通过严格 UTF-8 校验 —— 它的第二字节大量落在 ASCII 区，
/// 不是合法 UTF-8 续字节。）
fn decode_ansi(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    extern "system" {
        fn MultiByteToWideChar(
            cp: u32,
            flags: u32,
            mb: *const u8,
            cb: i32,
            wc: *mut u16,
            cc: i32,
        ) -> i32;
    }
    unsafe {
        // CP_ACP = 0
        let n = MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(b).into_owned();
        }
        let mut wbuf = vec![0u16; n as usize];
        MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, wbuf.as_mut_ptr(), n);
        OsString::from_wide(&wbuf).to_string_lossy().into_owned()
    }
}

/// 加上 `WS_EX_NOACTIVATE`：点击窗口不会抢走焦点。
///
/// 悬浮球（orb）态使用。同时加 `WS_EX_TOOLWINDOW`：
/// 悬浮球不该出现在 Alt+Tab 里。
pub fn set_no_activate(hwnd: Hwnd) {
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let next = cur | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    }
}

/// 去掉 `WS_EX_NOACTIVATE` **和** `WS_EX_TOOLWINDOW`：让窗口成为正常的前台窗口。
///
/// 面板（panel）态使用 —— 除了输入框要能聚焦，还要让窗口被系统认作"活动窗口"，
/// 否则部分自动化工具 / 辅助技术会找不到输入目标。
pub fn clear_no_activate(hwnd: Hwnd) {
    unsafe {
        let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let next = cur & !WS_EX_NOACTIVATE & !WS_EX_TOOLWINDOW;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    }
}

/// 保持在最顶层，且**不激活**窗口。
///
/// 适用于：悬浮球重新出现、或用户点了别处之后要把它重新提到最前。
pub fn keep_topmost_without_activating(hwnd: Hwnd) {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// **强行**把窗口提到前台 —— 绕开 Windows 的前台锁（foreground lock）。
///
/// 为什么单靠 Tauri 的 `set_focus()` 不够：
///   进程不是前台时，`SetForegroundWindow` 会被系统**静默拒绝**（不报错、不生效）。
///   于是窗口看上去打开了（bounds / 扩展样式都对），但 OS 级的「前台窗口」仍是用户
///   原来那个应用 —— 任何 `SendInput`（自动化工具的 `Type`、模拟键盘）全落到那个
///   应用上，面板输入框永远空。这正是「windows-mcp 点得开球、却打不进输入框」的
///   根因（见 `allmemory/MEMORY.md` 的「已知环境坑」）。
///
/// 手法：`AttachThreadInput` 把**窗口线程**挂到当前前台线程上（绕前台锁），
/// 同时把**调用线程**挂到窗口线程上（`SetFocus` 只认"已挂到自己队列"的窗口）；
/// 用完两处都**立刻断开**，不给别的线程留副作用。
///
/// 只在**面板态**调用：球态刻意不抢焦点（`WS_EX_NOACTIVATE`），那是产品红线。
pub fn force_foreground(hwnd: Hwnd) {
    unsafe {
        if hwnd.is_null() {
            return;
        }
        let tid_win = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let tid_me = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let tid_fg = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };

        // ① 窗口线程 → 前台线程：让 SetForegroundWindow 被放行
        let a_fg = tid_fg != 0 && tid_fg != tid_win && AttachThreadInput(tid_win, tid_fg, 1) != 0;
        // ② 调用线程 → 窗口线程：让 SetFocus(hwnd) 合法
        let a_win = tid_win != 0 && tid_win != tid_me && AttachThreadInput(tid_me, tid_win, 1) != 0;

        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);

        if a_win {
            AttachThreadInput(tid_me, tid_win, 0);
        }
        if a_fg {
            AttachThreadInput(tid_win, tid_fg, 0);
        }
    }
}

/// 从 UTF-16 缓冲区读出字符串（以第一个 NUL 截断）
fn from_wide(buf: &[u16], len: i32) -> String {
    if len <= 0 {
        return String::new();
    }
    let n = (len as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 前台窗口信息：`(进程 exe 路径, 窗口标题)`。
///
/// 拿不到（无前台窗口 / 权限不足）时返回 None。
/// 这是「用户在用什么应用」的采集入口 —— 比 UIA 树稳定得多：
/// 进程名和标题是内核级窗口属性，不依赖目标应用开不开无障碍。
pub fn foreground_window_info() -> Option<(String, String)> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }

        // 标题
        let mut title_buf = [0u16; 512];
        let tlen = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
        let title = from_wide(&title_buf, tlen);

        // 进程路径
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid as *mut u32);
        let mut exe = String::new();
        if pid != 0 {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !h.is_null() {
                let mut path_buf = [0u16; 1024];
                let mut size = path_buf.len() as u32;
                if QueryFullProcessImageNameW(h, 0, path_buf.as_mut_ptr(), &mut size) != 0 {
                    exe = from_wide(&path_buf, size as i32);
                }
                CloseHandle(h);
            }
        }

        if exe.is_empty() && title.is_empty() {
            return None;
        }
        Some((exe, title))
    }
}

/// **绕开 Windows 最小窗口尺寸限制**，直接设置窗口的位置与尺寸。
///
/// 为什么必须用这个而不是 Tauri 的 `set_size`：
///
///   Windows 会给窗口发 `WM_GETMINMAXINFO`，默认把最小跟踪尺寸钳制到
///   `SM_CXMINTRACK`（本机实测 = 202 物理像素）。Tauri 创建窗口时没有
///   覆盖该消息，所以配置里的 `width: 56`（= 84 物理像素 @1.5x）会被系统
///   强行抬到 202x84 —— 悬浮球就被撑成了长条。
///
///   `SetWindowPos` 不受 `WM_GETMINMAXINFO` 约束，能直接落到目标尺寸。
///
/// 所有参数均为**物理像素**。
pub fn set_bounds(hwnd: Hwnd, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小可解析的 .lnk：无 ItemID，LinkInfo 无 Unicode 段
    /// （HeaderSize=0x1C），LocalBasePath 直接放调用方给的字节。
    fn make_lnk_ansi(path_bytes: &[u8]) -> Vec<u8> {
        let li_hdr = 0x1Cusize; // 28：无 Unicode 段
        let vol_len = 0x10usize; // VolumeID：DriveType+Serial+LabelOff+NUL+补零
        let lbp_off = li_hdr + vol_len; // 0x2C
        let li_len = lbp_off + path_bytes.len() + 1 + 1; // path + NUL + suffix NUL

        let mut v = vec![0u8; 76];
        v[..4].copy_from_slice(&[0x4C, 0, 0, 0]); // magic
        v[20..24].copy_from_slice(&2u32.to_le_bytes()); // HasLinkInfo

        let mut li = vec![0u8; li_len];
        li[0..4].copy_from_slice(&(li_len as u32).to_le_bytes());
        li[4..8].copy_from_slice(&(li_hdr as u32).to_le_bytes());
        li[8..12].copy_from_slice(&1u32.to_le_bytes()); // VolumeAndLocalBasePath
        li[12..16].copy_from_slice(&(li_hdr as u32).to_le_bytes()); // VolumeIDOffset
        li[16..20].copy_from_slice(&(lbp_off as u32).to_le_bytes()); // LocalBasePathOffset
        li[24..28].copy_from_slice(&((lbp_off + path_bytes.len() + 1) as u32).to_le_bytes());
        // VolumeID：DriveType=3（固定盘）、Serial=0、label 偏移 0x0C 处是 NUL 空串
        li[li_hdr..li_hdr + 4].copy_from_slice(&3u32.to_le_bytes());
        li[lbp_off..lbp_off + path_bytes.len()].copy_from_slice(path_bytes);

        v.extend_from_slice(&li);
        v
    }

    #[test]
    fn lnk_utf8_path_in_ansi_field() {
        // 新式程序（如 Typora）写的 lnk：ANSI 段直接塞 UTF-8 中文
        let name = "数据结构.md";
        let lnk = make_lnk_ansi(name.as_bytes());
        assert_eq!(lnk_target(&lnk).as_deref(), Some(name));
    }

    #[test]
    fn lnk_gbk_path_falls_back_to_acp() {
        // 旧式 GBK 字节（「数据.md」的 GBK 编码）不是合法 UTF-8 → 应回落 CP_ACP
        // 解码且不产生替换符。具体解出什么字取决于机器 ACP（中文机器=936 解出原名）。
        let gbk: &[u8] = &[0xCA, 0xFD, 0xBE, 0xDD, 0x2E, 0x6D, 0x64];
        let lnk = make_lnk_ansi(gbk);
        let got = lnk_target(&lnk).unwrap_or_default();
        assert!(got.ends_with(".md"), "GBK 应走 ACP 解码，实际: {got}");
        assert!(!got.contains('\u{FFFD}'), "不应有替换符: {got}");
    }

    #[test]
    fn lnk_ascii_untouched() {
        let lnk = make_lnk_ansi(b"C:\\dir\\file.txt");
        assert_eq!(lnk_target(&lnk).as_deref(), Some("C:\\dir\\file.txt"));
    }
}
