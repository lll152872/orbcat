import { describe, expect, it } from "vitest";
import { MAX_BUBBLES, splitBubbles } from "@/bubble";

/**
 * 闲聊模式的「一条回复 → 多个气泡」切分规则。
 *
 * 这批用例锁的是**行为契约**：切点在哪、什么不许拆、上限与保底怎么起作用。
 * 措辞可以调，但这些不变量不能破 —— 破了用户的聊天体验就变回"文档"了。
 */
describe("splitBubbles", () => {
  it("空输入 → 没有气泡", () => {
    expect(splitBubbles("")).toEqual([]);
    expect(splitBubbles("   \n  ")).toEqual([]);
  });

  it("短消息不切 —— 一句话还要拆两条反而别扭", () => {
    expect(splitBubbles("在的")).toEqual(["在的"]);
  });

  it("段落边界是首选切点（模型的分段 = 它自己决定发几条）", () => {
    const out = splitBubbles(
      "刚看完你说的那个方案，整体思路我觉得没什么问题，可以往下走。\n\n" +
        "不过有个细节要再确认一下，不然做到一半容易卡住。\n\n" +
        "就是数据同步那块，两边同时改同一条记录的时候，你打算怎么处理冲突？",
    );
    expect(out.length).toBe(3);
    expect(out[0]).toContain("思路我觉得没什么问题");
    expect(out[2]).toContain("怎么处理冲突");
  });

  it("段落太长 → 按句子再切", () => {
    const long =
      "第一句话得写得足够长才行，不然它会被合并到相邻的气泡里去。" +
      "第二句话同样不能太短，否则也会被并进前面那一条。" +
      "第三句话也得写够长度，否则同样逃不掉被合并的命运。";
    const out = splitBubbles(long);
    expect(out.length).toBeGreaterThan(1);
  });

  it("代码块整块保留，不许拆开", () => {
    const src =
      "这段代码你看下，我看不出问题在哪：\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\n有问题告诉我，我改。";
    const out = splitBubbles(src);
    const code = out.find((p) => p.includes("```"));
    expect(code).toBeTruthy();
    // 围栏内部有空行，但必须整块在同一个气泡里
    expect(code!.includes("const a = 1")).toBe(true);
    expect(code!.includes("const b = 2")).toBe(true);
  });

  it("列表整块保留（拆开就没法读了）", () => {
    const src =
      "我看下来有三件事要办：\n\n- 先把接口对一遍\n- 再看下缓存那块\n- 最后跑一轮压测\n\n顺序别搞反，后面依赖前面。";
    const out = splitBubbles(src);
    const list = out.find((p) => p.includes("- 先把接口对一遍"));
    expect(list).toBeTruthy();
    expect(list!.includes("- 最后跑一轮压测")).toBe(true);
  });

  it("硬上限：再长也不超过 5 条，且**一个字都不丢**", () => {
    const src = Array.from(
      { length: 12 },
      (_, i) => `这是第${i + 1}段内容，长度足够让它自己独立成为一个气泡。`,
    ).join("\n\n");
    const out = splitBubbles(src);
    expect(out.length).toBeLessThanOrEqual(MAX_BUBBLES);

    const joined = out.join("\n");
    for (let i = 1; i <= 12; i++) {
      expect(joined).toContain(`这是第${i}段内容`);
    }
  });

  it("太短的并进相邻气泡，不刷屏", () => {
    const src = "嗯嗯。\n\n好。\n\n这个我想想再回你，手头还有点事要处理完。";
    const out = splitBubbles(src);
    expect(out.length).toBe(1);
  });

  it("首条太短要向后并 —— 只做「向前并」会让第一个气泡过短", () => {
    const src = "在的。\n\n刚看到你消息，我这边正好也在弄这个，等下细说。";
    const out = splitBubbles(src);
    expect(out.length).toBe(1);
    expect(out[0]).toContain("刚看到你消息");
  });

  it("返回的每一条都是可直接渲染的完整文本（不含分隔标记）", () => {
    const out = splitBubbles("第一句要写够长度才行，不然会被并掉。\n\n第二句也要够长度，同样不能被并掉。");
    for (const b of out) {
      expect(b.includes("%%%")).toBe(false);
      expect(b.trim()).toBe(b);
    }
  });
});
