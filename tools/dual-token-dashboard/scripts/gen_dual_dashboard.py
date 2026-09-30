# -*- coding: utf-8 -*-
"""双数据源 Token 用量驾驶舱构建脚本（WorkBuddy + orbcat 小 agent）

渲染层为自研「驾驶舱」前端（panel/index.html + panel.css + panel.js），
本脚本只负责：扫日志 → 归一化 → 组装单文件 HTML + AI 可读 summary。

  · WorkBuddy  —— 扫 ~/.workbuddy/projects/**/*.jsonl 的 providerData.rawUsage/usage
  · 小 agent   —— 扫 orbcat/agent-data/sessions/*.json 的 messages[].usage

跨平台：Windows / macOS / Linux 通用，仅依赖 Python 标准库（3.8+）。

用法：
    python gen_dual_dashboard.py                       # 输出 ./dual-token-dashboard.html
    python gen_dual_dashboard.py --out 看板.html        # 自定义输出路径
    python gen_dual_dashboard.py --projects <目录>      # WorkBuddy 日志目录
    python gen_dual_dashboard.py --agent-data <目录>    # 小 agent 数据目录
"""
import argparse
import datetime
import glob
import json
import os
import re
import sys

# Windows 控制台默认 GBK，直接 print 中文会抛 UnicodeEncodeError
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")

DEFAULT_WORKBUDDY_PROJECTS = os.path.join(
    os.path.expanduser("~"), ".workbuddy", "projects")
DEFAULT_HTML_NAME = "dual-token-dashboard.html"
DEFAULT_SUMMARY_NAME = "dual-token-dashboard.summary.json"


def _parse_args():
    ap = argparse.ArgumentParser(
        description="双数据源 Token 用量驾驶舱生成器（WorkBuddy + 小 agent）")
    ap.add_argument("--projects", default=DEFAULT_WORKBUDDY_PROJECTS,
                    help="WorkBuddy 会话日志目录（默认 ~/.workbuddy/projects）")
    ap.add_argument("--agent-data", default=None,
                    help="orbcat 数据目录（默认自动探测 orbcat/agent-data）")
    ap.add_argument("--out", default=DEFAULT_HTML_NAME, help="输出 HTML 路径")
    ap.add_argument("--summary-out", default=None,
                    help="AI 可读摘要 JSON 路径（默认与 --out 同目录）")
    return ap.parse_args()


_args = _parse_args()

PROJECTS = _args.projects
OUT = _args.out
# 摘要默认与 HTML 同目录同名，方便成对搬运
SUMMARY_OUT = _args.summary_out or os.path.join(
    os.path.dirname(os.path.abspath(OUT)), DEFAULT_SUMMARY_NAME)


def _find_agent_data(explicit):
    """定位小 agent 的 agent-data 目录（内含 sessions/）。"""
    cands = []
    if explicit:
        cands.append(explicit)
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.abspath(os.path.join(here, "..", "..", ".."))
    cwd = os.getcwd()
    for base in (root, cwd, os.path.join(cwd, "..")):
        cands += [
            os.path.join(base, "orbcat", "agent-data"),
            os.path.join(base, "agent-data"),
        ]
    cands.append(os.path.join(os.path.expanduser("~"), "orbcat", "agent-data"))
    for c in cands:
        if c and os.path.isdir(os.path.join(c, "sessions")):
            return c
    for c in cands:
        if c and os.path.isdir(c):
            return c
    return None


AGENT_DATA = _find_agent_data(_args.agent_data)


# ===========================================================================
# 字段归一化层
#   各家日志的字段名/单位都不一样，差异全部收敛在这一层，
#   扫描层只面对内部口径。
# ===========================================================================

_UTC = datetime.timezone.utc
_EPOCH_MS = 1_000_000_000_000   # 13 位：毫秒时间戳的量级下限
_EPOCH_S = 1_000_000_000        # 10 位：秒时间戳的量级下限
_TS_FORMATS = (
    '%Y-%m-%dT%H:%M:%S.%fZ',
    '%Y-%m-%dT%H:%M:%SZ',
    '%Y-%m-%d %H:%M:%S',
    '%Y-%m-%dT%H:%M:%S.%f',
)


def _pick(*vals):
    """按 `or` 语义取第一个真值。

    为什么不写成 `next(v for v in vals if v is not None)`：这些日志里 0 与
    「字段缺失」在语义上等价（写了 0 通常就是没记），所以沿用 or 语义。
    """
    for v in vals:
        if v:
            return v
    return None


def _as_int(v):
    """转 int；转不动（None / 非数字字符串）当 0。"""
    try:
        return int(v)
    except (TypeError, ValueError):
        return 0


def to_epoch_seconds(v):
    """时间戳 → Unix 秒（float）。无法识别返回 None。

    · 数字：按量级判断毫秒还是秒（>1e12 毫秒；>1e9 秒；更小的量级不认，
      避免把 5 之类的脏值当成 1970 年）
    · 字符串：先按已知格式解析，**一律按 UTC 解读** —— 日志写入方就是这么
      约定的，按本地时区解读会让「跨零点」的日统计整体偏移一天。
      已知格式都不匹配时，再当作毫秒数字字符串试一次。
    """
    if isinstance(v, bool):          # bool 是 int 子类，先挡掉
        return None
    if isinstance(v, (int, float)):
        if v > _EPOCH_MS:
            return v / 1000.0
        if v > _EPOCH_S:
            return float(v)
        return None
    if not isinstance(v, str):
        return None
    s = v.strip()
    if not s:
        return None
    for fmt_ in _TS_FORMATS:
        try:
            dt = datetime.datetime.strptime(s, fmt_).replace(tzinfo=_UTC)
        except ValueError:
            continue
        return float(int(dt.timestamp()))   # 截到整秒，与上游日志精度一致
    try:
        return float(s) / 1000.0
    except ValueError:
        return None


def read_usage(o):
    """从 WorkBuddy 的一条日志对象里取用量；不是计费记录则返回 None。

    取值优先级：`rawUsage`（原始口径，带缓存明细）> `usage`（归一化口径）。
    拿不到 total 就认为这条与计费无关（工具调用、系统事件等）。
    """
    pd = o.get('providerData')
    if not isinstance(pd, dict):
        return None
    raw = pd.get('rawUsage') or {}
    norm = pd.get('usage') or {}
    total = _pick(raw.get('total_tokens'), norm.get('totalTokens'))
    if not total:
        return None

    details = raw.get('prompt_tokens_details') or {}
    # 缓存命中优先信 rawUsage 的顶层字段；它缺失（None）才回落到明细里。
    # 注意这里必须区分「None」和「0」：顶层显式写了 0 就是 0，不该被明细覆盖。
    hit = details.get('cached_tokens') or 0
    raw_hit = raw.get('prompt_cache_hit_tokens')
    if raw_hit is not None:
        hit = raw_hit

    return {
        'tt': int(total),
        'it': _as_int(_pick(raw.get('prompt_tokens'), norm.get('inputTokens'))),
        'ot': _as_int(_pick(raw.get('completion_tokens'), norm.get('outputTokens'))),
        'ch': _as_int(hit),
        'cw': _as_int(_pick(details.get('cached_creation_tokens'),
                            details.get('cache_write_tokens'))),
        'model': _pick(pd.get('model'), pd.get('requestModelId'),
                       pd.get('requestModelName'), 'unknown'),
    }


TITLE_MAX = 42            # 面板标题列的可用宽度
_INTERNAL_PREFIX = '<'    # system-reminder 之类内部消息的起始字符


def _first_text_segment(content):
    """从消息 content 里取第一段可用文本；str 直接用，list 找 text 段。"""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return None
    for seg in content:
        if not isinstance(seg, dict):
            continue
        if seg.get('type') not in ('input_text', 'text'):
            continue
        txt = (seg.get('text') or '').strip()
        if txt:
            return txt
    return None


def extract_title(o, fallback):
    """用首条「真人发的」用户消息当会话标题；内部标签消息跳过。

    `fallback` 仅为调用方签名稳定而保留，本函数取不到标题时返回 None，
    由上层决定兜底文案。
    """
    if o.get('role') != 'user':
        return None
    txt = _first_text_segment(o.get('content'))
    if not txt:
        return None
    flat = ' '.join(txt.split())          # 折叠换行/连续空白，标题要单行
    if not flat or flat.startswith(_INTERNAL_PREFIX):
        return None
    return flat[:TITLE_MAX]


MODEL_MERGE = {'glm-5.2-x': 'glm-5.2'}

_WS_MARKER = '-WorkBuddy-'


def workspace_short_name(name):
    """工作区目录名取 `-WorkBuddy-` 之后那段作短名；没有标记就原样返回。"""
    _head, sep, tail = name.rpartition(_WS_MARKER)
    if sep and tail:
        return tail
    return name


# ===========================================================================
# 数据源层：把日志读成统一内部记录
#   key / name / genMs / gen / ws / models / sess[{w,id,t,st,r}]
#   r 元组：(分钟戳, 模型下标, total, input, output, 缓存命中, 缓存写入)
# ===========================================================================

_SESSION_ID_SHOWN = 8          # 面板只显示会话 id 前 8 位
_NO_TITLE = '(无标题会话)'
_MS_PER_MIN = 60_000


class _Registry:
    """首次出现即登记，返回稳定下标。

    前端用下标引用模型/工作区（省带宽），所以登记顺序必须与扫描顺序一致。
    `render` 用于「登记键」与「显示名」不同的场合（工作区目录名 → 短名）。
    """

    def __init__(self, render=None):
        self._render = render or (lambda key: key)
        self._idx = {}
        self.names = []

    def index(self, key):
        if key not in self._idx:
            self._idx[key] = len(self.names)
            self.names.append(self._render(key))
        return self._idx[key]


def _build_sess_list(sessions):
    """内部会话字典 → 前端列表，按首个请求时间升序。

    丢掉没有任何记录的会话：面板上那会是一条没意义的空行。
    """
    rows = [
        {'w': rec['w'], 'id': sid[:_SESSION_ID_SHOWN],
         't': rec['t'] or _NO_TITLE, 'st': rec['r'][0][0], 'r': rec['r']}
        for sid, rec in sessions.items() if rec['r']
    ]
    rows.sort(key=lambda row: row['st'])
    return rows


def _iter_jsonl(path):
    """逐行读 JSONL，坏行跳过（日志被中断时最后一行常是残的）。"""
    with open(path, encoding='utf-8', errors='ignore') as fh:
        for line in fh:
            try:
                obj = json.loads(line)
            except Exception:
                continue
            if isinstance(obj, dict):
                yield obj


def _split_rel(rel):
    """相对路径 → (工作区分组名, 会话 id)。

    实测存在两种形态：
      `<工作区>/<会话id>.jsonl`                        主会话
      `<工作区>/<会话id>/subagents/agent-<x>.jsonl`    子 agent 日志

    两种都要归到同一个会话 id 上 —— 子 agent 的消耗属于它所属的那次会话，
    单独拆成会话会把一个任务的花费摊成好几条。
    所以会话 id 取**第 2 段**，不是最后一段。
    """
    parts = rel.split(os.sep)
    if len(parts) < 2 or not parts[0]:
        return None, None
    folder = parts[0]
    return folder, os.path.splitext(parts[1])[0]


def scan_workbuddy(projects=PROJECTS):
    """扫 WorkBuddy 的 `~/.workbuddy/projects/**/*.jsonl`。"""
    ws_reg = _Registry(workspace_short_name)
    model_reg = _Registry()
    sessions = {}

    for fp in glob.glob(os.path.join(projects, '**', '*.jsonl'), recursive=True):
        rel = os.path.relpath(fp, projects)
        folder, sid = _split_rel(rel)
        if folder is None:
            continue
        ws_slot = ws_reg.index(folder)      # 先登记工作区，空文件也占位
        # 只有主会话日志（层级 2）才用文件里的 sessionId 覆盖文件名推导的 id；
        # 子 agent 日志的 sessionId 是它自己的，用它会把子 agent 拆成独立会话。
        is_main_session = rel.count(os.sep) == 1
        recs, title = [], None

        for obj in _iter_jsonl(fp):
            inner_id = obj.get('sessionId')
            if inner_id and is_main_session:
                sid = inner_id
            if title is None:
                title = extract_title(obj, sid) or None
            usage = read_usage(obj)
            if not usage:
                continue
            ts = to_epoch_seconds(obj.get('timestamp'))
            if not ts:
                continue
            model = MODEL_MERGE.get(usage['model'], usage['model'])
            recs.append((
                int(ts // 60), model_reg.index(model),
                usage['tt'], usage['it'], usage['ot'], usage['ch'], usage['cw'],
            ))

        if not recs:
            continue
        recs.sort(key=lambda r: r[0])
        slot = sessions.setdefault(sid, {'w': ws_slot, 'id': sid, 't': title, 'r': []})
        slot['r'].extend(recs)

    return {'key': 'workbuddy', 'name': 'WorkBuddy',
            'ws': list(ws_reg.names), 'models': list(model_reg.names),
            'sess': _build_sess_list(sessions)}


def _agent_usage(msg):
    """orbcat 一条 assistant 消息 → 归一化用量字典；不可用返回 None。

    字段是 `TokenUsage` 的 camelCase 序列化：`cacheHit` / `cacheWrite`。
    更早的会话没有这两个字段，按 0 计。
    """
    if not isinstance(msg, dict) or msg.get('role') != 'assistant':
        return None
    usage = msg.get('usage')
    if not isinstance(usage, dict):
        return None
    total = _as_int(usage.get('total'))
    at = msg.get('at')
    if not total or not at:
        return None
    return {
        'at': int(at),
        'model': msg.get('model') or 'unknown',
        'total': total,
        'prompt': _as_int(usage.get('prompt')),
        'completion': _as_int(usage.get('completion')),
        'hit': _as_int(_pick(usage.get('cacheHit'), usage.get('cache_hit'))),
        'write': _as_int(_pick(usage.get('cacheWrite'), usage.get('cache_write'))),
    }


def scan_agent(agent_data=AGENT_DATA):
    """扫 orbcat 的 `agent-data/sessions/*.json`。"""
    if not agent_data:
        return {'key': 'agent', 'name': '小 agent', 'ws': [], 'models': [], 'sess': []}

    model_reg = _Registry()
    sessions = {}
    for fp in sorted(glob.glob(os.path.join(agent_data, 'sessions', '*.json'))):
        try:
            with open(fp, encoding='utf-8') as fh:
                doc = json.load(fh)
        except Exception:
            continue
        if not isinstance(doc, dict):
            continue
        sid = doc.get('id') or os.path.splitext(os.path.basename(fp))[0]
        recs = []
        for msg in doc.get('messages', []):
            u = _agent_usage(msg)
            if not u:
                continue
            recs.append((
                u['at'] // _MS_PER_MIN, model_reg.index(u['model']),
                u['total'], u['prompt'], u['completion'], u['hit'], u['write'],
            ))
        if not recs:
            continue
        recs.sort(key=lambda r: r[0])
        sessions[sid] = {'w': 0, 'id': sid, 't': doc.get('title'), 'r': recs}

    return {'key': 'agent', 'name': '小 agent',
            'ws': ['小 agent'] if sessions else [], 'models': list(model_reg.names),
            'sess': _build_sess_list(sessions)}


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
payload_json = json.dumps(DATA, ensure_ascii=False, separators=(',', ':')).replace('</', '<\\/')


# ── 前端资产组装（自研驾驶舱：panel/index.html + panel.css + panel.js）──

def _find_panel_dir():
    here = os.path.dirname(os.path.abspath(__file__))
    cands = [
        os.path.join(here, '..', 'panel'),
        os.path.join(here, 'panel'),
        os.path.join(os.getcwd(), 'panel'),
        os.path.join(os.getcwd(), 'orbcat', 'tools', 'dual-token-dashboard', 'panel'),
        os.path.join(here, '..', '..', '..', 'orbcat', 'tools', 'dual-token-dashboard', 'panel'),
    ]
    for c in cands:
        c = os.path.abspath(c)
        if os.path.isfile(os.path.join(c, 'index.html')):
            return c
    return None


def _read(path):
    with open(path, 'r', encoding='utf-8') as f:
        return f.read()


def assemble_html(payload_json):
    """把 panel 三件套收成单文件 HTML，并把数据写进 #payload。"""
    pdir = _find_panel_dir()
    if not pdir:
        raise SystemExit(
            '找不到 panel/ 前端目录（需含 index.html / panel.css / panel.js）。\n'
            '期望位置：脚本旁 panel/ 或工作目录 panel/'
        )
    html = _read(os.path.join(pdir, 'index.html'))
    css = _read(os.path.join(pdir, 'panel.css'))
    js = _read(os.path.join(pdir, 'panel.js'))

    # 内联 CSS / JS → 单文件离线可看
    html = re.sub(
        r'<link\s+rel="stylesheet"\s+href="panel\.css"\s*/?>',
        '<style>\n' + css + '\n</style>',
        html, count=1, flags=re.I,
    )
    html = re.sub(
        r'<script\s+src="panel\.js"></script>',
        '<script>\n' + js + '\n</script>',
        html, count=1, flags=re.I,
    )

    # 注入真实数据（整段替换 #payload 的 JSON）
    def _inject(m):
        return m.group(1) + payload_json + m.group(3)

    html2, n = re.subn(
        r'(<script id="payload" type="application/json">\s*)(.*?)(\s*</script>)',
        _inject, html, count=1, flags=re.S,
    )
    if n != 1:
        raise SystemExit('panel/index.html 里找不到 <script id="payload"> 注入点')
    return html2


html = assemble_html(payload_json)
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
