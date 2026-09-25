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
import argparse, json, glob, os, sys, datetime, re

# Windows 控制台 GBK 环境下防止中文 print 报错
try:
    sys.stdout.reconfigure(encoding="utf-8")
except Exception:
    pass

_ap = argparse.ArgumentParser(description="双数据源 Token 用量驾驶舱生成器（WorkBuddy + 小 agent）")
_ap.add_argument("--projects", default=os.path.join(os.path.expanduser("~"), ".workbuddy", "projects"),
                 help="WorkBuddy 会话日志目录（默认 ~/.workbuddy/projects）")
_ap.add_argument("--agent-data", default=None,
                 help="orbcat 数据目录（默认自动探测 orbcat/agent-data）")
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
    if ch is None: ch = ptd.get('cached_tokens') or 0
    cw = ptd.get('cached_creation_tokens') or ptd.get('cache_write_tokens') or 0
    return dict(tt=int(tt), it=int(ru.get('prompt_tokens') or u.get('inputTokens') or 0),
                ot=int(ru.get('completion_tokens') or u.get('outputTokens') or 0),
                ch=int(ch or 0), cw=int(cw or 0), model=model)


def session_title(o, fallback):
    """取首条非 system-reminder 的用户消息前 42 字作为任务标题"""
    if o.get('role') != 'user': return None
    c = o.get('content')
    if isinstance(c, str):
        t = ' '.join(c.split())
        if t and not t.startswith('<'):
            return t[:42]
        return None
    if not isinstance(c, list): return None
    for seg in c:
        if isinstance(seg, dict) and seg.get('type') in ('input_text', 'text'):
            t = (seg.get('text') or '').strip()
            if not t: continue
            if t.startswith('<'): return None
            t = ' '.join(t.split())
            return t[:42]
    return None


MODEL_MERGE = {'glm-5.2-x': 'glm-5.2'}


def short_ws(name):
    i = name.rfind('-WorkBuddy-')
    if i >= 0 and name[i + 11:]:
        return name[i + 11:]
    return name


# ===========================================================================
# 数据源层：把日志读成统一内部记录
#   key / name / genMs / gen / ws / models / sess[{w,id,t,st,r}]
#   r 元组：(分钟戳, 模型下标, total, input, output, 缓存命中, 缓存写入)
# ===========================================================================

def _build_sess_list(sessions):
    out = []
    for sid, d in sessions.items():
        if not d['r']:
            continue
        out.append({'w': d['w'], 'id': sid[:8], 't': d['t'] or '(无标题会话)',
                    'st': d['r'][0][0], 'r': d['r']})
    out.sort(key=lambda s: s['st'])
    return out


def scan_workbuddy(projects=PROJECTS):
    ws_names, ws_idx = {}, {}
    models, model_idx = [], {}
    sessions = {}
    for fp in glob.glob(os.path.join(projects, '**', '*.jsonl'), recursive=True):
        rel = os.path.relpath(fp, projects)
        parts = rel.split(os.sep)
        if not parts:
            continue
        folder = parts[0]
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
                if sid_r and len(parts) == 2: sid = sid_r
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
            # TokenUsage 序列化是 camelCase：cacheHit / cacheWrite（新）；旧数据无此字段 → 0
            ch = int(u.get('cacheHit') or u.get('cache_hit') or 0)
            cw = int(u.get('cacheWrite') or u.get('cache_write') or 0)
            recs.append((int(int(ts) // 60000), model_idx[mid], tt,
                         int(u.get('prompt') or 0), int(u.get('completion') or 0), ch, cw))
        if not recs: continue
        recs.sort(key=lambda x: x[0])
        sessions[sid] = {'w': 0, 'id': sid, 't': o.get('title'), 'r': recs}
    return {'key': 'agent', 'name': '小 agent',
            'ws': ['小 agent'] if sessions else [], 'models': models,
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
