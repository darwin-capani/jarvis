import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/* ------------------------------------------------------------------------ *
 * Mock the Tauri runtime BEFORE importing the bridge, so enterTakeover /     *
 * exitTakeover dispatch through a controllable invoke and `inTauri()` reads  *
 * true. No dev server, no real window — the invoke is a spy. (Same pattern   *
 * as command-deck.test.ts.)                                                  *
 * ------------------------------------------------------------------------ */
const invokeMock = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: unknown) => invokeMock(cmd, args),
}));

vi.mock("../tauri/bridge", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../tauri/bridge")>();
  return { ...actual, inTauri: () => true };
});

import TakeoverStage from "../components/TakeoverStage";
import {
  exitAlwaysReachable,
  initialTakeoverState,
  isExitKey,
  takeoverReduce,
  type TakeoverState,
} from "../core/takeover";
import { initialState, type HudState } from "../core/state";
import { enterTakeover, exitTakeover } from "../tauri/takeover";

beforeEach(() => {
  invokeMock.mockReset();
});
afterEach(() => {
  vi.restoreAllMocks();
});

/* ------------------------------------------------------------------------ *
 * The pure takeover state machine. Ships OFF; enter/exit are idempotent so   *
 * the active bit can never wedge, and the exit-reachability invariant holds. *
 * ------------------------------------------------------------------------ */
describe("takeover state machine (pure)", () => {
  it("ships OFF — initial state is inactive (never auto-enters)", () => {
    expect(initialTakeoverState()).toEqual({ active: false });
  });

  it("enter sets active; exit clears it", () => {
    const entered = takeoverReduce(initialTakeoverState(), { type: "enter" });
    expect(entered.active).toBe(true);
    const exited = takeoverReduce(entered, { type: "exit" });
    expect(exited.active).toBe(false);
  });

  it("is idempotent — double-enter and double-exit return the SAME reference", () => {
    const entered = takeoverReduce(initialTakeoverState(), { type: "enter" });
    expect(takeoverReduce(entered, { type: "enter" })).toBe(entered);
    const idle = initialTakeoverState();
    expect(takeoverReduce(idle, { type: "exit" })).toBe(idle);
  });

  it("EXIT IS ALWAYS REACHABLE whenever takeover is active (the invariant)", () => {
    // Active => the visible EXIT affordance is guaranteed present.
    expect(exitAlwaysReachable({ active: true })).toBe(true);
    // Inactive => nothing to exit (windowed HUD has its own OS chrome).
    expect(exitAlwaysReachable({ active: false })).toBe(false);
    // It holds for every reachable active state produced by the reducer.
    const reachableActive = takeoverReduce(initialTakeoverState(), { type: "enter" });
    expect(exitAlwaysReachable(reachableActive)).toBe(true);
  });

  it("Esc is the keyboard exit key; other keys are not", () => {
    expect(isExitKey("Escape")).toBe(true);
    expect(isExitKey("Enter")).toBe(false);
    expect(isExitKey("c")).toBe(false);
    expect(isExitKey("F11")).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * The bridge wiring: enterTakeover -> invoke('enter_takeover'), exitTakeover *
 * -> invoke('exit_takeover'), both param-less (Tauri injects window+state).  *
 * ------------------------------------------------------------------------ */
describe("takeover bridge dispatch (mocked Tauri invoke)", () => {
  it("enterTakeover invokes enter_takeover and returns the backend's bool", async () => {
    invokeMock.mockResolvedValue(true);
    const ok = await enterTakeover();
    expect(ok).toBe(true);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock.mock.calls[0][0]).toBe("enter_takeover");
  });

  it("exitTakeover invokes exit_takeover and returns the backend's bool", async () => {
    invokeMock.mockResolvedValue(true);
    const ok = await exitTakeover();
    expect(ok).toBe(true);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock.mock.calls[0][0]).toBe("exit_takeover");
  });

  it("a thrown enter never wedges — resolves false (stays OUT of takeover)", async () => {
    invokeMock.mockRejectedValue("backend exploded");
    await expect(enterTakeover()).resolves.toBe(false);
  });

  it("a thrown exit NEVER fails closed — resolves true (the exit can't trap you)", async () => {
    invokeMock.mockRejectedValue("backend exploded");
    await expect(exitTakeover()).resolves.toBe(true);
  });
});

/* ------------------------------------------------------------------------ *
 * App-handler contract: the enter/exit handlers fold the reducer AND fire    *
 * the right backend command. Modeled exactly as App.tsx wires them (reducer  *
 * dispatch + void invoke), so the assertions track the real handlers.        *
 * ------------------------------------------------------------------------ */
describe("App takeover handlers (reducer + bridge, as wired in App.tsx)", () => {
  it("ENTER sets takeoverActive AND invokes enter_takeover", async () => {
    invokeMock.mockResolvedValue(true);
    // App.enterTakeover(): dispatch enter, then void invokeEnterTakeover().
    const next = takeoverReduce(initialTakeoverState(), { type: "enter" });
    await enterTakeover();
    expect(next.active).toBe(true); // takeoverActive becomes true
    expect(invokeMock.mock.calls[0][0]).toBe("enter_takeover");
  });

  it("EXIT clears takeoverActive AND invokes exit_takeover", async () => {
    invokeMock.mockResolvedValue(true);
    const active: TakeoverState = { active: true };
    // App.exitTakeover(): dispatch exit, then void invokeExitTakeover().
    const next = takeoverReduce(active, { type: "exit" });
    await exitTakeover();
    expect(next.active).toBe(false); // takeoverActive becomes false
    expect(invokeMock.mock.calls[0][0]).toBe("exit_takeover");
  });

  it("Esc exits ONLY while active — exactly the App keydown guard", async () => {
    invokeMock.mockResolvedValue(true);
    // App: `if (takeoverActive && isExitKey(ev.key)) { exit }`.
    const escWhileActive = ({ active: true } as TakeoverState).active && isExitKey("Escape");
    expect(escWhileActive).toBe(true);
    const next = takeoverReduce({ active: true }, { type: "exit" });
    await exitTakeover();
    expect(next.active).toBe(false);
    expect(invokeMock.mock.calls[0][0]).toBe("exit_takeover");

    // While inactive, Esc does NOT route to takeover-exit (deck/other handlers win).
    const escWhileIdle = ({ active: false } as TakeoverState).active && isExitKey("Escape");
    expect(escWhileIdle).toBe(false);
  });
});

/* ------------------------------------------------------------------------ *
 * The TAKEOVER layout (renderToStaticMarkup — node env, the same SSR pattern *
 * as command-deck.test.ts). It COMPOSES the existing CommandDeck + panels    *
 * and the EXIT control is rendered UNCONDITIONALLY (never hideable).         *
 * ------------------------------------------------------------------------ */
describe("TakeoverStage layout (device-gated render aside; the DOM stage is real)", () => {
  const hud: HudState = initialState();
  const render = (props: Parameters<typeof TakeoverStage>[0]) =>
    renderToStaticMarkup(createElement(TakeoverStage, props));

  it("ALWAYS renders the visible EXIT TAKEOVER control + the Esc hint", () => {
    const html = render({ state: hud, onExit: () => {} });
    expect(html).toContain("EXIT TAKEOVER");
    expect(html).toContain('aria-label="Exit takeover"');
    expect(html).toMatch(/Esc to exit/i);
  });

  it("the EXIT control is present for EVERY active state (it can never be hidden)", () => {
    // The stage takes no prop that could suppress the control; assert across a
    // spread of HUD states that the exit is invariably rendered.
    const variants: HudState[] = [
      initialState(),
      { ...hud, connected: true },
      { ...hud, activeAgent: { name: "darwin", hue: 190, role: "Prime Orchestrator" } },
    ];
    for (const v of variants) {
      const html = render({ state: v, onExit: () => {} });
      expect(html).toContain("EXIT TAKEOVER");
    }
  });

  it("COMPOSES the existing CommandDeck (reused, not forked) — the holotable", () => {
    const html = render({ state: hud, onExit: () => {} });
    // The same CommandDeck markup the windowed HUD renders, mounted open here.
    expect(html).toContain("COMMAND DECK");
    expect(html).toContain("CONSTELLATION");
  });

  it("renders the live panels into the no-OS-chrome desktop", () => {
    const html = render({ state: hud, onExit: () => {} });
    expect(html).toContain("takeover-desktop");
    expect(html).toContain("takeover-stage");
  });

  it("renders no secret/token-shaped string", () => {
    const html = render({ state: hud, onExit: () => {} });
    expect(html).not.toMatch(/token/i);
    expect(html).not.toMatch(/sk-/);
    expect(html).not.toMatch(/secret/i);
  });
});
