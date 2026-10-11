//! 编译期资源收集 + 防夹带闸门。
//!
//! ## 这个文件在干什么
//!
//! 把 `skeleton/`（内置区，唯一进包的模板）遍历一遍，生成一份 Rust 静态表
//! 塞进 `OUT_DIR/skeleton_manifest.rs`，`bootstrap.rs` 用 `include!` 取用。
//! **顺带**在编译期把"用户数据混进包"这件事拦下来 —— 拦不住就别想编过。
//!
//! ## 为什么闸门必须在 build.rs，不能是 npm 脚本
//!
//! `tools/build-release.sh` 第 93/97 行直接跑 `cargo build`，**绕过 npm**。
//! 只挂在 npm 上的检查在发版路径上是装饰品。
//!
//! ## 为什么手写 JSON 解析 + 手写 sha256
//!
//! 1. **不引新 crate**（项目铁律）。serde_json 虽然已在依赖树里，但它在
//!    `[dependencies]` —— 要在 build.rs 里用就得挪进 `[build-dependencies]`。
//! 2. 本仓库的 `Cargo.toml` 第 19-38 行有一整段**关于 build-dep 依赖图把构建搞挂**
//!    的记录（indexmap 的 `has_std` 探测失效）。build-dep 图是块雷区，
//!    没必要为了一个自用 manifest 往里再塞一个带 proc-macro 依赖的 crate。
//! 3. manifest.json 是**我们自己写的**，schema 固定，手写解析器完全够用且好审计。
//!
//! sha256 实现会在 `main()` 开头拿标准测试向量自校验 —— 算错就直接构建失败，
//! 不会拿一个错误的 seed 去驱动升级逻辑。

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

// ===========================================================================
// 极简 JSON（只够读我们自己那份 manifest）
// ===========================================================================

#[derive(Debug, Clone)]
#[allow(dead_code)] // Num / Null 是通用 JSON 的完整形状，这里用不到但留着才像个解析器
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!(
                "JSON 第 {} 字节处期望 {:?}，实际 {:?}",
                self.i,
                c as char,
                self.peek().map(|b| b as char)
            ))
        }
    }
    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.peek() {
            None => Err("JSON 意外结束".into()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(_) => self.number(),
        }
    }
    fn lit(&mut self, w: &str, v: Json) -> Result<Json, String> {
        if self.s[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(format!("JSON 第 {} 字节处不是 {}", self.i, w))
        }
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .map(Json::Num)
            .ok_or_else(|| format!("JSON 第 {} 字节处数字解析失败", start))
    }
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("JSON 字符串未闭合")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or("JSON 转义未完成")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(
                                self.s.get(self.i..self.i + 4).ok_or("JSON \\u 截断")?,
                            )
                            .map_err(|_| "JSON \\u 非 UTF-8")?;
                            let cp = u32::from_str_radix(hex, 16).map_err(|_| "JSON \\u 非法")?;
                            self.i += 4;
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err("JSON 未知转义".into()),
                    }
                }
                _ => {
                    // 原始 UTF-8 多字节：按字节回填（外层 String 已是 UTF-8 拼装）
                    let start = self.i - 1;
                    while let Some(n) = self.peek() {
                        if n == b'"' || n == b'\\' {
                            break;
                        }
                        self.i += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.s[start..self.i])
                            .map_err(|_| "JSON 字符串非 UTF-8")?,
                    );
                }
            }
        }
    }
    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut a = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(a));
        }
        loop {
            a.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                _ => return Err("JSON 数组未闭合".into()),
            }
        }
    }
    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut kv = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(kv));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.expect(b':')?;
            let v = self.value()?;
            kv.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                _ => return Err("JSON 对象未闭合".into()),
            }
        }
    }
}

fn parse_json(src: &str) -> Result<Json, String> {
    let mut p = P {
        s: src.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    Ok(v)
}

// ===========================================================================
// sha256（自校验，零依赖）
// ===========================================================================

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// 构建期自校验：算错就直接构建失败，不会拿一个错误的 seed 去驱动升级逻辑。
fn selftest_sha256() {
    let vectors: [(&[u8], &str); 3] = [
        (
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
    ];
    for (input, want) in vectors {
        let got = hex(&sha256(input));
        assert_eq!(
            got, want,
            "build.rs 内置的 sha256 实现算错了（输入 {:?}）—— seed 不可信，拒绝构建",
            String::from_utf8_lossy(input)
        );
    }
}

// ===========================================================================
// manifest
// ===========================================================================

#[derive(Debug, Clone)]
struct Asset {
    src: String,
    rel: String,
    template: bool,
    render: bool,
    distribute: bool,
}

#[derive(Debug)]
struct Manifest {
    dirs: Vec<String>,
    assets: Vec<Asset>,
}

fn parse_manifest(path: &Path) -> Manifest {
    let src = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "读不到骨架声明 {}：{e}\n这一份 manifest 是内置区的唯一真相，没有它整套骨架无从谈起。",
            path.display()
        )
    });
    let j = parse_json(&src)
        .unwrap_or_else(|e| panic!("骨架声明 {} 解析失败：{e}", path.display()));

    // schemaVersion 写数字 1（也容忍字符串 "1"）
    match j.get("schemaVersion") {
        Some(Json::Num(n)) if *n == 1.0 => {}
        Some(Json::Str(s)) if s == "1" => {}
        other => panic!(
            "manifest schemaVersion 必须是 1，实际 {:?}",
            other.map(|v| match v {
                Json::Num(n) => format!("{n}"),
                Json::Str(s) => format!("\"{s}\""),
                other => format!("{other:?}"),
            })
        ),
    }
    let kind = j.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "orbcat-skeleton" {
        panic!("manifest kind 必须是 \"orbcat-skeleton\"，实际 {kind:?}");
    }

    let dirs: Vec<String> = j
        .get("dirs")
        .and_then(|v| v.as_arr())
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    let mut assets = Vec::new();
    for a in j.get("assets").and_then(|v| v.as_arr()).unwrap_or(&[]) {
        let src = a
            .get("src")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("assets 里有一条缺 src"))
            .to_string();
        let rel = a
            .get("rel")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("assets 里的 {src} 缺 rel"))
            .to_string();
        assets.push(Asset {
            src,
            rel,
            template: a.get("template").and_then(|v| v.as_bool()).unwrap_or(false),
            render: a
                .get("render")
                .and_then(|v| v.as_arr())
                .map(|r| !r.is_empty())
                .unwrap_or(false),
            distribute: a
                .get("distribute")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        });
    }
    Manifest { dirs, assets }
}

// ===========================================================================
// 闸门
// ===========================================================================

/// 命中即 panic —— 构建失败，这是设计目标，不是意外。
fn deny(rule: &str, what: &str, why: &str) -> ! {
    panic!(
        "\n\
         ═════════════════════════════════════════════════════════════\n\
         ✗ 防夹带闸门命中（规则 {rule}）\n\
         ═════════════════════════════════════════════════════════════\n\
           命中对象：{what}\n\
           为什么禁：{why}\n\
         \n\
         内置区 skeleton/ 是**唯一**允许进包的地方。\n\
         用户数据（记忆/画像/会话/密钥）住在 agent-data/，它属于用户区，\n\
         永远不该出现在这里。\n\
         ═════════════════════════════════════════════════════════════\n"
    )
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => panic!("遍历骨架目录 {} 失败：{e}", dir.display()),
    };
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if p.is_dir() {
            walk(&p, base, out);
        } else {
            out.push((rel, p));
        }
    }
}

/// 规则 ①：白名单 —— 骨架区里出现任何没在 manifest 声明的文件即失败。
///
/// 为什么是白名单不是黑名单：黑名单要穷举「哪些算用户数据」，永远穷举不完；
/// 白名单只要**诚实**。忘了声明 = 忘了审 = 失败。
fn guard_undeclared(root: &Path, m: &Manifest) {
    let declared: BTreeSet<&str> = m.assets.iter().map(|a| a.src.as_str()).collect();
    let mut files = Vec::new();
    walk(root, root, &mut files);
    for (rel, _) in files {
        if rel == "manifest.json" {
            continue;
        }
        if !declared.contains(rel.as_str()) {
            deny(
                "①",
                &format!("skeleton/{rel}"),
                "这个文件没在 manifest.json 里声明。\
                 内置区只认 manifest —— 多出来的文件一律视为「忘了审」。\
                 确认它是干净模板就加进 manifest 的 assets；确认它是用户数据就删掉它。",
            );
        }
    }
    // 反向：声明了但文件不在
    for a in &m.assets {
        if !root.join(&a.src).is_file() {
            deny(
                "①",
                &format!("manifest 声明了 {}，但文件不存在", a.src),
                "声明与实际不一致 —— 这会让构建产物与源码对不上。",
            );
        }
    }
}

/// 规则 ②③④⑤⑥⑦：逐条查每个声明文件的内容。
fn guard_content(root: &Path, m: &Manifest) {
    const MAX_FILE: u64 = 512 * 1024;
    for a in &m.assets {
        let abs = root.join(&a.src);

        // ③ 体积
        let len = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        if len > MAX_FILE {
            deny(
                "③",
                &format!("{}（{:.1} KB）", a.src, len as f64 / 1024.0),
                "单个文件超过 512 KB。内置区总共才 ~88 KB，\
                 出现这种体积几乎一定是误塞了数据集、构建产物或大日志。",
            );
        }

        // ⑥ 编译缓存 / 依赖目录 / 备份残留
        for frag in ["__pycache__", "node_modules", ".venv", "/target/"] {
            if a.src.contains(frag) {
                deny(
                    "⑥",
                    &a.src,
                    "编译缓存 / 依赖目录 / 构建产物不该进包 —— 它们是**本机运行状态**的残留。",
                );
            }
        }
        for bad in [".pyc", ".bak", ".exe", ".dll", ".pdb", ".sqlite"] {
            if a.src.ends_with(bad) {
                deny("⑥", &a.src, "编译产物 / 备份 / 密钥库文件不该进包。");
            }
        }

        // ④ rel 里不该出现 agent-data（防把用户区路径写进编译期产物）
        if a.rel.contains("agent-data") {
            deny(
                "④",
                &a.rel,
                "rel 是相对**数据目录**的路径，写 agent-data 等于把用户区名字焊进产物。",
            );
        }

        let body = fs::read(&abs).unwrap_or_else(|e| panic!("读 {} 失败：{e}", abs.display()));

        // ② UTF-8 且无 BOM
        if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
            deny(
                "②",
                &a.src,
                "文件带 UTF-8 BOM。BOM 会混进正文，让标题匹配和 seed 都失真 —— \
                 用无 BOM 的 UTF-8 另存一次。",
            );
        }
        let text = match String::from_utf8(body) {
            Ok(t) => t,
            Err(_) => deny(
                "②",
                &a.src,
                "不是合法 UTF-8 文本。内置区只放文本；二进制塞进来是绕开审查看不见内容的路子。",
            ),
        };

        // ⑤ Windows 盘符绝对路径 —— §2.5 那 12 个用户技能全栽在这条
        if let Some(off) = find_drive_abs(&text) {
            deny(
                "⑤",
                &format!("{} 第 {} 字节附近：{:?}", a.src, off, snippet(&text, off)),
                "正文里有本机绝对路径（`X:\\…`）。这会把**构建者的用户名与磁盘布局**\
                 原样分发给所有用户，换台机器也直接失效。\
                 要用路径就用 {{DATA_DIR}} / {{SKILL_DIR}} 占位符，\
                 或者让 agent 运行时自己探测。",
            );
        }

        // ⑦ 凭证
        for (pat, name) in [
            ("Bearer ", "Bearer 令牌"),
            ("api_key=", "api_key 赋值"),
            ("apiKey:", "apiKey 赋值"),
        ] {
            if let Some(off) = text.find(pat) {
                deny(
                    "⑦",
                    &format!("{}：{:?}", a.src, snippet(&text, off)),
                    &format!("正文里有{name}。凭证与 token 存放位置绝不能随包分发。"),
                );
            }
        }
        if let Some(off) = find_api_key_literal(&text) {
            deny(
                "⑦",
                &format!("{}：{:?}", a.src, snippet(&text, off)),
                "正文里有疑似真实 API key（sk-…）。",
            );
        }
    }
}

/// 找 `X:\` 形态的 Windows 盘符路径（要求后面紧跟一个非引号字符，排除 `C:\"` 这种）。
fn find_drive_abs(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        let c = b[i];
        if i + 2 < b.len()
            && c.is_ascii_alphabetic()
            && b[i + 1] == b':'
            && b[i + 2] == b'\\'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
        {
            // 排除 Markdown 里的行尾续行 `\` 以及反斜杠紧跟反引号的情况
            return Some(i);
        }
    }
    None
}

fn find_api_key_literal(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find("sk-") {
        let at = from + i;
        let tail = &s[at + 3..];
        let ok = tail
            .chars()
            .take(24)
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .count();
        if ok >= 16 {
            return Some(at);
        }
        from = at + 3;
    }
    None
}

fn snippet(s: &str, at: usize) -> String {
    let lo = at.saturating_sub(20);
    let hi = (at + 40).min(s.len());
    let lo = (0..=lo).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    let hi = (hi..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
    format!("…{}…", &s[lo..hi])
}

// ===========================================================================
// codegen
// ===========================================================================

fn rust_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{{{:x}}}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// seed = 骨架树的内容指纹。按 src 排序后逐条吃 `(src, 字节)`。
///
/// **不含** `distribute: false` 的文件 —— 它们不进 exe，改它们不该触发用户侧重铺。
fn compute_seed(root: &Path, m: &Manifest) -> String {
    let mut ordered: Vec<&Asset> = m.assets.iter().filter(|a| a.distribute).collect();
    ordered.sort_by(|a, b| a.src.cmp(&b.src));
    let mut acc: Vec<u8> = Vec::new();
    for a in ordered {
        acc.extend_from_slice(a.src.as_bytes());
        acc.push(0);
        acc.extend_from_slice(&fs::read(root.join(&a.src)).unwrap_or_default());
        acc.push(0);
    }
    hex(&sha256(&acc))
}

fn codegen(root: &Path, m: &Manifest, seed: &str) -> String {
    let mut s = String::new();
    s.push_str("// @generated by src-tauri/build.rs —— 不要手改\n");
    s.push_str("// 改骨架请改 skeleton/ 下的文件，重新构建即可。\n\n");
    s.push_str("/// 内置区的一个文件。内容由 build.rs 在编译期读入并烧进二进制。\n");
    s.push_str("pub struct SkeletonAsset {\n");
    s.push_str("    /// 相对数据目录的落点，恒用 `/` 分隔\n");
    s.push_str("    pub rel: &'static str,\n");
    s.push_str("    /// 全文（未渲染占位符）\n");
    s.push_str("    pub body: &'static str,\n");
    s.push_str("    /// 版本化模板：升级时只补缺失小节，绝不覆盖用户内容\n");
    s.push_str("    pub template: bool,\n");
    s.push_str("    /// 释放时替换 {{DATA_DIR}} / {{SKILL_DIR}}\n");
    s.push_str("    pub render: bool,\n");
    s.push_str("}\n\n");

    s.push_str("/// 需要建的目录。空目录文件树表达不了，所以在 manifest 里显式声明。\n");
    s.push_str("pub static SKELETON_DIRS: &[&str] = &[");
    for d in &m.dirs {
        s.push_str(&format!("{}, ", rust_str(d)));
    }
    s.push_str("];\n\n");

    s.push_str("/// 骨架树内容指纹。变了才需要重看一遍（且依然不覆盖用户内容）。\n");
    s.push_str(&format!(
        "pub const SKELETON_SEED: &str = {};\n\n",
        rust_str(seed)
    ));

    s.push_str("pub static SKELETON_ASSETS: &[SkeletonAsset] = &[\n");
    for a in &m.assets {
        if !a.distribute {
            continue;
        }
        let body = fs::read_to_string(root.join(&a.src)).unwrap_or_default();
        let _ = writeln!(
            s,
            "    SkeletonAsset {{ rel: {}, body: {}, template: {}, render: {} }},",
            rust_str(&a.rel),
            rust_str(&body),
            a.template,
            a.render
        );
    }
    s.push_str("];\n");
    s
}

// ===========================================================================
// main
// ===========================================================================

fn main() {
    selftest_sha256();

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("skeleton");
    let manifest_path = root.join("manifest.json");

    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = parse_manifest(&manifest_path);
    for a in &manifest.assets {
        println!("cargo:rerun-if-changed={}", root.join(&a.src).display());
    }

    guard_undeclared(&root, &manifest);
    guard_content(&root, &manifest);

    let seed = compute_seed(&root, &manifest);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    fs::write(out_dir.join("skeleton_manifest.rs"), codegen(&root, &manifest, &seed))
        .expect("写 skeleton_manifest.rs 失败");

    tauri_build::build()
}