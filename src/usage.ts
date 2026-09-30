/**
 * Token 用量的**纯聚合与判定**（2026-09-30 从 main.ts 拆出）。
 *
 * ## 为什么单独成模块
 *
 * 这里的函数是**纯计算**：扁平记录 → 按「本地日期 × 模型」聚合、命中率、
 * 今日合计。它们不碰 DOM、不碰 Tauri、不碰任何模块状态 —— 也就意味着
 * **可以脱离浏览器环境直接测**（一个 `UsageRecord[]` 进，一个结构出）。
 * 而它们承载的判定正是"成本是否失控"这件事，值得有独立测试。
 *
 * 留在 `main.ts` 的是 `renderUsageView` / `bindUsageView`：那两个依赖
 * 模型列表（`modelName()` 要查 `models` 这个模块状态）。
 */

// ---------------- 设置 › Token 用量 ----------------

import { localDateKey } from "./format";

/** 一次模型调用的 token 用量（后端 `llm::TokenUsage` 的归一化结果） */
export interface TokenUsage {
  prompt: number;
  completion: number;
  reasoning?: number;
  /** 缓存命中（读）token —— DeepSeek `prompt_cache_hit_tokens` / OpenAI `cached_tokens` */
  cacheHit?: number;
  /** 缓存写入 token —— Claude `cache_creation_input_tokens` 等 */
  cacheWrite?: number;
  total: number;
}

/** 一条用量记录（后端 `sessions::UsageRecord`） */
export interface UsageRecord {
  at: number;
  model: string;
  session: string;
  usage: TokenUsage;
}

export interface DayAgg {
  date: string;
  total: number;
  prompt: number;
  completion: number;
  /** 缓存命中（读）的 prompt token 小计 */
  cacheHit: number;
  /** 缓存写入 token 小计（Claude 系才有） */
  cacheWrite: number;
  /** 模型 id → 当天小计 */
  models: Map<string, { total: number; prompt: number; completion: number; cacheHit: number }>;
}

/**
 * 缓存命中率 = 命中 ÷ 输入。
 *
 * ## 为什么这个比值比"总 token"重要（2026-09-30 加）
 *
 * prompt token 的**大头是缓存命中价**（实测全量 2 亿里 94% 是命中），
 * 所以"上下文多大"本身不等于"花了多少钱"。真正按全价计费的是
 * `输入 − 命中` 那一截。命中率从 98% 掉到 90%，全价部分就翻 5 倍，
 * 而在此之前**界面完全看不见这个变化** —— 项目为"前缀稳定性"做的
 * 那些工作（稳定/动态分区、历史折叠下沉到落盘）也就无从验证。
 *
 * 返回 `null` 表示这条记录没进过输入（无意义，不显示）。
 */
export function cacheHitRate(prompt: number, hit: number | undefined): number | null {
  if (!prompt || prompt <= 0) return null;
  return Math.min(1, Math.max(0, (hit ?? 0) / prompt));
}

/** 命中率 → 一句话结论 + 是否该报警（低于 90% 视为异常） */
export function cacheVerdict(rate: number): { label: string; warn: boolean } {
  const pct = Math.round(rate * 1000) / 10;
  if (rate >= 0.95) return { label: `${pct}% 良好`, warn: false };
  if (rate >= 0.9) return { label: `${pct}% 一般`, warn: false };
  return { label: `${pct}% 偏低`, warn: true };
}

/** 扁平记录 → 「本地日期 → 模型」聚合；日期倒序（最近在最上面） */
export function aggregateUsage(records: UsageRecord[]): DayAgg[] {
  const byDay = new Map<string, DayAgg>();
  for (const r of records) {
    const date = localDateKey(r.at);
    let day = byDay.get(date);
    if (!day) {
      day = {
        date,
        total: 0,
        prompt: 0,
        completion: 0,
        cacheHit: 0,
        cacheWrite: 0,
        models: new Map(),
      };
      byDay.set(date, day);
    }
    day.total += r.usage.total;
    day.prompt += r.usage.prompt;
    day.completion += r.usage.completion;
    day.cacheHit += r.usage.cacheHit ?? 0;
    day.cacheWrite += r.usage.cacheWrite ?? 0;
    const mid = r.model || "（未知模型）";
    const m = day.models.get(mid) ?? { total: 0, prompt: 0, completion: 0, cacheHit: 0 };
    m.total += r.usage.total;
    m.prompt += r.usage.prompt;
    m.completion += r.usage.completion;
    m.cacheHit += r.usage.cacheHit ?? 0;
    day.models.set(mid, m);
  }
  return [...byDay.values()].sort((a, b) => (a.date < b.date ? 1 : -1));
}

/** 今日消耗合计（设置入口页摘要用） */
export function todayTokenTotal(records: UsageRecord[]): number {
  const key = localDateKey(Date.now());
  let n = 0;
  for (const r of records) if (localDateKey(r.at) === key) n += r.usage.total;
  return n;
}
