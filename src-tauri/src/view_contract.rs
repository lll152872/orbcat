//! 视图结构的**字段名契约**：后端 view struct 必须序列化成 camelCase。
//!
//! ## 这条契约为什么值钱
//!
//! 历史上 `ProjectMemoryView`（本文件外，`lib.rs` 里）漏了
//! `#[serde(rename_all = "camelCase")]`，序列化成 `source_alive` / `has_memory`，
//! 而前端读 `p.sourceAlive` / `p.hasMemory` —— 全是 `undefined`。
//!
//! 后果静默得可怕：`!undefined === true`，于是**只要项目绑了路径就永远显示
//! 「源路径不存在」**，而那 4 条路径一个都没失效。Rust 编译通过、单测全绿、
//! UI 照常渲染 —— 只是把一个布尔读成了 `undefined`。2026-10-14 用户截图报出。
//!
//! TypeScript 抓不到这类问题：`invoke<T>()` 的泛型是**编译期承诺**，
//! 运行时后端给什么形状它都不管。所以唯一能拦住它的哨兵在这儿。
//!
//! ## 为什么能放在本文件测
//!
//! `lib.rs` 是 tauri 装配层，绝大多数 `#[tauri::command]` 都要 `State<AppState>`
//! 才有意义，单测构造不出来。但 serde 序列化是**纯函数** —— 起不启 app 都能测，
//! 所以把这类"不碰运行时"的契约检查单独拆到这里，不去动那些命令。

use super::*;

// ---------------------------------------------------------------------------
// 窗口摆位：面板/菜单必须锚定到**球被拖到的真实位置**
// ---------------------------------------------------------------------------

/// 1920×1080 @scale 1.0，`None` = 首启、还没记过球位置。
const MON_POS: (i32, i32) = (0, 0);
const MON_SIZE: (u32, u32) = (1920, 1080);

/// 球被拖到左上区域某点。
const DRAGGED: (i32, i32) = (300, 500);

/// 回归本体（2026-10-14 用户报）：
/// 「我移动球不能影响打开面板的位置 / 打开逻辑应该是基于当前球的位置的」。
///
/// 旧 `compute_bounds` 每次把球**重新硬算**成「贴右边 + 固定 40% 高度」，
/// 球拖到哪儿都白拖。这三条锁住「面板跟着球走」。
#[test]
fn panel_follows_the_dragged_orb() {
    let (_x, y, _w, _h) = compute_bounds("panel", MON_POS, MON_SIZE, 1.0, Some(DRAGGED));
    // 纵向锚点：面板顶 = 球顶 - 面板高/3（与旧默认的几何关系一致）
    let ph = PANEL_H_LOGICAL as i32; // 600
    assert_eq!(
        y,
        DRAGGED.1 - ph / 3,
        "面板纵向必须锚在球上，而不是固定的 40% 屏高"
    );

    // 对照：没有锚点时仍是旧默认 —— 球的 y = 1080*0.40 = 432，面板 = 432 - 200
    let (_dx, def_y, _, _) = compute_bounds("panel", MON_POS, MON_SIZE, 1.0, None);
    assert_eq!(def_y, 432 - ph / 3, "首启摆位不变（球在 40% 屏高）");
    assert_ne!(y, def_y, "测试前提失效：两种情况算出了同一个 y");
}

/// 球在左半边时，面板**不能溢出屏幕左边**。
///
/// 这是加锚点后新引入的风险：球在右边时面板向左展开刚好不出界，球挪到
/// 左边再向左展开就出去了。必须夹住。
#[test]
fn panel_is_clamped_when_orb_dragged_to_left_edge() {
    let (x, _y, w, _h) = compute_bounds("panel", MON_POS, MON_SIZE, 1.0, Some((0, 500)));
    assert!(x >= MARGIN_LOGICAL as i32, "面板左边缘 {x} 跑出屏幕");
    assert!(
        x + w <= MON_SIZE.0 as i32 - MARGIN_LOGICAL as i32,
        "面板右边缘 {} 跑出屏幕",
        x + w
    );
}

/// 球在**右下角**（默认附近）时，面板仍然贴着球的右边缘 —— 不能因为加了
/// 锚点就把「右对齐」这个几何关系改掉。
#[test]
fn panel_right_edge_still_aligns_with_orb_right_edge() {
    let orb = (1836, 432); // 默认摆位下的球左上角
    let (x, _y, w, _h) = compute_bounds("panel", MON_POS, MON_SIZE, 1.0, Some(orb));
    let orb_right = orb.0 + ORB_LOGICAL as i32;
    assert_eq!(
        orb_right - x,
        w,
        "面板右边缘应与球右边缘对齐（旧行为不能被改掉）"
    );
}

/// 菜单同理：跟着球走，且两个方向都夹住。
#[test]
fn menu_follows_orb_and_stays_on_screen() {
    let (x, y, w, h) = compute_bounds("menu", MON_POS, MON_SIZE, 1.0, Some((0, 0)));
    assert!(x >= MARGIN_LOGICAL as i32, "菜单左侧 {x} 出界");
    assert!(y >= MARGIN_LOGICAL as i32, "菜单顶部 {y} 出界");
    assert!(x + w <= 1920 - MARGIN_LOGICAL as i32, "菜单右侧出界");
    assert!(y + h <= 1080 - MARGIN_LOGICAL as i32, "菜单底部出界");
}

/// 收起时球要回到**用户拖的位置**，不是默认摆位。
///
/// 旧代码 orb 分支也用硬算值，于是「展开再收起」会把球弹回右边 ——
/// 这是同一个 bug 的另一半，很容易只修一半。
#[test]
fn orb_returns_to_dragged_position_not_default() {
    let (x, y, w, _h) = compute_bounds("orb", MON_POS, MON_SIZE, 1.0, Some(DRAGGED));
    let pad = ORB_PAD_LOGICAL as i32;
    let orb_px = ORB_LOGICAL as i32;
    assert_eq!(
        (x + pad, y + pad),
        DRAGGED,
        "收起后球必须回到拖到的位置"
    );
    assert_eq!(w, orb_px + pad * 2, "球窗口尺寸不变（仍比球大一圈）");

    // 反向对照：没有锚点时才是默认摆位
    let (dx, dy, _, _) = compute_bounds("orb", MON_POS, MON_SIZE, 1.0, None);
    let pad = ORB_PAD_LOGICAL as i32;
    assert_eq!(dx + pad, 1920 - ORB_LOGICAL as i32 - MARGIN_LOGICAL as i32);
    assert_ne!((x, y), (dx, dy), "测试前提失效：锚点没起作用");
}

/// 首启（没有锚点）必须完全保持旧行为 —— 别把新装用户的位置搞坏。
#[test]
fn no_anchor_keeps_legacy_default_placement() {
    let (x, y, _w, _h) = compute_bounds("orb", MON_POS, MON_SIZE, 1.0, None);
    let pad = ORB_PAD_LOGICAL as i32;
    assert_eq!(x + pad, 1920 - 56 - 24, "默认贴右边 24px");
    assert_eq!(y + pad, 432, "默认在 40% 屏高");
}

// ---------------------------------------------------------------------------
// 球位置落盘：防抖 + 退出前 flush
// ---------------------------------------------------------------------------

/// `ORB_SAVE_PENDING` 是**全局 static**，下面两个用例共用同一条队列。
///
/// cargo test 默认并行跑：flush 用例的 `take()` 会把 debounce 用例正等着写的
/// 值顺手拿走，于是 debounce 用例在整套里偶发红（单跑却绿）——
/// 典型的"只在整套里出现"的假失败。这里串行化。
static ORB_SAVE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 连打一串位置后，落盘的必须是**最后一个**。
///
/// ## 这条测试真正证明了什么（别过度解读）
///
/// 它锁的是**用户可见的结果**：「拖一串事件之后，落盘位置 = 最后那个」。
/// 注意它**不能**区分「防抖了一次写」和「每次都同步写」—— 两者最终都是最后那个值，
/// 而后者会卡主线程。所以"有没有真的防抖"靠的是 [`ORB_SAVE_DEBOUNCE_MS`]
/// 那个 worker 的代码结构，不是这条测试。
///
/// 那为什么还要它？因为「落盘的是中间某个值」或「落盘的是第一个值」是真实的
/// 失效模式（队列取错、race），这条正好把它们挡住。
#[test]
fn debounced_orb_pos_save_writes_only_the_last_position() {
    let _serial = ORB_SAVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "orbcat_orbdebounce_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 先放一个可辨认的初值，确保"写进去了"能被看出来
    let mut s = config::load_settings(&dir);
    s.orb_pos = Some(config::OrbPos { x: 1, y: 1 });
    config::save_settings(&dir, &s).unwrap();

    for i in 2..=6 {
        schedule_orb_pos_save(&dir, (i * 100, i * 10));
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    // ⚠️ **轮询**而不是「睡够安静期 + 400ms」（2026-10-18）：
    // 原来那版在 587 条用例**并行**跑时偶发失败（本机单跑 3/3 通过、
    // 全量跑偶发挂）—— 固定睡眠等于假定 worker 线程拿得到 CPU。机器一忙，
    // worker 被推迟到断言之后才落盘，于是读到的是**中间某个**位置。
    //
    // 这里等的是**真正要等的那件事**（落盘成最后那个值），而不是"睡够这么久
    // 应该落完了"。慢机器上只是多等几轮，不会误判成失败。
    let want = Some(config::OrbPos { x: 600, y: 60 });
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5_000);
    let got = loop {
        let cur = config::load_settings(&dir).orb_pos;
        if cur == want || std::time::Instant::now() > deadline {
            break cur;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    assert_eq!(
        got, want,
        "拖一串事件后必须落盘**最后一个**位置，不是第一个也不是中间某个"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `flush` 必须把还排在防抖窗口里的值立刻写掉。
///
/// 对应「拖完球直接退出/杀进程」那条路 —— 不 flush 就会退回更早一次的位置。
#[test]
fn flush_writes_pending_position_without_waiting_for_debounce() {
    let _serial = ORB_SAVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "orbcat_orbflush_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    schedule_orb_pos_save(&dir, (777, 888));
    // **不等**安静期，直接 flush
    flush_orb_pos_save();

    assert_eq!(
        config::load_settings(&dir).orb_pos,
        Some(config::OrbPos { x: 777, y: 888 }),
        "退出前必须把待写的位置落盘"
    );
    // flush 后队列已空，再 flush 一次不能出事
    flush_orb_pos_save();
    assert_eq!(config::load_settings(&dir).orb_pos, Some(config::OrbPos { x: 777, y: 888 }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 🔴 回归：**待写槽里的值必须连目录一起走**。
///
/// ## 这是什么（2026-10-24 那条偶发红的根因）
///
/// `debounced_orb_pos_save_writes_only_the_last_position` 在整套并行跑时偶发红：
/// `flush` 用例留下的 worker（还在睡 600ms 安静期）醒来后 `take()` 走了
/// debounce 用例正等着写的值，却把它写进了**自己那个目录**
/// ⇒ debounce 用例的目录永远等不到那个值 ⇒ 红。
///
/// 根因在**实现**：`ORB_SAVE_PENDING` 原来**只存坐标、不存目录**，
/// 落盘的目标是「谁先 spawn 的那个 worker 捕获的目录」⇒ **目录与值对不上**。
/// （生产里只有一个数据目录，所以这条平时看不出来 —— 正是"平时没事、
/// 一进测试就偶发"的典型。）
///
/// ## 这条用例怎么**确定性地**复现那个交错
///
/// 不靠并行、不靠运气：直接把「值属于 dir_b、worker 却属于 dir_a」这个局面造出来。
/// ① 给 dir_a 排一个值 ⇒ 起 worker_a（安静期 `ORB_SAVE_DEBOUNCE_MS`，
///    它捕获的落盘目录 = dir_a）；
/// ② 再给 dir_b 排一个值 ⇒ pending 非空 ⇒ **不会**再起 worker
///    （所以活着的 worker 只有 worker_a，而值属于 dir_b）；
/// ③ 等 worker_a 醒来。
///
/// ⇒ **旧实现**：worker_a 把 dir_b 的值写进 dir_a ⇒ dir_b 永远是空的 ⇒ **必红**；
/// **修好后**：值连目录一起走 ⇒ worker_a 写进 dir_b ⇒ 绿。
///
/// 旧码下这条是**确定性失败**（不是 flaky）—— 这正是它能当判据的原因。
#[test]
fn 跨目录的待写值不许写错目录() {
    let _serial = ORB_SAVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!(
        "orbcat_orbmix_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let dir_a = root.join("a");
    let dir_b = root.join("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    // 先把槽清空 —— 保证本用例的「pending 为空 ⇒ 一定起 worker」这个前提成立
    {
        let mut g = ORB_SAVE_PENDING.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }

    // ① 值属于 dir_a，起 worker_a
    schedule_orb_pos_save(&dir_a, (11, 11));
    // ② 值换成属于 dir_b —— pending 非空 ⇒ 不会再起 worker（只有 worker_a 活着）
    schedule_orb_pos_save(&dir_b, (22, 22));

    // ③ 等 worker_a 醒来（留余量，慢机器上也够）
    std::thread::sleep(std::time::Duration::from_millis(
        ORB_SAVE_DEBOUNCE_MS + 400,
    ));

    assert_eq!(
        config::load_settings(&dir_b).orb_pos,
        Some(config::OrbPos { x: 22, y: 22 }),
        "dir_b 的值必须落到 dir_b —— 上一个 worker 不许把它写进别的目录"
    );
    assert_ne!(
        config::load_settings(&dir_a).orb_pos,
        Some(config::OrbPos { x: 22, y: 22 }),
        "dir_a 不该拿到 dir_b 的值（落盘目标必须跟着值走）"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 尺寸判据：只有**恰好**是球窗口大小才算球态。
///
/// 面板/菜单态移动时必须返回 `None` —— 否则会把面板位置写进 `orbPos`，
/// 下次启动球直接飞到面板那儿。
#[test]
fn orb_anchor_only_for_exact_orb_window_size() {
    let pad = ORB_PAD_LOGICAL as i32;
    let orb_px = ORB_LOGICAL as i32;
    let orb_win = orb_px + pad * 2;
    let pos = tauri::PhysicalPosition::new(300i32, 500i32);

    // 球态
    assert_eq!(
        orb_anchor_if_orb(
            pos,
            tauri::PhysicalSize::new(orb_win as u32, orb_win as u32),
            1.0
        ),
        Some((300 + pad, 500 + pad)),
        "球态窗口应换算出球的左上角（含 pad）"
    );

    // 面板态：宽高都不对 → 不认
    assert_eq!(
        orb_anchor_if_orb(
            pos,
            tauri::PhysicalSize::new(460u32, 600u32),
            1.0
        ),
        None,
        "面板尺寸必须被排除"
    );
    // 只宽不对也不行（横竖屏切换的中间态可能出现）
    assert_eq!(
        orb_anchor_if_orb(
            pos,
            tauri::PhysicalSize::new(orb_win as u32, orb_px as u32),
            1.0
        ),
        None,
        "尺寸不匹配一律不算球态"
    );
    // 菜单态
    assert_eq!(
        orb_anchor_if_orb(
            pos,
            tauri::PhysicalSize::new(196u32, 214u32),
            1.0
        ),
        None,
        "菜单尺寸必须被排除"
    );
}
///
/// ⚠️ 挑 `orb_y = 400` 是**算过的**：1.5 倍缩放下 margin=36、面板高 900，
/// 不触发夹取的区间是 `orb_y ∈ [336, 444]`。挑 500 会撞上"面板底部出屏"
/// 那道夹取（结果 144），那时测的就不是"跟不跟球"而是"夹不夹"了。
#[test]
fn panel_follows_orb_at_150_percent_scale() {
    let scale = 1.5;
    let ph = (PANEL_H_LOGICAL * scale).round() as i32; // 900
    let (_x, y, _w, _h) = compute_bounds("panel", MON_POS, MON_SIZE, scale, Some((300, 400)));
    assert_eq!(
        y,
        400 - ph / 3,
        "高分屏下面板也要跟着球（未触发夹取时就是纯锚点公式）"
    );
    assert_ne!(y, 432, "测试前提失效：缩放没生效");
}

/// 高分屏下面板可能顶到屏幕下沿 —— 必须夹进屏内，不能让底边跑到屏幕外。
///
/// 1.5 倍缩放下面板高 900、margin 36，`orb_y = 500` 会让 `py + ph = 1100`
/// 超出 `1080 - 36 = 1044` → 必须下压。
#[test]
fn panel_clamped_on_screen_when_orb_sits_low() {
    let scale = 1.5;
    let (x, y, w, h) = compute_bounds("panel", MON_POS, MON_SIZE, scale, Some((300, 500)));
    let margin = (MARGIN_LOGICAL * scale).round() as i32; // 36
    assert!(
        y + h <= MON_SIZE.1 as i32 - margin,
        "面板底边 {} 跑出屏幕下沿 {}",
        y + h,
        MON_SIZE.1 as i32 - margin
    );
    assert!(y >= margin, "面板顶边 {y} 跑出屏幕上沿");
    assert!(x >= margin, "面板左边 {x} 跑出屏幕");
    assert!(x + w <= MON_SIZE.0 as i32 - margin, "面板右边出界");
}

/// 把可序列化值转成 JSON 对象的键集合（键序无关）。
fn keys_of<T: serde::Serialize>(v: &T) -> Vec<String> {
    let v = serde_json::to_value(v).expect("序列化不应失败");
    v.as_object()
        .expect("view struct 应序列化成对象")
        .keys()
        .cloned()
        .collect()
}

/// 断言 `keys` 里含有全部 `want`。
fn assert_has(keys: &[String], want: &[&str], who: &str) {
    for w in want {
        assert!(
            keys.iter().any(|k| k == w),
            "{who} 缺字段 `{w}`，实际序列化出 {:?} —— 前端按驼峰读，\
             拿到 undefined 会静默走错分支",
            keys
        );
    }
}

/// 回归本体：项目记忆的绑定状态字段必须驼峰。
///
/// 就是这条让「已绑定」和「MEMORY 为空」两个徽标第一次有机会露出来 ——
/// 在此之前它们永远走不到。
#[test]
fn project_memory_view_serializes_camel_case() {
    let keys = keys_of(&ProjectMemoryView {
        name: "demo".into(),
        source: r"D:\some\path".into(),
        source_alive: true,
        has_memory: true,
    });
    assert_has(&keys, &["name", "source", "sourceAlive", "hasMemory"], "ProjectMemoryView");
    assert!(
        !keys.iter().any(|k| k == "source_alive" || k == "has_memory"),
        "还在吐 snake_case：{:?}",
        keys
    );
}

/// 同类哨兵：这些 struct 前端都按驼峰读，漏一个就是静默的 `undefined`。
///
/// 新增 view struct 时**顺手往这里加一行** —— 这比事后靠用户截图发现便宜得多。
#[test]
fn other_views_read_by_frontend_are_camel_case() {
    // 前端读 `canRequest` / `needsSystemSettings`
    let cc = keys_of(&ContextCapabilityInfo {
        level: "off",
        detail: String::new(),
        can_request: false,
        needs_system_settings: false,
    });
    assert_has(&cc, &["level", "detail", "canRequest", "needsSystemSettings"], "ContextCapabilityInfo");

    // 前端读 `permError`
    let pl = keys_of(&ProjectListView {
        bundles: vec![],
        active: None,
        unresolved: vec![],
        perm_error: None,
        tools: vec![],
    });
    assert_has(&pl, &["bundles", "active", "unresolved", "permError", "tools"], "ProjectListView");

    // 全是单词字段，但**同样必须**带 rename_all ——
    // 否则哪天加个 `last_used_at` 就会静默失效。
    let mf = keys_of(&MemoryFiles {
        rules: String::new(),
        soul: String::new(),
        identity: String::new(),
        user: String::new(),
        memory: String::new(),
    });
    assert_has(&mf, &["rules", "soul", "identity", "user", "memory"], "MemoryFiles");
}