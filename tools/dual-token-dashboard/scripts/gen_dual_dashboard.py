# -*- coding: utf-8 -*-
"""双数据源 Token 消耗看板构建脚本（WorkBuddy + float-agent 小 agent）

同一套看板渲染，两个数据源，顶部 tab 一键切换：
  · WorkBuddy  —— 扫 ~/.workbuddy/projects/**/*.jsonl 的 providerData.rawUsage/usage
  · 小 agent   —— 扫 float-agent/agent-data/sessions/*.json 的 messages[].usage

两边的 usage 都归一化成同一套内部记录，渲染逻辑完全复用。

跨平台：Windows / macOS / Linux 通用，仅依赖 Python 标准库（3.8+）。

用法：
    python gen_dual_dashboard.py                       # 输出 ./dual-token-dashboard.html
    python gen_dual_dashboard.py --out 看板.html        # 自定义输出路径
    python gen_dual_dashboard.py --projects <目录>      # WorkBuddy 日志目录
    python gen_dual_dashboard.py --agent-data <目录>    # 小 agent 数据目录
"""
import argparse, json, glob, os, sys, datetime

# Windows 控制台 GBK 环境下防止中文 print 报错
try:
    sys.stdout.reconfigure(encoding="utf-8")
except Exception:
    pass

_ap = argparse.ArgumentParser(description="双数据源 Token 消耗看板生成器（WorkBuddy + 小 agent）")
_ap.add_argument("--projects", default=os.path.join(os.path.expanduser("~"), ".workbuddy", "projects"),
                 help="WorkBuddy 会话日志目录（默认 ~/.workbuddy/projects）")
_ap.add_argument("--agent-data", default=None,
                 help="float-agent 数据目录（默认自动探测 float-agent/agent-data）")
_ap.add_argument("--out", default="dual-token-dashboard.html", help="输出 HTML 路径")
_ap.add_argument("--summary-out", default=None,
                 help="AI 可读摘要 JSON 路径（默认与 --out 同目录 dual-token-dashboard.summary.json）")
_args = _ap.parse_args()

PROJECTS = _args.projects
OUT = _args.out
SUMMARY_OUT = _args.summary_out or (
    os.path.join(os.path.dirname(os.path.abspath(OUT)) or ".",
                 "dual-token-dashboard.summary.json")
)


def _find_agent_data(explicit):
    """定位小 agent 的 agent-data 目录（内含 sessions/）。

    探测顺序：显式参数 → 脚本相对路径 → 常见工作区 → 用户目录。
    找不到就返回 None（看板里该 tab 显示"无数据"，不报错退出）。
    """
    cands = []
    if explicit:
        cands.append(explicit)
    here = os.path.dirname(os.path.abspath(__file__))
    # ① 脚本相对路径：scripts/ → dual-dashboard/ → tools/ → 项目根
    root = os.path.abspath(os.path.join(here, "..", "..", ".."))
    # ② 当前工作目录（skill 装在 ~/.workbuddy/skills/ 时，①会落空，靠 ② 兜底）
    cwd = os.getcwd()
    for base in (root, cwd, os.path.join(cwd, "..")):
        cands += [
            os.path.join(base, "float-agent", "agent-data"),
            os.path.join(base, "agent-data"),
        ]
    cands.append(os.path.join(os.path.expanduser("~"), "float-agent", "agent-data"))
    for c in cands:
        if c and os.path.isdir(os.path.join(c, "sessions")):
            return c
    # 兜底：连 sessions 都没有，但目录存在也认（该 tab 会空）
    for c in cands:
        if c and os.path.isdir(c):
            return c
    return None


AGENT_DATA = _find_agent_data(_args.agent_data)
def parse_ts(v):
    if isinstance(v, (int, float)):
        if v > 1e12: return v / 1000.0
        if v > 1e9: return float(v)
    if isinstance(v, str):
        s = v.strip()
        for fmt_ in ('%Y-%m-%dT%H:%M:%S.%fZ', '%Y-%m-%dT%H:%M:%SZ',
                     '%Y-%m-%d %H:%M:%S', '%Y-%m-%dT%H:%M:%S.%f'):
            try:
                import calendar
                return calendar.timegm(datetime.datetime.strptime(s, fmt_).timetuple())
            except Exception:
                pass
        try: return float(s) / 1000.0
        except Exception: return None
    return None

def grab(o):
    pd = o.get('providerData')
    if not isinstance(pd, dict): return None
    ru = pd.get('rawUsage') or {}; u = pd.get('usage') or {}
    tt = ru.get('total_tokens') or u.get('totalTokens')
    if not tt: return None
    model = pd.get('model') or pd.get('requestModelId') or pd.get('requestModelName') or 'unknown'
    ptd = ru.get('prompt_tokens_details') or {}
    ch = ru.get('prompt_cache_hit_tokens')
    if ch is None: ch = ptd.get('cached_tokens') or 0  # 兜底：DeepSeek 风格顶层字段 → OpenAI 风格嵌套字段（claude/qwen/glm）
    cw = ptd.get('cached_creation_tokens') or ptd.get('cache_write_tokens') or 0  # 缓存写入（Claude 按 1.25× 输入价计）
    return dict(tt=int(tt), it=int(ru.get('prompt_tokens') or u.get('inputTokens') or 0),
                ot=int(ru.get('completion_tokens') or u.get('outputTokens') or 0),
                ch=int(ch or 0), cw=int(cw or 0), model=model)

def session_title(o, fallback):
    """取首条非 system-reminder 的用户消息前 42 字作为任务标题；跳过 < 开头的内部标签消息"""
    if o.get('role') != 'user': return None
    c = o.get('content')
    if isinstance(c, str):
        t = ' '.join(c.split())
        if t and not t.startswith('<'):
            return t[:42]
        return None
    if not isinstance(c, list): return None
    for seg in c:
        if isinstance(seg, dict) and seg.get('type') == 'input_text':
            t = (seg.get('text') or '').strip()
            if not t: continue
            if t.startswith('<'): return None
            t = ' '.join(t.split())
            return t[:42]
        if isinstance(seg, dict) and seg.get('type') == 'text':
            t = (seg.get('text') or '').strip()
            if not t: continue
            if t.startswith('<'): return None
            t = ' '.join(t.split())
            return t[:42]
    return None

MODEL_MERGE={'glm-5.2-x':'glm-5.2'}

def short_ws(name):
    """工作空间目录名是路径 slug（如 c-Users-foo-WorkBuddy-2026-08-01-10-00-00），
    取 WorkBuddy 之后的部分作为短名；非 WorkBuddy 目录（如桌面工作区）原样返回"""
    i = name.rfind('-WorkBuddy-')
    if i >= 0 and name[i + 11:]:
        return name[i + 11:]
    return name

# ===========================================================================
# 数据源层（DataSource）
# ---------------------------------------------------------------------------
# 每个数据源 = 一个独立扫描器，唯一职责：把某处的日志读成**统一的内部记录**。
# 新增第三个数据源 = 写一个 scan_xxx() 并注册进 SOURCES —— 面板层完全不用动。
#
# 统一 schema（每个源一份；键名与前端 JS 严格对应）：
#   key     str     源标识（前端 tab 的 data-src）
#   name    str     显示名（tab 文字）
#   genMs   int     生成时刻 ms（时间窗过滤的基准）
#   gen     str     生成时刻可读串
#   ws      [str]   分组名（WorkBuddy=工作空间；小 agent=单一分组）
#   models  [str]   模型 id 表（记录里存的是下标）
#   sess    [obj]   会话：{w:分组下标, id:短id, t:标题, st:起始分钟, r:[记录元组]}
#
# 记录元组（定长，压体积的关键）：
#   (分钟戳, 模型下标, total, input, output, 缓存命中, 缓存写入)
# ===========================================================================

def _build_sess_list(sessions):
    """{sid: {'w','id','t','r'}} → 排序后的会话列表（面板层统一消费）"""
    out = []
    for sid, d in sessions.items():
        if not d['r']:
            continue
        out.append({'w': d['w'], 'id': sid[:8], 't': d['t'] or '(无标题会话)',
                    'st': d['r'][0][0], 'r': d['r']})
    out.sort(key=lambda s: s['st'])
    return out


def scan_workbuddy(projects=PROJECTS):
    """数据源①：WorkBuddy —— 扫 `~/.workbuddy/projects/**/*.jsonl` 的请求级 usage。

    深层子代理日志（`subagents/agent-*.jsonl`）自动归属父会话。
    """
    ws_names, ws_idx = {}, {}
    models, model_idx = [], {}
    sessions = {}
    for fp in glob.glob(os.path.join(projects, '**', '*.jsonl'), recursive=True):
        rel = os.path.relpath(fp, projects)
        parts = rel.split(os.sep)
        if not parts:
            continue
        folder = parts[0]
        # 二级文件 = 会话文件；三级及更深（subagents/agent-*.jsonl）归属父会话
        sid = os.path.splitext(parts[1])[0] if len(parts) == 2 else parts[1]
        if folder not in ws_idx:
            ws_names[folder] = short_ws(folder)
            ws_idx[folder] = len(ws_idx)
        recs, title = [], None
        with open(fp, encoding='utf-8', errors='ignore') as fh:
            for line in fh:
                try: o = json.loads(line)
                except Exception: continue
                if not isinstance(o, dict): continue
                sid_r = o.get('sessionId')
                if sid_r and len(parts) == 2: sid = sid_r  # 子代理文件保持归属父会话
                if title is None:
                    t = session_title(o, sid)
                    if t: title = t
                r = grab(o)
                if not r: continue
                r['model'] = MODEL_MERGE.get(r['model'], r['model'])
                ts = parse_ts(o.get('timestamp'))
                if not ts: continue
                if r['model'] not in model_idx:
                    model_idx[r['model']] = len(models); models.append(r['model'])
                recs.append((int(ts // 60), model_idx[r['model']], r['tt'], r['it'], r['ot'], r['ch'], r['cw']))
        if not recs: continue
        recs.sort(key=lambda x: x[0])
        d = sessions.setdefault(sid, {'w': ws_idx[folder], 'id': sid, 't': title, 'r': []})
        d['r'].extend(recs)
    return {'key': 'workbuddy', 'name': 'WorkBuddy',
            'ws': list(ws_names.values()), 'models': models,
            'sess': _build_sess_list(sessions)}


def scan_agent(agent_data=AGENT_DATA):
    """数据源②：小 agent —— 扫 `<agent-data>/sessions/*.json` 的 `messages[].usage`。

    与 WorkBuddy 的差异：
      · 没有 cache_write 概念（我方未记录）→ 置 0；
      · 模型取 `messages[].model`；
      · 时间戳是 `messages[].at`（epoch ms）；
      · 整个 agent 视作**单一分组**（ws 只有一项）。
    """
    sessions = {}
    models, model_idx = [], {}
    if not agent_data:
        return {'key': 'agent', 'name': '小 agent', 'ws': [], 'models': [], 'sess': []}
    for fp in sorted(glob.glob(os.path.join(agent_data, 'sessions', '*.json'))):
        try:
            with open(fp, encoding='utf-8') as fh:
                o = json.load(fh)
        except Exception:
            continue
        if not isinstance(o, dict): continue
        sid = o.get('id') or os.path.splitext(os.path.basename(fp))[0]
        recs = []
        for m in o.get('messages', []):
            if not isinstance(m, dict) or m.get('role') != 'assistant': continue
            u = m.get('usage')
            if not isinstance(u, dict): continue
            tt = int(u.get('total') or 0)
            ts = m.get('at')
            if not tt or not ts: continue
            mid = m.get('model') or 'unknown'
            if mid not in model_idx:
                model_idx[mid] = len(models); models.append(mid)
            recs.append((int(int(ts) // 60000), model_idx[mid], tt,
                         int(u.get('prompt') or 0), int(u.get('completion') or 0), 0, 0))
        if not recs: continue
        recs.sort(key=lambda x: x[0])
        sessions[sid] = {'w': 0, 'id': sid, 't': o.get('title'), 'r': recs}
    return {'key': 'agent', 'name': '小 agent',
            'ws': ['小 agent'] if sessions else [], 'models': models,
            'sess': _build_sess_list(sessions)}


# 注册表 —— 面板层只认这个列表，不关心具体实现
SOURCES = [scan_workbuddy, scan_agent]

now = datetime.datetime.now()
gen_ms = int(now.timestamp() * 1000)
gen_str = now.strftime('%Y-%m-%d %H:%M')

src_map = {}
src_order = []
for _fn in SOURCES:
    _s = _fn()
    _s['genMs'] = gen_ms
    _s['gen'] = gen_str
    src_map[_s['key']] = _s
    src_order.append(_s['key'])

DATA = {'genMs': gen_ms, 'gen': gen_str,
        'order': src_order,
        'sources': src_map}
js = json.dumps(DATA, ensure_ascii=False, separators=(',', ':')).replace('</', '<\\/')

TEMPLATE = r'''<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Token 消耗看板</title>
<style>
*{box-sizing:border-box;margin:0;padding:0}
:root{--bg:#fafaf9;--card:#fff;--border:#e8e5df;--border2:#e4e1da;--border3:#efede8;--t1:#1f2328;--t2:#6f6c64;--t3:#9a978f;--t4:#b5b2a9;--t5:#a3a099;--t6:#55524b;--wcnm:#374151;--tdc:#3d3b36;--row:#f2f0ea;--empty:#f4f2ed;--hover:#fbfaf7;--focus:#c9c6be;--sep:#d4d1c9;--selbg:#e9f0f7;--selfg:#3f6b99;--selbd:#aec9e0;--red:#d64545;--blue:#5f8cb6;--tipbg:#1f2328;--tiptc:#fff}
body.dark{--bg:#1c1b19;--card:#262521;--border:#3a3833;--border2:#3a3833;--border3:#35332e;--t1:#f0eee9;--t2:#b5b2a9;--t3:#8a877f;--t4:#6f6d66;--t5:#8a877f;--t6:#d5d2ca;--wcnm:#c5c2ba;--tdc:#c5c2ba;--row:#323029;--empty:#2e2c29;--hover:#2a2925;--focus:#55524b;--sep:#4a4842;--selbg:#2c3a47;--selfg:#a9c9e4;--selbd:#4a6a85;--red:#e06666;--blue:#8fb8d9;--tipbg:#f0eee9;--tiptc:#1c1b19;color-scheme:dark}
body{font-family:-apple-system,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;background:var(--bg);color:var(--t1);padding:22px 18px 60px;max-width:1600px;margin:0 auto}
.wrap{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr);gap:14px;align-items:start}
.colL,.colR{min-width:0}
h1{font-size:20px;font-weight:700;letter-spacing:.3px}
.sub{font-size:11px;color:var(--t3);margin-top:3px;display:flex;align-items:center;gap:8px}
.tbtn{border:1px solid var(--border2);background:var(--card);color:var(--t2);font-size:11px;padding:1px 10px;border-radius:999px;cursor:pointer}
.tbtn:hover{border-color:var(--focus);color:var(--t1)}
header{display:flex;justify-content:space-between;align-items:flex-end;margin-bottom:16px}
.pill{border:1px solid var(--border2);background:var(--card);color:var(--t2);font-size:12px;padding:5px 14px;border-radius:6px;cursor:pointer;margin-left:6px}
.pill.on{background:var(--selbg);color:var(--selfg);border-color:var(--selbd)}
.datepick{border:1px solid var(--border2);background:var(--card);color:var(--t2);font-size:12px;padding:4px 8px;border-radius:6px;outline:none;vertical-align:middle}
.datepick:focus{border-color:var(--focus)}
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:16px 18px;margin-bottom:14px}
.sec-h{display:flex;justify-content:space-between;align-items:center;margin-bottom:12px;flex-wrap:wrap;gap:6px}
.sec-h h2{font-size:14px;font-weight:400}
.kpis{display:grid;grid-template-columns:repeat(7,1fr);gap:10px;margin-bottom:14px}
.kpi{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:14px 15px;min-width:0}
.kpi .k{font-size:11px;color:var(--t3);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.kpi .v{font-size:20px;font-weight:700;color:var(--t1);margin-top:6px;white-space:nowrap}
.kpi .v small{font-size:12px;color:var(--t3);font-weight:600;margin-left:3px}
.t5{display:flex;align-items:center;justify-content:space-between;gap:6px;background:var(--card);border:1px solid var(--border);border-radius:8px;padding:5px 9px;min-width:0;cursor:pointer;transition:background .15s,border-color .15s}
.t5:hover{border-color:var(--focus)}
.t5.on{background:var(--selbg);border-color:var(--selbd)}
.t5.on .k,.t5.on .v{color:var(--selfg)}
.t5 .k{font-size:9px;color:var(--t5);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.t5 .k i{width:6px;height:6px;margin-right:3px;vertical-align:0}
.t5 .v{font-size:11px;font-weight:600;color:var(--t6);white-space:nowrap;flex:none}
.heat-wrap{max-height:640px;overflow-y:auto;border:1px solid var(--border3);border-radius:8px;padding:6px 8px}
.hrow{display:flex;align-items:center;gap:2px;margin-bottom:-1px}
.hlab{width:42px;flex:none;font-size:10px;color:var(--t3)}
.hcell{flex:1;height:7.3px;border-radius:0;background:var(--empty);cursor:default}
.hhead{display:flex;gap:2px;margin:0 1px 4px;padding-left:8px;padding-right:8px}
.hhead span{flex:1;min-width:0;text-align:center;font-size:9px;color:var(--t4)}
.legend{display:flex;align-items:center;gap:4px;font-size:10px;color:var(--t3)}
.legend i{width:14px;height:10px;border-radius:2px;display:inline-block}
.caption{font-size:11px;color:var(--t3);margin-bottom:6px}
.bandbar{display:flex;height:14px;border-radius:5px;overflow:hidden;margin:8px 0 10px}
.bandbar div{height:100%}
.bandrow{display:flex;justify-content:space-between;font-size:11px;color:var(--t2);margin-bottom:3px}
.tools{display:flex;gap:6px;align-items:center}
.tools input{border:1px solid var(--border2);border-radius:6px;padding:5px 10px;font-size:12px;width:110px;background:var(--card);color:var(--t1);outline:none}
.tools input:focus{border-color:var(--focus)}
.chip{border:1px solid var(--border2);background:var(--card);color:var(--t2);font-size:11px;padding:4px 10px;border-radius:6px;cursor:pointer}
.chip.on{background:var(--selbg);color:var(--selfg);border-color:var(--selbd)}
.crumb{font-size:12px;color:var(--t3);margin-bottom:10px;display:none;align-items:center;gap:10px}
.crumb a{color:var(--t2);text-decoration:none;cursor:pointer}
.crumb a:hover{color:var(--selfg)}
.crumb .cur{color:var(--selfg)}
.crumb .sep{margin:0 6px;color:var(--sep)}
.lrow{display:flex;align-items:center;gap:10px;padding:8px 4px;border-bottom:1px solid var(--row);cursor:pointer}
.lrow:hover{background:var(--hover)}
.lrow .nm{flex:1.4;min-width:0;font-size:12.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lrow .meta{flex:.8;font-size:11px;color:var(--t3);white-space:nowrap}
.lrow .bar{flex:1.2;height:7px;background:var(--empty);border-radius:4px;overflow:hidden}
.lrow .bar i{display:block;height:100%;border-radius:4px;background:var(--blue)}
.lrow .num{flex:.9;text-align:right;font-size:12px;font-weight:700;white-space:nowrap}
.lrow .num em{font-style:normal;color:var(--t4);font-weight:600;font-size:10px;margin-left:4px}
.empty{font-size:12px;color:var(--t3);padding:20px 0;text-align:center}
.subgrid{display:grid;grid-template-columns:minmax(0,1fr);gap:0}
.wcard{display:flex;align-items:center;gap:10px;padding:3px 2px;border-bottom:1px solid var(--row);cursor:pointer;background:transparent;min-width:0}
.wcard:hover{background:var(--hover)}
.wc-nm{flex:none;width:16%;min-width:0;font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--wcnm)}
.wc-tt{flex:none;width:72px;text-align:right;font-size:13px;font-weight:400;color:var(--t1);white-space:nowrap}
.wc-meta{flex:none;width:88px;text-align:right;font-size:10px;color:var(--t3);white-space:nowrap}
.wviz{flex:1.5;display:flex;gap:1px;min-width:0;overflow:hidden}
.wcell{flex:1 1 0;min-width:0;height:10.4px;border-radius:0;background:var(--empty)}
.panel-scroll{max-height:290px;overflow-y:auto;border:1px solid var(--border3);border-radius:8px;padding:2px 10px}
.wsn{flex:none;font-size:10px;border:1px solid var(--border2);border-radius:5px;padding:1px 3px;background:var(--card);color:var(--t2);outline:none}
.band4{display:flex;gap:6px;margin-bottom:12px}
.b4{flex:1;display:flex;justify-content:space-between;align-items:baseline;gap:6px;border:1px solid var(--border3);border-radius:6px;padding:5px 9px;background:var(--card);min-width:0}
.b4k{font-size:10px;color:var(--t3);white-space:nowrap}
.b4v{font-size:12.5px;font-weight:700;color:var(--t1);white-space:nowrap}
.kpi .s{font-size:10px;color:var(--t4);margin-top:2px;white-space:nowrap}
.view-h{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px}
.view-t{font-size:13px;font-weight:700}
.view-m{font-size:11px;color:var(--t3)}
svg text{font-family:inherit}
table.rt{width:100%;border-collapse:collapse;font-size:11px}
table.rt th{position:sticky;top:0;background:var(--card);text-align:right;color:var(--t3);font-weight:600;padding:5px 6px;border-bottom:1px solid var(--border)}
table.rt th:first-child,table.rt td:first-child{text-align:left}
table.rt td{text-align:right;padding:4px 6px;border-bottom:1px solid var(--row);color:var(--tdc);white-space:nowrap}
.rt-wrap{max-height:320px;overflow:auto;border:1px solid var(--border3);border-radius:8px}
#tip{position:fixed;background:var(--tipbg);color:var(--tiptc);padding:7px 10px;border-radius:7px;font-size:11px;pointer-events:none;opacity:0;transition:opacity .08s;white-space:pre;z-index:99;line-height:1.55}
/* 数据源 tab（WorkBuddy / 小 agent 切换） */
.tabs{display:flex;gap:6px;margin:9px 0 7px}
.tab{display:flex;align-items:center;gap:6px;border:1px solid var(--border2);background:var(--card);color:var(--t2);font-size:12.5px;font-weight:600;padding:5px 14px;border-radius:8px;cursor:pointer;transition:background .15s,border-color .15s,color .15s}
.tab:hover{border-color:var(--focus);color:var(--t1)}
.tab.on{background:var(--selbg);color:var(--selfg);border-color:var(--selbd)}
.tab-n{font-size:10px;font-weight:600;color:var(--t4);font-variant-numeric:tabular-nums}
.tab.on .tab-n{color:var(--selfg);opacity:.75}
</style></head><body>

<header style="align-items:flex-end">
  <div>
    <h1>Token 消耗看板</h1>
    <div id="srcTabs" class="tabs"></div>
    <div class="sub"><span id="genAt"></span><button class="tbtn" id="themeBtn" type="button">深色</button></div>
  </div>
  <div style="flex:none;white-space:nowrap">
    <button class="pill" data-w="1">1 天</button><button class="pill" data-w="3">3 天</button><button class="pill" data-w="7">近 7 天</button><button class="pill on" data-w="30">近 30 天</button><input type="date" id="dStart" class="datepick" title="起始日期"><span style="color:var(--t4);font-size:11px;margin:0 2px">至</span><input type="date" id="dEnd" class="datepick" title="结束日期">
  </div>
</header>

<section class="kpis" id="kpis"></section>

<div class="wrap"><div class="colL">

<section class="card">
  <div class="sec-h"><h2>工作空间活跃分布</h2>
    <div class="tools" id="wsTools"><button class="chip" data-m="ses">按会话</button><button class="chip on" data-m="ws">按工作空间</button><input id="wsSearch" placeholder="搜索…"><span id="wsSorts" style="display:none"><button class="chip on" data-s="tt">按 Token</button><button class="chip" data-s="n">按会话数</button></span><select id="wsMult" class="wsn" title="可视化倍率：方块总数=窗口天数×倍率"><option value="1">1X</option><option value="2">2X</option><option value="3" selected>3X</option><option value="4">4X</option><option value="5">5X</option></select></div>
  </div>
  <div class="crumb" id="crumb"></div>
  <div class="panel-scroll"><div id="panel"></div></div>
</section>

<section class="card">
  <div class="sec-h"><h2>单次请求大小分布</h2><span class="caption" style="margin:0" id="mdCap"></span></div>
  <svg id="mdSvg" width="100%" height="240"></svg>
  <div style="font-size:10px;color:var(--t3);margin-top:6px">每模型一条分布形状（各自归一）· 竖线=该模型 50% · 灰点=90% · 虚线=全局 50% · 数值下条形=两列共享刻度的大小对比（深=50% 浅=90%）</div>
</section>
</div>

<div class="colR">
<section class="card" id="heatCard">
  <div class="sec-h"><h2>日活分布</h2><div class="legend" id="heatLegend"></div></div>
  <div class="band4" id="band4"></div>
  <div class="hhead" id="hhead"></div>
  <div class="heat-wrap" id="heatWrap"></div>
</section>

<section class="card">
  <div class="sec-h"><h2>0–24 时分布 · 按模型</h2><div class="legend" id="rdLegend" style="flex-wrap:wrap;justify-content:flex-end"></div></div>
  <svg id="rdSvg" width="100%" height="112"></svg>
</section>

<section class="card">
  <div class="sec-h"><h2>每日模型用量比例</h2></div>
  <svg id="dmSvg" width="100%" height="102"></svg>
  <div style="font-size:10px;color:var(--t3);margin-top:4px">x=日期（最新在右）· y=当日 Token · 颜色=模型 · 点击下方模型卡片高亮单模型</div>
  <section class="kpis" id="top5" style="margin:12px 0 0;gap:6px;grid-template-columns:repeat(5,1fr)"></section>
</section>
</div></div>

<div id="tip"></div>

<script>const PAYLOAD=__DATA__;</script>
<script>
const $=s=>document.querySelector(s);

// ---- 面板层：数据源切换 ----
// 面板渲染只依赖全局 D；切换 tab 时把 D 指向另一个数据源再整体重绘。
// 渲染函数一行都不用改 —— 这就是数据源层与面板层解耦的收益。
const SRC_ORDER=PAYLOAD.order;
let SRC_KEY=SRC_ORDER[0];
try{const s=localStorage.getItem('td-src');if(s&&PAYLOAD.sources[s])SRC_KEY=s;}catch(e){}
let D=PAYLOAD.sources[SRC_KEY];
function switchSrc(k){
  if(!PAYLOAD.sources[k]||k===SRC_KEY)return;
  SRC_KEY=k;D=PAYLOAD.sources[k];
  try{localStorage.setItem('td-src',k);}catch(e){}
  // 换源 = 换数据集，下钻状态必须清空（否则会拿着旧下标去索引新数组）
  state={ws:null,ses:null};q='';wsMode='ws';wsSort='tt';
  const si=document.getElementById('wsSearch');if(si)si.value='';
  const ws2=document.getElementById('wsMult');if(ws2)ws2.value='3';wsMult=3;
  syncChrome();
  render();
}
const MCL=['#4e7fbc','#c8894a','#59a5a0','#b05a5a','#8172b3','#7a9a4e','#c98ba6','#9aa8bd'];
const MCD=['#7aa5d4','#d8a76b','#6cc0ba','#d48080','#a798cf','#9dba6e','#d8a3b8','#aab6c8'];
const RMPL=['#f4f2ed','#e8eef5','#cfddeb','#aec9e0','#88abd0','#5f8cb6','#3f6b99','#2b4f73'];
const RMPD=['#2e2c29','#33404d','#3a4f61','#42607a','#4b6f8f','#5682a6','#6b9abf','#8fb8d9'];
const CJL={grid:'#f0ede7',axis:'#b5b2a9',axis2:'#c9c6be',blue:'#5f8cb6',dash:'#c9c6be',base:'#d4d1c9',dim:'#c9c6be',dimBar:'#e4e1da',other:'#8C8C8C',red:'#d64545',muted:'#9a978f',t1:'#1f2328'};
const CJD={grid:'#323029',axis:'#6f6d66',axis2:'#6f6d66',blue:'#8fb8d9',dash:'#55524b',base:'#4a4842',dim:'#55524b',dimBar:'#3a3833',other:'#7d8b96',red:'#e06666',muted:'#8a877f',t1:'#f0eee9'};
let DARK=false,CJ=CJL,MC=MCL,RAMP=RMPL;
try{if(localStorage.getItem('td-dark')==='1'){DARK=true;CJ=CJD;MC=MCD;RAMP=RMPD;}}catch(e){}
const modelColor=i=>MC[i%MC.length];
// 预估单价（元 / 百万 token）：[输入, 输出, 缓存命中]（官方价）；成本按每条请求的实际缓存计：读命中走缓存价、缓存写入（Claude）按 1.25× 输入价、其余输入走输入价
const PRICE={'deepseek-v4-flash':[1,4,0.2],'deepseek-v4-pro':[2,8,0.5],'deepseek':[2,8,0.5],'glm-5.3':[2,8,0.4],'glm-5.2-x':[1.2,6,0.24],'glm-5.2':[0.8,3.2,0.16],'glm':[1,4,0.2],'kimi':[1.5,6,0.3],'claude-opus-5':[150,750,15],'claude-opus-4-8':[110,550,11],'claude':[110,550,11],'qwen':[2.4,9.6,0.48],'hy3':[4,16,0.8],'hy':[4,16,0.8],'minimax':[1,5,0.2],'m3':[1,5,0.2]};
const priceOf=name=>{const n=(name||'').toLowerCase();for(const k in PRICE)if(n.startsWith(k))return PRICE[k];return [2,8,0.4];};
const MP=D.models.map(m=>priceOf(m));
let win=30, customStart=0, customEnd=0, wsSort='tt', q='', state={ws:null,ses:null};
let wsMult=3,PCTX=null,wsMode='ws',SHD=new Map();
function vizCells(m){
  const days=PCTX?PCTX.days:[],avgD=PCTX?PCTX.avgD:1;
  let h='';
  for(const dk of days){
    const arr=(m&&(m[dk]||m.get&&m.get(dk)))||new Array(24).fill(0);
    for(let k=0;k<wsMult;k++){
      const a=Math.floor(24*k/wsMult),b=Math.floor(24*(k+1)/wsMult);
      let v=0;for(let hh=a;hh<b;hh++)v+=arr[hh]||0;
      const l=v==0?0:Math.min(7,Math.max(1,1+Math.floor(Math.log(v/avgD/0.001)/Math.log(350)*7)));
      h+='<div class="wcell" style="background:'+RAMP[l]+'" title="'+dk+' '+a+':00–'+b+':00 · '+fmt(v)+' token"></div>';
    }
  }
  return h;
}
function buildViz(w){
  const pc=PCTX||{whd:{},days:[],avgD:1};
  const m=pc.whd[w]||{};
  let h='';
  for(const dk of pc.days){
    const arr=m[dk]||new Array(24).fill(0);
    for(let k=0;k<wsMult;k++){
      const a=Math.floor(24*k/wsMult),b=Math.floor(24*(k+1)/wsMult);
      let v=0;for(let hh=a;hh<b;hh++)v+=arr[hh]||0;
      const lv=v==0?0:Math.min(7,Math.max(1,1+Math.floor(Math.log(v/pc.avgD/0.001)/Math.log(350)*7)));
      h+='<div class="wcell" style="background:'+RAMP[lv]+'" title="'+dk+' '+a+':00–'+b+':00 · '+fmt(v)+' token"></div>';
    }
  }
  return h||'<div class="wcell"></div>';
}
// ---- 顶部：数据源 tab + 数据时间戳 ----
const genMs=PAYLOAD.genMs;   // 时间窗基准取生成时刻（两个源共用，避免切源时窗口跳动）
function syncChrome(){
  const tb=$('#srcTabs');
  if(tb){
    tb.innerHTML=SRC_ORDER.map(k=>{
      const s=PAYLOAD.sources[k];
      const n=s.sess.reduce((a,x)=>a+x.r.length,0);
      const on=k===SRC_KEY?' on':'';
      return '<button class="tab'+on+'" data-src="'+k+'" type="button">'+esc(s.name)+
             '<span class="tab-n">'+nf(n)+'</span></button>';
    }).join('');
    tb.querySelectorAll('[data-src]').forEach(b=>b.onclick=()=>switchSrc(b.dataset.src));
  }
  const ga=$('#genAt');
  if(ga){
    const n=D.sess.reduce((a,x)=>a+x.r.length,0);
    ga.textContent=D.name+' · 数据截至 '+D.gen+' · 请求级真实 usage · 共 '+nf(n)+' 次请求';
  }
}
const cut=()=>win?genMs-win*86400e3:(customStart?customStart*60000:0);
const cutHi=()=>(!win&&customEnd)?customEnd*60000:Infinity;
function fmt(n){if(n>=1e8)return (n/1e8).toFixed(2)+' 亿';if(n>=1e4)return (n/1e4).toFixed(1)+' 万';return String(Math.round(n));}
const nf=n=>n.toLocaleString('en-US');
const esc=s=>String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
function hm(n){return new Date(n*60000);}
function fdate(d){const p=x=>String(x).padStart(2,'0');return (d.getMonth()+1)+'-'+p(d.getDate());}
function ftime(d){const p=x=>String(x).padStart(2,'0');return p(d.getHours())+':'+p(d.getMinutes());}

function iterReq(cb){
  const c=cut();
  for(const s of D.sess){
    let tt=0,tn=0;
    const reqs=[];
    for(const r of s.r){const ts=r[0]*60000;if(ts<c)continue;reqs.push(r);tt+=r[2];tn++;}
    if(tn)cb(s,reqs,tt,tn);
  }
}
function aggregate(){
  const c=cut(),hiC=cutHi();
  const daily={},ws={},band=[0,0,0,0];
  const hr=new Array(24).fill(0);
  const mTot=new Array(D.models.length).fill(0);
  const dm={};
  let tot=0,inp=0,out=0,ch=0,turns=0,nsess=0,cost=0;
  const sessAgg=[];
  for(const s of D.sess){
    let tt=0,tn=0,first=0,last=0;
    for(const r of s.r){const ts=r[0]*60000;if(ts<c||ts>=hiC)continue;
      const d=hm(r[0]);const dk=fdate(d);const h=d.getHours();
      (daily[dk]=daily[dk]||{h:new Array(24).fill(0),hh:new Array(72).fill(0),tt:0});
      daily[dk].h[h]+=r[2];daily[dk].tt+=r[2];
      daily[dk].hh[h*3+Math.floor(d.getMinutes()/20)]+=r[2];
      hr[h]+=r[2];mTot[r[1]]+=r[2];
      (dm[dk]=dm[dk]||{});dm[dk][r[1]]=(dm[dk][r[1]]||0)+r[2];
      const wn=s.w;ws[wn]=ws[wn]||{tt:0,n:0};ws[wn].tt+=r[2];
      const bi=Math.floor(h/6);band[bi]+=r[2];
      tot+=r[2];inp+=r[3];out+=r[4];ch+=r[5];turns++;tn++;tt+=r[2];
      const pp=MP[r[1]];const cht=Math.max(0,Math.min(r[5],r[3]));const cwt=Math.max(0,Math.min(r[6]||0,r[3]-cht));cost+=(cht*pp[2]+cwt*1.25*pp[0]+(r[3]-cht-cwt)*pp[0]+r[4]*pp[1])/1e6;
      if(!first)first=r[0];last=r[0];}
    if(tn){nsess++;(ws[s.w]=ws[s.w]||{tt:0,n:0}).n+=1;sessAgg.push({s,tt,tn,first,last});}
    else s._=0;
  }
  const wsArr=D.ws.map((nm,i)=>({i,n:nm,tt:(ws[i]||{tt:0}).tt,cnt:(ws[i]||{n:0}).n})).filter(x=>x.tt>0);
  return {daily,wsArr,band,tot,inp,out,ch,turns,nsess,sessAgg,hr,mTot,dm,cost};
}

function renderKpis(A){
  const days=Object.keys(A.daily).length||1;
  const ce=A.inp?A.ch/A.inp*100:0;
  const money=A.cost>=1e4?'¥'+(A.cost/1e4).toFixed(1)+' 万':'¥'+A.cost.toFixed(1);
  $('#kpis').innerHTML=[
    ['合计 Token',fmt(A.tot)],
    ['日均（有数据天）',fmt(A.tot/days)],
    ['会话',nf(A.nsess)],
    ['轮次',nf(A.turns)],
    ['输入 / 输出比',(A.out?Math.round(A.inp/A.out):0)+':1'],
    ['缓存命中率',ce.toFixed(1)+'%'],
    ['预估金额（元）',money]
  ].map(x=>'<div class="kpi"><div class="k">'+x[0]+'</div><div class="v">'+x[1]+'</div></div>').join('');
}

function renderHeat(A){
  const dates=Object.keys(A.daily).sort().reverse();
  const head=$('#hhead');
  head.innerHTML='<span style="width:42px;flex:none"></span>'+Array.from({length:72},(_,i)=>'<span style="text-align:left">'+(i%3==0?(i/3):'')+'</span>').join('');
  // 每小时 3 格（20 分钟粒度）；色阶：最深 = 窗口日均×0.35，最浅 = 窗口日均×0.001（对数插值）
  const avg=A.tot/(dates.length||1);
  const wrap=$('#heatWrap');let html='';
  for(const d of dates){
    html+='<div class="hrow"><div class="hlab">'+d+'</div>';
    for(let i=0;i<72;i++){
      const v=A.daily[d].hh?A.daily[d].hh[i]:0;
      const lv=v==0?0:Math.min(7,Math.max(1,1+Math.floor(Math.log(v/avg/0.001)/Math.log(350)*7)));
      const h0=Math.floor(i/3),m0=(i%3)*20,m1=m0+20,pm=x=>String(x).padStart(2,'0');
      const tip=d+' '+h0+':'+pm(m0)+'–'+(m1==60?h0+1:h0)+':'+(m1==60?'00':pm(m1))+'\n'+nf(v)+' token（占当日 '+(A.daily[d].tt?(v/A.daily[d].tt*100).toFixed(1):0)+'%）';
      html+='<div class="hcell" style="background:'+RAMP[lv]+'" data-tip="'+tip.replace(/\n/g,'&#10;')+'"></div>';
    }
    html+='</div>';
  }
  wrap.innerHTML=html||'<div class="empty">该时间范围内无数据</div>';
  let lg='<span>低</span>';for(let i=1;i<8;i++)lg+='<i style="background:'+RAMP[i]+'"></i>';lg+='<span>高</span>';
  $('#heatLegend').innerHTML=lg;
}
document.addEventListener('mousemove',e=>{
  const t=e.target;
  const tip=$('#tip');
  if(t.classList&&t.classList.contains('hcell')&&t.dataset.tip){
    tip.textContent=t.dataset.tip;tip.style.opacity=1;tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
  }else if(t.tagName==='circle'&&t.dataset.tip){
    tip.textContent=t.dataset.tip;tip.style.opacity=1;tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
  }else if(t.tagName==='path'&&t.dataset.tip){
    tip.textContent=t.dataset.tip;tip.style.opacity=1;tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
  }else if(t.tagName==='td'&&t.closest('tr')&&t.closest('tr').dataset.tip){
    tip.textContent=t.closest('tr').dataset.tip;tip.style.opacity=1;tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
  }else if(t.tagName==='rect'&&t.dataset.tip){
    tip.textContent=t.dataset.tip;tip.style.opacity=1;tip.style.left=(e.clientX+12)+'px';tip.style.top=(e.clientY+12)+'px';
  }else{tip.style.opacity=0;}
});

function smoothLine(pts){
  // 单调三次插值（Fritsch-Carlson）：平滑且不过冲
  const n=pts.length;
  if(n<3)return 'M'+pts.map(p=>p[0].toFixed(1)+' '+p[1].toFixed(1)).join(' L');
  const dx=[],m=[],t=new Array(n);
  for(let i=0;i<n-1;i++)dx.push(pts[i+1][0]-pts[i][0]);
  for(let i=0;i<n-1;i++)m.push((pts[i+1][1]-pts[i][1])/dx[i]);
  t[0]=m[0];t[n-1]=m[n-2];
  for(let i=1;i<n-1;i++)t[i]=(m[i-1]*m[i]<=0)?0:(m[i-1]+m[i])/2;
  for(let i=0;i<n-1;i++){
    if(m[i]===0){t[i]=0;t[i+1]=0;continue;}
    const a=t[i]/m[i],b=t[i+1]/m[i],s=a*a+b*b;
    if(s>9){const f=3/Math.sqrt(s);t[i]=f*a*m[i];t[i+1]=f*b*m[i];}
  }
  let d='M'+pts[0][0].toFixed(1)+' '+pts[0][1].toFixed(1);
  for(let i=0;i<n-1;i++){
    const h=dx[i];
    d+=' C'+(pts[i][0]+h/3).toFixed(1)+' '+(pts[i][1]+t[i]*h/3).toFixed(1)+' '+(pts[i+1][0]-h/3).toFixed(1)+' '+(pts[i+1][1]-t[i+1]*h/3).toFixed(1)+' '+pts[i+1][0].toFixed(1)+' '+pts[i+1][1].toFixed(1);
  }
  return d;
}
function renderBand4(A){
  const names=['凌晨 0-6','上午 6-12','下午 12-18','晚间 18-24'];
  const tot=A.band.reduce((a,b)=>a+b,0)||1;
  $('#band4').innerHTML=A.band.map((v,i)=>'<div class="b4"><div class="b4k">'+names[i]+'</div><div class="b4v">'+(v/tot*100).toFixed(1)+'%</div></div>').join('');
}

let selModel=null,LASTA=null;
function topModels(A){
  const idx=D.models.map((m,i)=>i).filter(i=>A.mTot[i]>0).sort((a,b)=>A.mTot[b]-A.mTot[a]);
  return {top:idx.slice(0,8)};
}
// 按当前窗口用量排名取色：用量越大越靠前，使用 MC 中的蓝色系
function rankColor(A){
  const order=D.models.map((m,i)=>i).sort((a,b)=>(A.mTot[b]||0)-(A.mTot[a]||0));
  const map={};order.forEach((mi,r)=>map[mi]=MC[r%MC.length]);return map;
}
let hrSel=null;
function hourData(){
  const c=cut(),hiC=cutHi();
  const vals=Array.from({length:24},()=>[]),hrTot=new Array(24).fill(0),dom=new Array(24).fill(-1);
  const mH=Array.from({length:24},()=>({})),mC=Array.from({length:24},()=>({}));
  let gmax=0;
  for(const s of D.sess)for(const r of s.r){const ts=r[0]*60000;if(ts<c||ts>=hiC)continue;const h=hm(r[0]).getHours();vals[h].push(r[2]);hrTot[h]+=r[2];if(r[2]>gmax)gmax=r[2];const mm=mH[h];mm[r[1]]=(mm[r[1]]||0)+r[2];const cc=mC[h];cc[r[1]]=(cc[r[1]]||0)+1;}
  for(let h=0;h<24;h++){let bm=-1,bv=-1;for(const k in mH[h])if(mH[h][k]>bv){bv=mH[h][k];bm=+k;}dom[h]=bm;}
  return {vals,hrTot,dom,gmax,mH,mC};
}
function renderRidge(A){
  const svg=$('#rdSvg');
  const {mH}=hourData();
  const W=svg.clientWidth||800,H=112,pL=44,pR=8,pT=8,pB=18,plotH=H-pT-pB,base=pT+plotH;
  svg.setAttribute('viewBox','0 0 '+W+' '+H);svg.setAttribute('height',H);
  const pitch=(W-pL-pR)/24;
  const mh={};
  for(let h=0;h<24;h++)for(const k in mH[h]){const mi=+k;(mh[mi]=mh[mi]||new Array(24).fill(0))[h]=mH[h][k];}
  let vmax=0,vmin=Infinity;
  for(const mi in mh)for(const v of mh[mi])if(v>0){if(v>vmax)vmax=v;if(v<vmin)vmin=v;}
  if(!vmax){svg.innerHTML='<text x="'+pL+'" y="40" font-size="12" fill="'+CJ.muted+'">该时间范围内无数据</text>';$('#rdLegend').innerHTML='';return;}
  const lo=Math.log10(vmin),hi=Math.log10(vmax),span=Math.max(1e-9,hi-lo);
  const yv=v=>pT+plotH*(1-(Math.log10(v)-lo)/span);
  let yt='';
  for(let k=Math.ceil(lo);k<=Math.floor(hi);k++){const y=yv(Math.pow(10,k)).toFixed(1);yt+='<line x1="'+pL+'" y1="'+y+'" x2="'+(W-pR)+'" y2="'+y+'" stroke="'+CJ.grid+'" stroke-width="0.5"/><text x="'+(pL-5)+'" y="'+(+y+3).toFixed(1)+'" font-size="8.5" fill="'+CJ.axis2+'" text-anchor="end">'+fmt(Math.pow(10,k))+'</text>';}
  const mis=Object.keys(mh).map(Number).sort((a,b)=>{const ta=mh[a].reduce((x,y)=>x+y,0),tb=mh[b].reduce((x,y)=>x+y,0);return ta-tb;});
  const RC=rankColor(A);
  let defs='',curves='';
  for(const mi of mis){
    const a=mh[mi];
    const dim=hrSel!==null&&hrSel!==mi;
    const col=dim?CJ.dim:RC[mi];
    const runs=[];let cur=[];
    for(let h=0;h<24;h++){if(a[h])cur.push([pL+pitch*(h+0.5),yv(a[h])]);else{if(cur.length)runs.push(cur);cur=[];}}
    if(cur.length)runs.push(cur);
    const tt=a.reduce((x,y)=>x+y,0);
    let pk=0;for(let h=1;h<24;h++)if(a[h]>a[pk])pk=h;
    const tip=D.models[mi]+'\n窗口合计 '+fmt(tt)+' token\n时段峰值 '+pk+' 时 · '+fmt(a[pk]);
    defs+='<linearGradient id="rg'+mi+'" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="'+col+'" stop-opacity="0.30"/><stop offset="100%" stop-color="'+col+'" stop-opacity="0.02"/></linearGradient>';
    let body='',hitD='';
    for(const run of runs){
      // 两端补零轴点，使山脊平滑落回基线
      let pts;
      if(run.length===1){
        const x=run[0][0];
        pts=[[Math.max(pL,x-pitch*0.5),base],[x,run[0][1]],[Math.min(W-pR,x+pitch*0.5),base]];
      }else{
        pts=[[Math.max(pL,run[0][0]-pitch*0.6),base]].concat(run,[[Math.min(W-pR,run[run.length-1][0]+pitch*0.6),base]]);
      }
      const line=smoothLine(pts);
      hitD+=(hitD?' ':'')+line;
      const area=line+' L'+pts[pts.length-1][0].toFixed(1)+' '+base+' L'+pts[0][0].toFixed(1)+' '+base+' Z';
      body+='<path d="'+area+'" fill="url(#rg'+mi+')"/><path d="'+line+'" fill="none" stroke="'+col+'" stroke-width="0.8" stroke-opacity="0.95" pointer-events="none"/>';
    }
    const hit=hitD?'<path d="'+hitD+'" fill="none" stroke="rgba(0,0,0,0)" stroke-width="8" pointer-events="stroke" data-tip="'+tip+'"/>':'';
    curves+='<g opacity="'+(dim?0.22:1)+'">'+body+hit+'</g>';
  }
  let xt='';
  for(let h=0;h<24;h+=2)xt+='<text x="'+(pL+pitch*(h+0.5))+'" y="'+(H-6)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="middle">'+h+'时</text>';
  const idx=D.models.map((m,i)=>i).filter(i=>A.mTot[i]>0).sort((a,b)=>A.mTot[b]-A.mTot[a]).slice(0,8);
  const RC2=rankColor(A);
  $('#rdLegend').innerHTML=idx.map(mi=>'<span data-hrm="'+mi+'" style="cursor:pointer;'+(hrSel!==null&&hrSel!==mi?'opacity:.4':'')+'"><i style="display:inline-block;width:8px;height:8px;border-radius:2px;background:'+RC2[mi]+';margin-right:4px"></i>'+D.models[mi]+'</span>').join('');
  svg.innerHTML='<defs>'+defs+'</defs>'+yt+curves+xt+'<line x1="'+pL+'" y1="'+base+'" x2="'+(W-pR)+'" y2="'+base+'" stroke="'+CJ.base+'"/>';
}
document.addEventListener('click',e=>{
  const hg=e.target.closest('[data-hrm]');
  if(hg){const v=+hg.dataset.hrm;hrSel=(hrSel===v?null:v);if(LASTA)renderRidge(LASTA);}
});
function renderTop5(A){
  const order=D.models.map((m,i)=>i).filter(i=>A.mTot[i]>0).sort((a,b)=>A.mTot[b]-A.mTot[a]).slice(0,10);
  const RC=rankColor(A);
  const card=mi=>'<div class="t5'+(selModel===mi?' on':'')+'" data-dm="'+mi+'"><div class="k"><i style="display:inline-block;border-radius:2px;background:'+RC[mi]+'"></i>'+esc(D.models[mi])+'</div><div class="v">'+fmt(A.mTot[mi])+'</div></div>';
  $('#top5').innerHTML=order.map(card).join('');
}
function renderDailyModel(A){
  LASTA=A;
  const svg=$('#dmSvg');const {top}=topModels(A);
  const dates=Object.keys(A.dm).sort();
  const mx=Math.max(...dates.map(d=>Object.values(A.dm[d]).reduce((a,b)=>a+b,0)),1);
  const W=svg.clientWidth||800,H=102,pL=44,pR=8,pT=8,pB=18;
  const n=dates.length||1;const pitch=(W-pL-pR)/n;const bw=Math.max(2,pitch*0.62);
  const segs=top.concat(selModel!==null&&selModel!==-1&&!top.includes(selModel)?[selModel]:[]);
  const RC=rankColor(A);
  const colorOf=mi=>mi===-1?CJ.other:RC[mi];
  let g='',bars='',xlabels='';
  for(let i=1;i<8;i++){const y=pT+(H-pT-pB)*i/8;g+='<line x1="'+pL+'" y1="'+y+'" x2="'+(W-pR)+'" y2="'+y+'" stroke="'+CJ.grid+'" stroke-width="0.5"/>';}
  dates.forEach((d,i)=>{
    const dtt=Object.values(A.dm[d]).reduce((a,b)=>a+b,0);
    let cum=0;
    segs.forEach(mi=>{
      const v=(A.dm[d][mi]||0);if(!v)return;
      const bh=v/mx*(H-pT-pB);
      const x=pL+pitch*i+(pitch-bw)/2,y=H-pB-cum-bh;
      const dimmed=selModel!==null&&selModel!==mi;
      bars+='<rect x="'+x.toFixed(1)+'" y="'+y.toFixed(1)+'" width="'+bw.toFixed(1)+'" height="'+Math.max(0.5,bh).toFixed(1)+'" fill="'+(dimmed?CJ.dimBar:colorOf(mi))+'" fill-opacity="'+(dimmed?'0.5':'0.85')+'"/>';
      cum+=bh;
    });
    const dayList=segs.map(mi=>({mi:mi,v:(A.dm[d][mi]||0)})).filter(o=>o.v>0).sort((a,b)=>b.v-a.v);
    const dayTip=d+' · 合计 '+nf(dtt)+'\n'+dayList.map(o=>'· '+(o.mi===-1?'其他':D.models[o.mi])+'  '+nf(o.v)+'（'+(dtt?(o.v/dtt*100).toFixed(1):0)+'%）').join('\n');
    bars+='<rect x="'+(pL+pitch*i).toFixed(1)+'" y="'+pT+'" width="'+pitch.toFixed(1)+'" height="'+(H-pT-pB)+'" fill="#fff" fill-opacity="0" pointer-events="all" data-tip="'+dayTip+'"/>';
    const step=Math.ceil(n/7);
    if(i%step===0||i===n-1)xlabels+='<text x="'+(pL+pitch*i+pitch/2)+'" y="'+(H-4)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="middle">'+d+'</text>';
  });
  svg.setAttribute('viewBox','0 0 '+W+' '+H);svg.setAttribute('height',H);
  svg.innerHTML=g+bars+xlabels+
    '<text x="'+(pL-5)+'" y="'+(pT+7)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="end">'+fmt(mx)+'</text>'+
    '<text x="'+(pL-5)+'" y="'+(H-pB)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="end">0</text>';
}
document.addEventListener('click',e=>{
  const lg=e.target.closest('[data-dm]');
  if(lg){const v=+lg.dataset.dm;selModel=(selModel===v?null:v);if(LASTA){renderDailyModel(LASTA);renderTop5(LASTA);}}
});

function renderModelDist(){
  const svg=$('#mdSvg');const W=svg.clientWidth||300;
  const c=cut(),hiC=cutHi();
  const bym={};
  for(const s of D.sess)for(const r of s.r){const ts=r[0]*60000;if(ts<c||ts>=hiC)continue;(bym[r[1]]=bym[r[1]]||[]).push(r[2]);}
  const rows=Object.keys(bym).map(Number).sort((a,b)=>bym[b].length-bym[a].length).slice(0,8);
  if(!rows.length){svg.setAttribute('viewBox','0 0 '+W+' 60');svg.setAttribute('height',60);svg.innerHTML='<text x="16" y="30" font-size="11" fill="'+CJ.muted+'">该时间范围内无数据</text>';$('#mdCap').textContent='';return;}
  const st=rows.map(m=>{const a=bym[m].slice().sort((x,y)=>x-y);const n=a.length;
    const p=q=>a[Math.min(n-1,Math.ceil(q*n)-1)];return{m,n,p50:p(.5),p90:p(.9)};});
  const all=[];rows.forEach(m=>all.push(...bym[m]));
  const sorted=all.slice().sort((a,b)=>a-b);
  const gpct=q=>sorted[Math.min(sorted.length-1,Math.ceil(q*sorted.length)-1)];
  const gp50=gpct(.5);
  const cap=Math.max(1e5,Math.ceil(gpct(.995)*1.06/5e4)*5e4);
  const nameW=Math.min(118,Math.max(...rows.map(m=>D.models[m].length))*6.1+6);
  const cntX=nameW+8;
  const x50=cntX+48,x90=x50+56,lw=x90+76;
  const barW=52;
  const X=v=>lw+(W-lw-6)*Math.min(1,v/cap);
  const pScale=Math.max(st.reduce((a,s)=>Math.max(a,s.p90),0),1);
  const rh=44,bh=24,pT=14;
  const axY=pT+st.length*rh+2;
  const K=36,cntM=st.map(()=>new Array(K).fill(0));
  for(let ri=0;ri<st.length;ri++)for(const v of bym[st[ri].m]){const i=Math.min(K-1,Math.floor(v/cap*K));cntM[ri][i]++;}
  const RC=LASTA?rankColor(LASTA):null;
  let out='<line x1="'+X(gp50).toFixed(1)+'" y1="'+(pT-6)+'" x2="'+X(gp50).toFixed(1)+'" y2="'+axY+'" stroke="'+CJ.dash+'" stroke-width="0.8" stroke-dasharray="3 2"/>'+
    '<text x="'+(X(gp50)+3).toFixed(1)+'" y="'+(pT-1)+'" font-size="9" fill="'+CJ.axis+'">全局 50% '+fmt(gp50)+'</text>'+
    '<text x="0" y="'+(pT-1)+'" font-size="13" fill="'+CJ.axis+'">模型</text>'+
    '<text x="'+cntX+'" y="'+(pT-1)+'" font-size="13" fill="'+CJ.axis+'">次数</text>'+
    '<text x="'+x50+'" y="'+(pT-1)+'" font-size="13" fill="'+CJ.axis+'">50%</text>'+
    '<text x="'+x90+'" y="'+(pT-1)+'" font-size="13" fill="'+CJ.axis+'">90%</text>';
  st.forEach((s,ri)=>{
    const col=RC?RC[s.m]:modelColor(s.m);
    const baseY=pT+ri*rh+24;
    const raw=cntM[ri],sm=raw.map((v,i)=>((raw[i-1]||0)+2*v+(raw[i+1]||0))/4);
    const mx=Math.max(...sm,1e-9);
    let d='M'+lw.toFixed(1)+' '+baseY;
    for(let i=0;i<K;i++){const x=lw+(W-lw-6)*(i+0.5)/K;d+=' L'+x.toFixed(1)+' '+(baseY-sm[i]/mx*bh).toFixed(1);}
    d+=' L'+(W-6).toFixed(1)+' '+baseY+' Z';
    const tip=esc(D.models[s.m])+'&#10;'+nf(s.n)+' 次 · 50% '+fmt(s.p50)+' · 90% '+fmt(s.p90);
    out+='<path d="'+d+'" fill="'+col+'" fill-opacity="0.3" stroke="'+col+'" stroke-width="0.8" data-tip="'+tip+'"/>'+
      '<line x1="'+X(s.p50).toFixed(1)+'" y1="'+(baseY-bh-1)+'" x2="'+X(s.p50).toFixed(1)+'" y2="'+baseY+'" stroke="'+col+'" stroke-width="1.6" data-tip="'+tip+'"/>'+
      '<circle cx="'+X(s.p90).toFixed(1)+'" cy="'+(baseY-4)+'" r="2" fill="'+CJ.axis+'" data-tip="'+tip+'"/>'+
      '<text x="0" y="'+(baseY-5.5)+'" font-size="11" fill="'+CJ.t1+'">'+esc(D.models[s.m])+'</text>'+
      '<text x="'+cntX+'" y="'+(baseY-5.5)+'" font-size="11" fill="'+CJ.t1+'">'+nf(s.n)+'</text>'+
      '<text x="'+x50+'" y="'+(baseY-9)+'" font-size="12" fill="'+CJ.t1+'">'+fmt(s.p50)+'</text>'+
      '<rect x="'+x50+'" y="'+(baseY-5)+'" width="'+barW+'" height="5" fill="'+CJ.dimBar+'" fill-opacity="0.7" data-tip="'+tip+'"/>'+
      '<rect x="'+x50+'" y="'+(baseY-5)+'" width="'+Math.max(1.2,s.p50/pScale*barW).toFixed(1)+'" height="5" fill="'+col+'" fill-opacity="0.9" data-tip="'+tip+'"/>'+
      '<text x="'+x90+'" y="'+(baseY-9)+'" font-size="12" fill="'+CJ.t1+'">'+fmt(s.p90)+'</text>'+
      '<rect x="'+x90+'" y="'+(baseY-5)+'" width="'+barW+'" height="5" fill="'+CJ.dimBar+'" fill-opacity="0.7" data-tip="'+tip+'"/>'+
      '<rect x="'+x90+'" y="'+(baseY-5)+'" width="'+Math.max(1.2,s.p90/pScale*barW).toFixed(1)+'" height="5" fill="'+col+'" fill-opacity="0.45" data-tip="'+tip+'"/>';
  });
  let tks='<line x1="'+lw+'" y1="'+axY+'" x2="'+(W-6)+'" y2="'+axY+'" stroke="'+CJ.base+'"/>';
  const stepT=1e5;
  for(let v=0;v<=cap;v+=stepT){const x=X(v);
    tks+='<line x1="'+x.toFixed(1)+'" y1="'+axY+'" x2="'+x.toFixed(1)+'" y2="'+(axY+2)+'" stroke="'+CJ.axis+'"/>'+
      '<text x="'+x.toFixed(1)+'" y="'+(axY+12)+'" font-size="8" fill="'+CJ.axis+'" text-anchor="middle">'+(v===0?'0':(v/1e4)+'万')+'</text>';}
  const H=axY+16;
  svg.setAttribute('viewBox','0 0 '+W+' '+H);svg.setAttribute('height',H);
  svg.innerHTML=out+tks;
  $('#mdCap').textContent='TOP '+st.length+' 模型 · 共 '+nf(all.length)+' 次';
}
function renderCrumb(){
  const c=$('#crumb');
  if(state.ws===null){c.style.display='none';return;}
  const wn=D.ws[state.ws];
  let path,back;
  if(state.ses!==null){back='<button class="chip on" data-a="up">← 返回</button>';path='<a data-a="root">'+esc(wn)+'</a><span class="sep">›</span><span class="cur">'+esc(state.ses.t)+'</span>';}
  else{back='<button class="chip on" data-a="root">← 返回列表</button>';path='<span class="cur">'+esc(wn)+'</span>';}
  c.innerHTML=back+'<span class="path">'+path+'</span>';
  c.style.display='flex';
}
document.addEventListener('click',e=>{
  const a=e.target.closest('[data-a]');
  if(!a)return;
  if(a.dataset.a==='root'){state={ws:null,ses:null};render();}
  else if(a.dataset.a==='up'&&state.ws!==null){state={ws:state.ws,ses:null};render();}
});

function renderPanel(A){
  const p=$('#panel');
  const tools=$('#wsTools');
  const c=cut(),hiC=cutHi();
  const WHD={},dset={};SHD=new Map();
  for(const s of D.sess){
    let sm=null;
    for(const r of s.r){const ts=r[0]*60000;if(ts<c||ts>=hiC)continue;const d=hm(r[0]);const dk=fdate(d);
      dset[dk]=1;
      const w=s.w;(WHD[w]=WHD[w]||{});(WHD[w][dk]=WHD[w][dk]||new Array(24).fill(0));WHD[w][dk][d.getHours()]+=r[2];
      if(!sm){sm={};SHD.set(s,sm);}
      (sm[dk]=sm[dk]||new Array(24).fill(0));sm[dk][d.getHours()]+=r[2];}
  }
  PCTX={whd:WHD,days:Object.keys(dset).sort(),avgD:A.tot/(Object.keys(A.daily).length||1)};
  if(state.ws===null){
    tools.style.display='flex';
    if(wsMode==='ws'){
      $('#wsSorts').style.display='flex';
      let arr=A.wsArr.filter(x=>!q||x.n.toLowerCase().includes(q));
      if(wsSort==='tt')arr.sort((a,b)=>b.tt-a.tt);else arr.sort((a,b)=>b.n-a.n);
      if(!arr.length){p.innerHTML='<div class="empty">无匹配的工作空间</div>';return;}
      p.innerHTML='<div class="subgrid">'+arr.map(x=>'<div class="wcard" data-ws="'+x.i+'"><div class="wc-nm" title="'+esc(x.n)+'">'+esc(x.n)+'</div><div class="wc-tt">'+fmt(x.tt)+'</div><div class="wc-meta">'+(x.tt/A.tot*100).toFixed(1)+'% · '+x.cnt+' 会话</div><div class="wviz">'+buildViz(x.i)+'</div></div>').join('')+'</div>';
      return;
    }
    $('#wsSorts').style.display='none';
    let arr=A.sessAgg.filter(x=>!q||(x.s.t||'').toLowerCase().includes(q)).sort((a,b)=>b.tt-a.tt);
    if(!arr.length){p.innerHTML='<div class="empty">无匹配的会话</div>';return;}
    p.innerHTML='<div class="subgrid">'+arr.map(x=>{const i=D.sess.indexOf(x.s);const t=esc(x.s.t||'');
      return '<div class="wcard" data-si="'+i+'"><div class="wc-nm" title="'+t+'">'+t+'</div><div class="wc-tt">'+fmt(x.tt)+'</div><div class="wc-meta">'+(x.tt/A.tot*100).toFixed(1)+'% · '+x.tn+' 轮</div><div class="wviz">'+vizCells(SHD.get(x.s))+'</div></div>';}).join('');
  }else if(state.ses===null){
    tools.style.display='none';
    const arr=A.sessAgg.filter(x=>x.s.w===state.ws).sort((a,b)=>b.tt-a.tt);
    const wsTot=arr.reduce((a,x)=>a+x.tt,0);
    if(!arr.length){p.innerHTML='<div class="empty">该窗口内此工作空间无数据</div>';return;}
    p.innerHTML=arr.map(x=>{const i=D.sess.indexOf(x.s);const t=esc(x.s.t||'');
      return '<div class="wcard" data-ses="'+i+'"><div class="wc-nm" title="'+t+'">'+t+'</div><div class="wc-tt">'+fmt(x.tt)+'</div><div class="wc-meta">'+(wsTot?(x.tt/wsTot*100).toFixed(1):0)+'% · '+x.tn+' 轮</div><div class="wviz">'+vizCells(SHD.get(x.s))+'</div></div>';}).join('');
  }else{
    tools.style.display='none';
    const s=state.ses;const c=cut(),hiC=cutHi();
    const reqs=s.r.filter(r=>{const ts=r[0]*60000;return ts>=c&&ts<hiC;});
    const tt=reqs.reduce((a,r)=>a+r[2],0);
    const t0=reqs[0][0],t1=reqs[reqs.length-1][0];
    p.innerHTML='<div class="view-h"><div><div class="view-t">'+esc(s.t)+'</div><div class="view-m">'+fdate(hm(t0))+' '+ftime(hm(t0))+' → '+fdate(hm(t1))+' '+ftime(hm(t1))+' · '+reqs.length+' 次请求 · 合计 '+fmt(tt)+'</div></div><div><button class="chip on" data-v="sc">散点</button> <button class="chip" data-v="tb">明细表</button></div></div><div id="reqView"></div>';
    drawScatter(reqs);
    p.querySelector('[data-v="sc"]').onclick=()=>{drawScatter(reqs);setV('sc');};
    p.querySelector('[data-v="tb"]').onclick=()=>{drawTable(reqs);setV('tb');};
    function setV(v){p.querySelectorAll('[data-v]').forEach(b=>b.classList.toggle('on',b.dataset.v===v));}
  }
}
function drawScatter(reqs){
  const el=$('#reqView');const RC=LASTA?rankColor(LASTA):null;const mcol=i=>RC?RC[i]:modelColor(i);
  const W=el.clientWidth||560,H=240,pL=42,pR=12,pT=12,pB=26;
  const t0=reqs[0][0],t1=reqs[reqs.length-1][0],span=Math.max(1,t1-t0);
  const mx=Math.max(...reqs.map(r=>r[2]),1);
  const x=v=>pL+(W-pL-pR)*(v-t0)/span;
  const y=v=>pT+(H-pT-pB)*(1-Math.log(v+1)/Math.log(mx+1));
  let g='';for(let i=0;i<4;i++){const yy=pT+(H-pT-pB)*i/3;g+='<line x1="'+pL+'" y1="'+yy+'" x2="'+(W-pR)+'" y2="'+yy+'" stroke="'+CJ.grid+'"/>';}
  const d0=hm(t0);
  let pts='';
  for(const r of reqs){
    const cx=x(r[0]),cy=y(r[2]);
    const d=hm(r[0]);
    const tip=fdate(d)+' '+ftime(d)+':'+String(d.getSeconds()).padStart(2,'0')+'\n'+D.models[r[1]]+'\n输入 '+nf(r[3])+' · 输出 '+nf(r[4])+'\n缓存 '+nf(r[5])+' · 合计 '+nf(r[2]);
    pts+='<circle cx="'+cx.toFixed(1)+'" cy="'+cy.toFixed(1)+'" r="3.2" fill="'+mcol(r[1])+'" fill-opacity=".82" data-tip="'+tip.replace(/\n/g,'&#10;')+'"/>';
  }
  const lbl='<text x="'+pL+'" y="'+(pT+8)+'" font-size="9" fill="'+CJ.axis+'">'+fmt(mx)+'</text><text x="'+pL+'" y="'+(H-pB)+'" font-size="9" fill="'+CJ.axis+'">1</text>';
  const xm='max';
  el.innerHTML='<svg viewBox="0 0 '+W+' '+H+'" width="100%"><g>'+g+pts+lbl+
    '<text x="'+pL+'" y="'+(H-6)+'" font-size="9" fill="'+CJ.axis+'">'+fdate(d0)+' '+ftime(d0)+'</text>'+
    '<text x="'+(W-pR)+'" y="'+(H-6)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="end">'+fdate(hm(t1))+' '+ftime(hm(t1))+'</text>'+
    '<text x="'+(pL-8)+'" y="'+(H/2)+'" font-size="9" fill="'+CJ.axis+'" text-anchor="end" transform="rotate(-90 '+(pL-8)+' '+(H/2)+')">token（对数轴）</text></g></svg>'+
    '<div style="margin-top:6px;font-size:10px;color:var(--t3)">x=时间 · y=单次请求 token（对数） · 颜色=模型：<span style="color:#9a978f">'+modelLegend(reqs)+'</span></div>';
}
function modelLegend(reqs){
  const set={};reqs.forEach(r=>set[r[1]]=1);
  const RC=LASTA?rankColor(LASTA):null;return Object.keys(set).map(i=>'<span style="color:'+(RC?RC[i]:modelColor(i))+'">●</span>'+D.models[i]).join('　');
}
function drawTable(reqs){
  const el=$('#reqView');
  let rows='';
  for(const r of reqs.slice().reverse()){
    const d=hm(r[0]);
    const tip='输入 '+nf(r[3])+' · 输出 '+nf(r[4])+' · 缓存 '+nf(r[5])+' · 合计 '+nf(r[2]);
    rows+='<tr data-tip="'+tip+'"><td>'+fdate(d)+' '+ftime(d)+'</td><td style="text-align:left">'+D.models[r[1]]+'</td><td>'+nf(r[3])+'</td><td>'+nf(r[4])+'</td><td>'+nf(r[5])+'</td><td>'+nf(r[2])+'</td></tr>';
  }
  el.innerHTML='<div class="rt-wrap"><table class="rt"><thead><tr><th>时间</th><th style="text-align:left">模型</th><th>输入</th><th>输出</th><th>缓存命中</th><th>合计</th></tr></thead><tbody>'+rows+'</tbody></table></div>';
}

document.addEventListener('click',e=>{
  const r=e.target.closest('[data-ws]');
  if(r){state={ws:+r.dataset.ws,ses:null};render();return;}
  const s0=e.target.closest('[data-si]');
  if(s0){const ss=D.sess[+s0.dataset.si];state={ws:ss.w,ses:ss};render();return;}
  const s2=e.target.closest('[data-ses]');
  if(s2){state.ses=D.sess[+s2.dataset.ses];render();return;}
  const p2=e.target.closest('.pill');
  if(p2){win=+p2.dataset.w;customStart=0;customEnd=0;$('#dStart').value='';$('#dEnd').value='';selModel=null;hrSel=null;document.querySelectorAll('.pill').forEach(b=>b.classList.toggle('on',b===p2));render();return;}
  const mch=e.target.closest('[data-m]');
  if(mch){wsMode=mch.dataset.m;document.querySelectorAll('#wsTools [data-m]').forEach(b=>b.classList.toggle('on',b===mch));renderPanel(aggregate());return;}
  const ch2=e.target.closest('#wsSorts .chip');
  if(ch2){wsSort=ch2.dataset.s;document.querySelectorAll('#wsSorts .chip').forEach(b=>b.classList.toggle('on',b===ch2));renderPanel(aggregate());}
});
$('#wsSearch').addEventListener('input',e=>{q=e.target.value.trim().toLowerCase();renderPanel(aggregate());});
$('#wsMult').addEventListener('change',e=>{wsMult=+e.target.value;renderPanel(aggregate());});
function applyCustom(){
  const sv=$('#dStart').value,ev=$('#dEnd').value;
  if(!sv&&!ev)return;
  customStart=sv?new Date(sv+'T00:00:00').getTime()/60000:0;
  customEnd=ev?(new Date(ev+'T00:00:00').getTime()+86400e3)/60000:0;
  win=0;selModel=null;
  document.querySelectorAll('.pill').forEach(b=>b.classList.remove('on'));
  render();
}
$('#dStart').addEventListener('change',applyCustom);
$('#dEnd').addEventListener('change',applyCustom);

function render(){
  syncChrome();
  // 空数据源（比如小 agent 还没跑过对话）→ 直接给一句话，别渲染一堆空图表
  const reqN=D.sess.reduce((a,x)=>a+x.r.length,0);
  if(!reqN){
    ['#kpis','#heatWrap','#band4','#hhead','#top5'].forEach(s=>{const e=$(s);if(e)e.innerHTML='';});
    ['#rdSvg','#dmSvg','#mdSvg'].forEach(s=>{const e=$(s);if(e)e.innerHTML='';});
    $('#panel').innerHTML='<div class="empty">「'+esc(D.name)+'」暂无用量记录<br><span style="font-size:11px;color:var(--t4)">该数据源还没产生带 usage 的请求</span></div>';
    const lg=$('#heatLegend');if(lg)lg.innerHTML='';
    const rl=$('#rdLegend');if(rl)rl.innerHTML='';
    return;
  }
  const A=aggregate();LASTA=A;
  renderKpis(A);renderHeat(A);renderBand4(A);renderCrumb();renderPanel(A);
  renderRidge(A);renderTop5(A);renderDailyModel(A);
  renderModelDist();
  const ps=document.querySelector('.panel-scroll'),hc=document.querySelector('#heatCard');
  if(ps&&hc){const card=ps.closest('.card');
    const delta=card.getBoundingClientRect().height-ps.getBoundingClientRect().height;
    ps.style.maxHeight=Math.max(180,hc.getBoundingClientRect().height-delta)+'px';}
}
render();
new ResizeObserver(()=>{clearTimeout(window._rzT);window._rzT=setTimeout(render,120)}).observe(document.querySelector('.wrap'));
const themeBtn=$('#themeBtn');themeBtn.textContent=DARK?'浅色':'深色';if(DARK)document.body.classList.add('dark');
themeBtn.onclick=()=>{DARK=!DARK;CJ=DARK?CJD:CJL;MC=DARK?MCD:MCL;RAMP=DARK?RMPD:RMPL;try{localStorage.setItem('td-dark',DARK?'1':'0');}catch(e){}document.body.classList.toggle('dark',DARK);themeBtn.textContent=DARK?'浅色':'深色';render();};
</script>
</body></html>'''

html = TEMPLATE.replace('__DATA__', js)
with open(OUT, 'w', encoding='utf-8') as _f:
    _f.write(html)

# ── AI 可读摘要 ──────────────────────────────────────────────────────────────
# 面板 HTML 是给人看的；这份 JSON 给 AI/脚本直接读，不必解析 HTML。
# 口径与看板一致：只有 total>0 的请求才计入；成本按公开牌价粗略估算。
_PRICE = {
    'deepseek-v4-flash': (1, 4, 0.2), 'deepseek-v4-pro': (2, 8, 0.5),
    'deepseek': (2, 8, 0.5), 'glm-5.3': (2, 8, 0.4), 'glm-5.2-x': (1.2, 6, 0.24),
    'glm-5.2': (0.8, 3.2, 0.16), 'glm': (1, 4, 0.2), 'kimi': (1.5, 6, 0.3),
    'claude-opus-5': (150, 750, 15), 'claude-opus-4-8': (110, 550, 11),
    'claude': (110, 550, 11), 'qwen': (2.4, 9.6, 0.48),
    'hy3': (4, 16, 0.8), 'hy': (4, 16, 0.8),
    'minimax': (1, 5, 0.2), 'm3': (1, 5, 0.2),
}


def _price_of(name):
    n = (name or '').lower()
    for k, v in _PRICE.items():
        if n.startswith(k):
            return v
    return (2.0, 8.0, 0.4)


def _cost_cny(model, rec):
    """rec = (min, mid, total, input, output, cache_hit, cache_write)"""
    pin, pout, pch = _price_of(model)
    _, _, _, it, ot, ch, cw = rec
    # 缓存命中按缓存价；缓存写入按 1.25×输入价；其余输入按输入价（元/百万 token）
    return (ch * pch + (it - ch) * pin + cw * pin * 1.25 + ot * pout) / 1e6


def _local_date(minute_ts):
    return datetime.datetime.fromtimestamp(minute_ts * 60).strftime('%Y-%m-%d')


def build_summary(data, agent_data, out_html, summary_path):
    sources = []
    for key in data['order']:
        s = data['sources'][key]
        models = s.get('models') or []
        sess = s.get('sess') or []
        total = prompt = completion = cache_hit = cache_write = n_req = 0
        model_stats = {}
        day_stats = {}
        last_min = 0
        for se in sess:
            for rec in se.get('r') or ():
                minute, mid, tt, it, ot, ch, cw = rec
                n_req += 1
                total += tt
                prompt += it
                completion += ot
                cache_hit += ch
                cache_write += cw
                last_min = max(last_min, minute)
                mname = models[mid] if 0 <= mid < len(models) else 'unknown'
                ms = model_stats.setdefault(mname, {'requests': 0, 'total': 0, 'prompt': 0, 'completion': 0, 'cost_cny': 0.0})
                ms['requests'] += 1
                ms['total'] += tt
                ms['prompt'] += it
                ms['completion'] += ot
                ms['cost_cny'] = round(ms['cost_cny'] + _cost_cny(mname, rec), 4)
                day = _local_date(minute)
                ds = day_stats.setdefault(day, {'date': day, 'requests': 0, 'total': 0, 'prompt': 0, 'completion': 0})
                ds['requests'] += 1
                ds['total'] += tt
                ds['prompt'] += it
                ds['completion'] += ot
        top_models = sorted(
            ({'model': m, **{k: (round(v, 4) if k == 'cost_cny' else v) for k, v in st.items()}}
             for m, st in model_stats.items()),
            key=lambda x: x['total'], reverse=True,
        )
        daily = sorted(day_stats.values(), key=lambda x: x['date'])
        cost = round(sum(m['cost_cny'] for m in top_models), 4)
        sources.append({
            'key': s.get('key'),
            'name': s.get('name'),
            'sessions': len(sess),
            'requests': n_req,
            'models': len(models),
            'groups': len(s.get('ws') or []),
            'total_tokens': total,
            'prompt_tokens': prompt,
            'completion_tokens': completion,
            'cache_hit_tokens': cache_hit,
            'cache_write_tokens': cache_write,
            'est_cost_cny': cost,
            'cost_note': '按公开牌价粗略估算，实际套餐价不同会有偏差',
            'last_request_local': (_local_date(last_min) + ' ' +
                                   datetime.datetime.fromtimestamp(last_min * 60).strftime('%H:%M')) if last_min else None,
            'top_models': top_models[:10],
            'daily': daily,
        })
    summary = {
        'schema': 'dual-token-dashboard/summary/v1',
        'generated_at': data.get('gen'),
        'generated_ms': data.get('genMs'),
        'html': os.path.abspath(out_html),
        'summary_json': os.path.abspath(summary_path),
        'agent_data': os.path.abspath(agent_data) if agent_data else None,
        'workbuddy_projects': os.path.abspath(PROJECTS) if PROJECTS else None,
        'note': 'AI/脚本请读本 JSON 回答用量问题；人看请打开 html。成本为牌价粗估。',
        'sources': sources,
        'totals': {
            'requests': sum(s['requests'] for s in sources),
            'total_tokens': sum(s['total_tokens'] for s in sources),
            'est_cost_cny': round(sum(s['est_cost_cny'] for s in sources), 4),
        },
    }
    with open(summary_path, 'w', encoding='utf-8') as f:
        json.dump(summary, f, ensure_ascii=False, indent=2)
    return summary


summary = build_summary(DATA, AGENT_DATA, OUT, SUMMARY_OUT)

print('written:', OUT, f'({os.path.getsize(OUT)/1e6:.2f} MB)')
print('summary:', SUMMARY_OUT)
print('agent-data:', AGENT_DATA or '(未找到，小 agent tab 将为空)')
for _s in summary['sources']:
    print(f"  [{_s['name']}] 会话 {_s['sessions']} | 请求 {_s['requests']} | 模型 {_s['models']} | "
          f"分组 {_s['groups']} | token {_s['total_tokens']} | 粗估 ¥{_s['est_cost_cny']}")
print(f"  [合计] 请求 {summary['totals']['requests']} | token {summary['totals']['total_tokens']} | "
      f"粗估 ¥{summary['totals']['est_cost_cny']}")
