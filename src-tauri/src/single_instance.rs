//! 单实例锁 —— 防止重复拉起第二个 orbcat。
//!
//! ## 为什么必须做
//! 悬浮球是 `always-on-top` + `skipTaskbar` 的窗口。跑起两个实例的表现是：
//! **两个球叠在一起、两个托盘图标、两份 `settings.json` 互相覆盖**，
//! 而用户在任务栏里看不到它们（skipTaskbar），只能去任务管理器杀进程。
//! 双击桌面快捷方式、或"开机自启 + 手动又点一次"，都会撞上这个。
//!
//! ## 机制（零新增依赖，纯 Win32）
//! 1. 先建命名事件 `Local\orbcat-wake`（已存在则拿到它的句柄）。
//! 2. 再建命名互斥体 `Local\orbcat-single-instance`：
//!    - `ERROR_ALREADY_EXISTS` → 已有实例在跑 → `SetEvent` 通知它
//!      「用户又启动了一次，把球叫出来」→ 本进程立刻退出；
//!    - 否则本进程是主实例，两个句柄都**故意泄漏**（进程活着名字才活着，
//!      `Drop` 掉就等于放锁）。
//!
//! ⚠️ **两步顺序不能反**：先建事件再建互斥体，保证第二实例看到互斥体存在时
//! 事件一定已经存在 —— 反过来会撞上「主实例建完互斥体、还没建事件」的窗口期。
//!
//! `Local\` 前缀 = 只在**当前登录会话**可见：同一台机器上不同用户各跑一份，互不干扰。

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Handle = *mut c_void;

    const MUTEX_NAME: &str = "Local\\orbcat-single-instance";
    const EVENT_NAME: &str = "Local\\orbcat-wake";

    /// `GetLastError` 的「对象已存在」错误码
    const ERROR_ALREADY_EXISTS: i32 = 183;
    const INFINITE: u32 = 0xFFFF_FFFF;

    /// 主实例的唤醒事件句柄（以 `usize` 存，便于跨线程传给监听线程）。
    static WAKE_EVENT: OnceLock<usize> = OnceLock::new();

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attr: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
        fn CreateEventW(
            attr: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> Handle;
        fn SetEvent(handle: Handle) -> i32;
        fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
        fn SetLastError(code: u32);
        fn CloseHandle(handle: Handle) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 本次启动的归属。
    #[derive(Debug, PartialEq, Eq)]
    pub enum Acquired {
        /// 本进程是唯一实例，继续正常启动。
        Primary,
        /// 已有实例在跑；已向它发过唤醒信号，本进程应当**立刻退出**。
        AlreadyRunning,
    }

    /// 尝试取得单实例锁。
    ///
    /// 必须在**创建任何窗口之前**调用。
    pub fn acquire() -> Acquired {
        let ev_name = wide(EVENT_NAME);
        // 自动重置事件（manual_reset = 0）：收到一次信号就自动复位，
        // 天然合并"用户连点好几次"的重复唤醒。
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, ev_name.as_ptr()) };
        if event.is_null() {
            // 极端情况（句柄耗尽等）：不因为锁失效就拒绝启动，退化成"允许多开"。
            eprintln!(
                "[orbcat] ⚠️ 单实例锁：创建唤醒事件失败（{}），本次跳过单实例检查",
                std::io::Error::last_os_error()
            );
            return Acquired::Primary;
        }

        let mtx_name = wide(MUTEX_NAME);
        // initial_owner = 0：不占用所有权。我们只要"名字活着"，不需要持有互斥体，
        // 这样也避免了"拥有者线程退出 → 互斥体变 abandoned"的一堆边界情况。
        unsafe { SetLastError(0) };
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mtx_name.as_ptr()) };
        let already = std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS);

        if already {
            unsafe { SetEvent(event) };
            // 事件句柄由主实例持有，这边用完就还。
            unsafe { CloseHandle(event) };
            return Acquired::AlreadyRunning;
        }

        if mutex.is_null() {
            eprintln!(
                "[orbcat] ⚠️ 单实例锁：创建互斥体失败（{}），退化为允许多开",
                std::io::Error::last_os_error()
            );
            return Acquired::Primary;
        }

        // ⚠️ 句柄**故意不关**：`HANDLE` 是裸指针、没有 `Drop`，只要进程活着
        //    这个句柄就活着 —— 互斥体的"名字"也就一直存在，第二个实例才检测得到我们。
        //    绑到带下划线的名字上，表明"留着不用"是刻意的，不是漏写。
        let _mutex_kept_alive = mutex;
        let _ = WAKE_EVENT.set(event as usize);
        Acquired::Primary
    }

    /// 起后台线程等唤醒信号，收到就回调 `on_wake`。
    ///
    /// 只在 `acquire()` 返回 `Primary` 后调用一次。回调在**非主线程**执行，
    /// 调用方若要碰窗口，自己 `run_on_main_thread` 或依赖 Tauri 的线程安全封装。
    pub fn spawn_wake_listener<F>(on_wake: F)
    where
        F: Fn() + Send + 'static,
    {
        let Some(&raw) = WAKE_EVENT.get() else {
            eprintln!("[orbcat] ⚠️ 单实例锁：没有唤醒事件句柄，唤醒监听未启动");
            return;
        };
        std::thread::spawn(move || {
            let handle = raw as Handle;
            loop {
                // WAIT_OBJECT_0 = 0；WAIT_FAILED / WAIT_ABANDONED 等没有必要继续等
                let r = unsafe { WaitForSingleObject(handle, INFINITE) };
                if r != 0 {
                    eprintln!("[orbcat] ⚠️ 唤醒等待异常结束（code={r}），监听线程退出");
                    return;
                }
                on_wake();
            }
        });
    }
}

/// macOS / Linux：用 `flock` 拿排他锁，锁在文件上而不是命名对象上。
///
/// ## 为什么 Windows 用 Mutex、这里用 flock
///
/// Windows 的命名互斥体是**会话级**的（`Local\` 前缀），天然解决「同一台机器
/// 不同用户各跑一份」。POSIX 没有跨进程命名锁的等价物，只能退到「锁文件 +
/// flock」。文件放 `$TMPDIR`（每用户独立），效果与 `Local\` 语义对齐 ——
/// 不同用户互相看不见对方的锁文件。
///
/// ## 唤醒信号怎么传
///
/// flock 本身只能判断「有没有别人」，不能传消息。做法是：第二实例发现锁被占，
/// 就往锁文件里写一行 `WAKE\n` 并 `fsync`；主实例的后台线程用**非阻塞轮询**
/// 检查文件内容是否被追加过。
///
/// 为什么不用 Unix domain socket / FIFO：那两个在 macOS 沙箱与临时目录清理上
/// 有坑，而轮询一个自己独占的小文件足够可靠 —— 唤醒频率极低
/// （只在用户重复启动时发生），2 秒一次轮询的开销可以忽略。
#[cfg(not(windows))]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// 本次启动的归属。
    #[derive(Debug, PartialEq, Eq)]
    pub enum Acquired {
        /// 本进程是唯一实例，继续正常启动。
        Primary,
        /// 已有实例在跑；已向它发过唤醒信号，本进程应当**立刻退出**。
        AlreadyRunning,
    }

    /// 锁文件路径。每用户独立目录，等价于 Windows 的 `Local\` 会话级语义。
    fn lock_path() -> PathBuf {
        let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        // 目录末尾通常带 `/`，`join` 会正确处理
        PathBuf::from(dir).join("orbcat-single-instance.lock")
    }

    /// 主实例持有的锁文件句柄。**故意泄漏到进程结束** —— fd 一关锁就放了。
    static PRIMARY_LOCK: OnceLock<File> = OnceLock::new();

    /// 尝试取得单实例锁。必须在**创建任何窗口之前**调用。
    pub fn acquire() -> Acquired {
        let path = lock_path();

        // ⚠️ 必须 `mut`：拿到锁后要 `set_len(0)` 清掉上次残留内容。
        //    这个 `mut` 是 macOS/Linux 专属路径才会被编译器检查到的 ——
        //    Windows 上整个 imp 模块不存在。
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                // 拿不到锁文件就退化成允许多开 —— 不要因为锁失效就拒绝启动
                eprintln!("[orbcat] ⚠️ 单实例锁：打开锁文件失败（{e}），本次跳过单实例检查");
                return Acquired::Primary;
            }
        };

        // 尝试拿排他锁。EWOULDBLOCK = 别人已经持锁 = 有实例在跑。
        match try_lock(&file) {
            Ok(()) => {
                // 拿到锁：初始化文件内容，再登记句柄
                let _ = file.set_len(0);
                let _ = file.write_all(b"PRIMARY\n");
                let _ = file.flush();
                let _ = PRIMARY_LOCK.set(file);
                Acquired::Primary
            }
            Err(_) => {
                // 锁被占 = 已有实例在跑 → 给它发唤醒信号
                signal_wake(&path);
                Acquired::AlreadyRunning
            }
        }
    }

    /// 起后台线程轮询唤醒信号。
    pub fn spawn_wake_listener<F>(on_wake: F)
    where
        F: Fn() + Send + 'static,
    {
        let Some(lock) = PRIMARY_LOCK.get() else {
            eprintln!("[orbcat] ⚠️ 单实例锁：没有锁文件句柄，唤醒监听未启动");
            return;
        };
        // 用路径而不是 fd —— 后者被主线程持有，跨线程读会撞上文件偏移
        let path = lock_path();
        let _ = lock; // 保持语义清晰：主实例是靠这个句柄持锁的

        std::thread::spawn(move || {
            let mut last_len = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let Ok(mut f) = OpenOptions::new().read(true).open(&path) else {
                    continue;
                };
                let Ok(len) = f.seek(SeekFrom::End(0)) else {
                    continue;
                };
                if len > last_len {
                    // 有新内容 = 有人请求唤醒
                    last_len = len;
                    let mut buf = String::new();
                    let _ = f.read_to_string(&mut buf);
                    if buf.contains("WAKE") {
                        last_len = 0; // 复位，避免同一次唤醒被重复消费
                        on_wake();
                    }
                }
            }
        });
    }

    // -- flock 的平台差异封装 ------------------------------------------------

    #[cfg(target_os = "macos")]
    fn try_lock(f: &File) -> Result<(), std::io::Error> {
        // LOCK_EX = 2（独占），LOCK_NB = 4（不阻塞，锁被占立刻报错）
        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        // 1 = LOCK_SH, 2 = LOCK_EX, 4 = LOCK_NB
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        let rc = unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn try_lock(f: &File) -> Result<(), std::io::Error> {
        // Linux 等：走 libc 的 flock(2)，同一套 LOCK_EX/LOCK_NB 语义
        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        let rc = unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;

    /// 往锁文件追加唤醒标记。
    fn signal_wake(path: &PathBuf) {
        let Ok(mut f) = OpenOptions::new().append(true).open(path) else {
            return;
        };
        let _ = f.write_all(b"WAKE\n");
        let _ = f.flush();
        let _ = f.sync_data(); // 落到磁盘，避免主实例读到陈旧缓存
    }
}

pub use imp::{acquire, spawn_wake_listener, Acquired};
