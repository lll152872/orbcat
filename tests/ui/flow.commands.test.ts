/**
 * 斜杠命令面板 —— 三路匹配 + 渲染 + 键盘导航。
 *
 * ## 为什么测匹配而不是测像素
 *
 * 匹配是**纯函数**（命令表 + 查询串 → 排序结果），测它不需要 DOM，
 * 也不需要真机。这正是把面板拆成独立模块换来的好处。
 *
 * ## 三路匹配是用户明确要求
 *
 * 用户原话：「/的命令中英双语,你可以拼音,英语,中文都没问题」
 * → 中文名 / 英文名 / 拼音首字母，**任一路命中都算命中**。
 */

import { describe, expect, it } from "vitest";
import {
  COMMANDS,
  currentCommand,
  filterCommands,
  MAX_VISIBLE,
  moveActive,
  newPanelState,
  openCommandPanel,
  renderCommandPanel,
  scoreCommand,
  type CommandDef,
} from "@/commands";

/** 造一条测试命令 */
function cmd(name: string, en: string, pinyin: string): CommandDef {
  return { name, en, pinyin, desc: `${name} 的说明`, icon: "", act: en };
}

describe("斜杠命令面板 · 三路匹配", () => {
  it("中文名能匹配", () => {
    const c = cmd("记忆", "memory", "jy");
    expect(scoreCommand(c, "记忆")).toBeGreaterThan(0);
    expect(scoreCommand(c, "记")).toBeGreaterThan(0);
  });

  it("英文名能匹配", () => {
    const c = cmd("记忆", "memory", "jy");
    expect(scoreCommand(c, "memory")).toBeGreaterThan(0);
    expect(scoreCommand(c, "mem")).toBeGreaterThan(0);
  });

  it("拼音首字母能匹配", () => {
    const c = cmd("记忆", "memory", "jy");
    expect(scoreCommand(c, "jy")).toBeGreaterThan(0);
    expect(scoreCommand(c, "j")).toBeGreaterThan(0);
  });

  it("大小写与空白不敏感", () => {
    const c = cmd("记忆", "memory", "jy");
    expect(scoreCommand(c, "  MEMORY ")).toBeGreaterThan(0);
    expect(scoreCommand(c, "JY")).toBeGreaterThan(0);
  });

  it("完全相等 > 前缀 > 包含", () => {
    const c = cmd("记忆", "memory", "jy");
    const exact = scoreCommand(c, "记忆");
    const prefix = scoreCommand(c, "记");
    const contains = scoreCommand(c, "忆");
    expect(exact).toBeGreaterThan(prefix);
    expect(prefix).toBeGreaterThan(contains);
  });

  it("不相关的查询不匹配", () => {
    const c = cmd("记忆", "memory", "jy");
    expect(scoreCommand(c, "xyz")).toBe(0);
    expect(scoreCommand(c, "zzz")).toBe(0);
  });

  it("空查询匹配全部（得分相同，靠原序）", () => {
    const list = [cmd("甲", "a", "j"), cmd("乙", "b", "y")];
    const r = filterCommands("", list);
    expect(r).toHaveLength(2);
    expect(r[0].cmd.name).toBe("甲");
  });
});

describe("斜杠命令面板 · 排序", () => {
  it("得分高的排前面", () => {
    const list = [cmd("设置", "settings", "sz"), cmd("设置页", "setup", "spy")];
    // 查 "设置"：第一条完全相等，第二条只是前缀
    const r = filterCommands("设置", list);
    expect(r[0].cmd.en).toBe("settings");
  });

  it("同分保持注册表原序（高频在前）", () => {
    const list = [cmd("甲", "alpha", "a"), cmd("乙", "beta", "b")];
    const r = filterCommands("x", list); // 都不匹配前缀，但都包含 x？改用空
    const r2 = filterCommands("", list);
    expect(r2.map((x) => x.cmd.name)).toEqual(["甲", "乙"]);
    expect(r).toHaveLength(0);
  });

  it("多路同时命中时只出现一次", () => {
    const list = [cmd("记忆", "memory", "jy")];
    const r = filterCommands("jy", list);
    expect(r).toHaveLength(1);
  });
});

describe("斜杠命令面板 · 注册表", () => {
  it("每条命令都有中英双语 + 拼音", () => {
    for (const c of COMMANDS) {
      expect(c.name, `${c.act} 缺中文名`).toBeTruthy();
      expect(c.en, `${c.act} 缺英文名`).toBeTruthy();
      expect(c.pinyin, `${c.act} 缺拼音`).toBeTruthy();
      expect(c.desc, `${c.act} 缺说明`).toBeTruthy();
      expect(c.act, `${c.name} 缺 act`).toBeTruthy();
    }
  });

  it("英文名不重复（重复会让用户不知道选哪个）", () => {
    const ens = COMMANDS.map((c) => c.en.toLowerCase());
    expect(new Set(ens).size).toBe(ens.length);
  });

  it("act 不重复", () => {
    const acts = COMMANDS.map((c) => c.act);
    expect(new Set(acts).size).toBe(acts.length);
  });

  it("英文名与拼音都是纯 ASCII（能被大小写/前缀匹配）", () => {
    for (const c of COMMANDS) {
      expect(c.en, `${c.act} 英文名含非 ASCII`).toMatch(/^[a-z0-9-]+$/i);
      expect(c.pinyin, `${c.act} 拼音含非 ASCII`).toMatch(/^[a-z]+$/i);
    }
  });
});

describe("斜杠命令面板 · 渲染", () => {
  it("中英双语都出现在条目里", () => {
    const list = [cmd("记忆", "memory", "jy")];
    const html = renderCommandPanel(filterCommands("记", list), 0, 1);
    expect(html).toContain("记忆");
    expect(html).toContain("memory");
  });

  it("最多渲染 MAX_VISIBLE 条（面板高度恒定）", () => {
    const many = Array.from({ length: 20 }, (_, i) =>
      cmd(`命令${i}`, `cmd${i}`, `c${i}`),
    );
    const matched = filterCommands("", many);
    const html = renderCommandPanel(matched, 0, 20);
    const count = (html.match(/class="cmd-item/g) ?? []).length;
    expect(count).toBeLessThanOrEqual(MAX_VISIBLE);
    expect(count).toBe(MAX_VISIBLE);
  });

  it("超出的部分给出提示而不是静默截断", () => {
    const many = Array.from({ length: 20 }, (_, i) =>
      cmd(`命令${i}`, `cmd${i}`, `c${i}`),
    );
    const html = renderCommandPanel(filterCommands("", many), 0, 20);
    expect(html).toContain("还有");
  });

  it("HTML 里的说明文字被转义（防注入）", () => {
    const evil: CommandDef = {
      name: "坏",
      en: "bad",
      pinyin: "b",
      desc: '<img src=x onerror="alert(1)">',
      icon: "",
      act: "bad",
    };
    const html = renderCommandPanel(filterCommands("坏", [evil]), 0, 1);
    expect(html).not.toContain("<img");
    expect(html).toContain("&lt;img");
  });
});

describe("斜杠命令面板 · 键盘导航", () => {
  it("↑↓ 循环移动高亮", () => {
    const st = newPanelState();
    // 手动塞两条匹配结果（不依赖 DOM）
    st.matched = filterCommands("", [cmd("甲", "a", "j"), cmd("乙", "b", "y")]);
    st.active = 0;
    // 没有 DOM 时 moveActive 也会更新 active（refreshActive 无 el 时直接返回）
    moveActive(st, 1);
    expect(st.active).toBe(1);
    moveActive(st, 1);
    expect(st.active).toBe(0); // 绕回
    moveActive(st, -1);
    expect(st.active).toBe(1); // 往回绕
  });

  it("没有面板时 currentCommand 返回 null", () => {
    const st = newPanelState();
    expect(currentCommand(st)).toBeNull();
  });
});
