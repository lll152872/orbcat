/**
 * 插话（steer）的**内核归属**与**带图透传** —— 回归。
 *
 * ## 要锁住两件事
 *
 * ① **带图透传**（2026-10-23 fix002 起后端 `chat_pi_steer` 也收 images）：
 *    插话带图时，图要跟着送出去，且**不再**弹「pi 不支持带图」的提示
 *    （那条提示是 fix002 之前的前端保守处理，后端已收参数 ⇒ 提示是过时的）。
 *
 * ② **内核归属**（这条更值钱，2026-10-23 查出来的真缺陷）：
 *    插话必须投到**正在跑的那一轮**所用的内核，而不是「pi 可不可用」。
 *    - 正常发一轮 `send()` 走的是**自研内核**（`chat`）⇒ 该会话**没有 pi 运行时**
 *      （Rust `PiPool::get` 明确**不新建**）⇒ 插话若投 `chat_pi_steer`，
 *      后端直接回「会话 X 没有正在运行的 pi 内核」⇒ **插话在 pi 可用时必然失败**。
 *    - 所以前端记下每一轮实际用的内核（`runKernel`），插话按它分流。
 *
 * ## 用例怎么造出「pi 可用」
 *
 * `setMock("pi_status", { available: true })` ⇒ 前端判定 pi 内核可用
 * （注意 `pi_status` **不能**放进 mock 的默认响应表 —— 那会让所有用例都切到 pi 模式）。
 */
import { describe, it, expect, beforeEach } from "vitest";
import { bootApp, openPanel, sendText, waitFor, callsOf, sleep, state } from "./helpers";
import { setMock } from "./mocks/tauri";

/** 一张 1x1 的透明 PNG（data URL 形态，和前端 `pendingImages` 里放的一样） */
const PNG_DATA_URL =
  "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/** 挂一张图（走截屏通道：capture_screen 返回 data URL，和真机同形态） */
async function attachImage(): Promise<void> {
  setMock("capture_screen", PNG_DATA_URL);
  document.getElementById("btn-grab")!.click();
  await waitFor(() => !!document.querySelector(".thumb"), 2000, "附件已挂上");
}

/** 起一轮慢的对话（让面板处于运行中 → 第二次发送自动走插话通道） */
async function startSlowTurn(): Promise<void> {
  state.chatDelayMs = 500;
  void sendText("第一条");
  await waitFor(() => callsOf("chat").length >= 1, 3000, "chat started");
  await waitFor(() => !!document.querySelector(".msg.running"), 2000, "running bubble");
}

describe("插话 · 内核归属 + 带图透传", () => {
  beforeEach(() => {
    // pi 内核「可用」—— 用来验证前端不会因此把插话投到没有运行时的 pi 上
    setMock("pi_status", { available: true, runtimeDir: "C:\\mock\\runtime\\pi" });
  });

  it("插话带图：图跟着送进插话命令（fix002 后端已收 images）", async () => {
    await bootApp();
    await openPanel();
    await waitFor(() => callsOf("pi_status").length >= 1, 3000, "pi_status 已查");
    await sleep(50);

    await startSlowTurn();
    // ⚠️ 图必须在**起轮之后**挂 —— 每一轮发出去都会清空附件区
    //    （`pendingImages = []`），先挂的话会被第一轮带走。
    await attachImage();

    await sendText("插话带图");
    await waitFor(() => callsOf("chat_steer").length >= 1, 3000, "chat_steer");

    const steer = callsOf("chat_steer")[0]!;
    expect(steer.args?.input, "插话文字透传").toBe("插话带图");
    const imgs = steer.args?.images as string[] | undefined;
    expect(Array.isArray(imgs) && imgs.length === 1, "图透传给插话命令").toBe(true);
    expect(String(imgs?.[0]).startsWith("data:image/png"), "图是 data URL 形态").toBe(true);
  });

  it("插话投给**正在跑的那一轮**的内核（pi 可用 ≠ 该投 pi）", async () => {
    await bootApp();
    await openPanel();
    await waitFor(() => callsOf("pi_status").length >= 1, 3000, "pi_status 已查");
    await sleep(50);

    await startSlowTurn();

    await sendText("插话");
    await waitFor(() => callsOf("chat_steer").length >= 1, 3000, "chat_steer");

    // 本轮跑在自研内核（`chat`）⇒ 该会话没有 pi 运行时 ⇒ 插话**不能**投 chat_pi_steer
    // （投了后端会回「会话 X 没有正在运行的 pi 内核」，插话直接落空）
    expect(callsOf("chat_pi_steer").length, "插话不该投给没有运行时的 pi").toBe(0);
    expect(callsOf("chat_steer").length, "插话投给正在跑的内核").toBe(1);
  });

  it("不再出现「pi 不支持带图」的提示（后端已收 images，别把提示加回来）", async () => {
    await bootApp();
    await openPanel();
    await waitFor(() => callsOf("pi_status").length >= 1, 3000, "pi_status 已查");
    await sleep(50);

    await startSlowTurn();
    await attachImage();

    await sendText("插话带图");
    await waitFor(() => callsOf("chat_steer").length >= 1, 3000, "chat_steer");

    expect((document.body.textContent ?? "").includes("暂不支持带图"), "过时提示不该再出现").toBe(
      false,
    );
  });
});
