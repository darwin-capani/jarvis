/**
 * HUD <-> Tauri bridge for the SYSTEM SETTINGS surface — the typed wrappers
 * around the three backend commands in src-tauri/src/config_settings.rs:
 *
 *   - config_get()            -> the live values of every whitelisted setting,
 *                                read from config/darwin.toml at the resolved
 *                                DARWIN root.
 *   - config_set(changes)     -> a WHITELIST-validated, in-place, atomic batch
 *                                write of edited values (preserves comments +
 *                                structure; rejects anything off the whitelist).
 *   - daemon_restart()        -> launchctl kickstart of com.darwin.daemon — the
 *                                ONLY path that makes a config edit take effect,
 *                                because every daemon module caches its config in
 *                                a OnceLock at startup (no hot-reload exists).
 *
 * Like command.ts, this layer carries NO secret/token and degrades gracefully in
 * a plain browser (vite dev, no Tauri runtime): it returns an honest "desktop
 * app required" failure instead of throwing, so the panel shows a clean
 * unavailable state rather than blanking the HUD.
 */
import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./bridge";

/** A typed setting value crossing the IPC boundary — the untagged
 *  `SettingValue` the Rust side accepts (Bool | Int | Float | Str | StrList).
 *  The 3-way autonomy control is a string ("off" | "propose" | "auto"); a
 *  freeform model-id field is a string; a roots/warm-set field is a string[]. */
export type SettingValue = boolean | number | string | string[];

/** One setting's live value + descriptor, exactly as `config_get` returns it.
 *  `kind` drives which control the UI renders; `options`/`min`/`max` bound it.
 *  No re-derivation of the whitelist is needed on the JS side. */
export interface SettingState {
  /** "section.key" for a flat setting; a bare section name for a 3-way control. */
  id: string;
  section: string;
  /** "" for a 3-way autonomy control (it spans enabled+mode). */
  key: string;
  /** "bool" | "int" | "float" | "enum" | "autonomy" | "string" | "strlist"
   *  | "pathlist". "string" is a freeform model-id field; "pathlist" is an
   *  absolute-folder array (folder picker); "strlist" is a repo-id array
   *  (manual add only). */
  kind: string;
  value: SettingValue;
  /** Present for enum + autonomy kinds — the allowed string options. */
  options?: string[];
  /** Present for int/float kinds — the inclusive bounds. */
  min?: number;
  max?: number;
}

/** One requested edit handed to `config_set`. `id` is a flat setting's
 *  "section.key" or a 3-way autonomy section name; `value` is the new typed
 *  value. The backend validates EVERY change before any write (a single bad
 *  change aborts the whole batch — no partial write). */
export interface Change {
  id: string;
  value: SettingValue;
}

/** The honest outcome of a daemon restart attempt (mirrors the Rust
 *  `RestartResult`). `ok=false` with a clear detail when the agent is not loaded
 *  — it never pretends a restart happened. */
export interface RestartResult {
  ok: boolean;
  detail: string;
}

/** READ every whitelisted setting's live value from config/darwin.toml. In a
 *  plain browser this throws an honest, UI-catchable error rather than a silent
 *  empty list (the panel surfaces "open the desktop app to edit config"). */
export async function configGet(): Promise<SettingState[]> {
  if (!inTauri()) {
    throw new Error("system settings require the DARWIN desktop app");
  }
  return invoke<SettingState[]>("config_get");
}

/** WRITE a batch of whitelisted edits to config/darwin.toml in place (atomic;
 *  comments + structure preserved). Returns the count of changed lines. Throws
 *  the backend's validation message on any rejected change (the whole batch is
 *  aborted — nothing is written). */
export async function configSet(changes: Change[]): Promise<number> {
  if (!inTauri()) {
    throw new Error("system settings require the DARWIN desktop app");
  }
  return invoke<number>("config_set", { changes });
}

/** RESTART darwind so the just-written config takes effect (there is no
 *  hot-reload). Honest about the launchd state — returns `ok=false` with a clear
 *  detail when the agent isn't loaded, rather than pretending. */
export async function daemonRestart(): Promise<RestartResult> {
  if (!inTauri()) {
    return {
      ok: false,
      detail: "restart requires the DARWIN desktop app",
    };
  }
  try {
    return await invoke<RestartResult>("daemon_restart");
  } catch (e) {
    return {
      ok: false,
      detail: typeof e === "string" ? e : "restart failed",
    };
  }
}

/** Open the NATIVE macOS folder picker (the backend's `pick_folder` command,
 *  which shells the canonical `choose folder` chooser — no new dependency). It
 *  returns the chosen ABSOLUTE path, or `null` when the user cancels OR when no
 *  picker is available (no GUI session / plain browser). The returned path is
 *  RE-VALIDATED by `config_set` on write exactly like a manually typed path —
 *  the picker is a convenience, never a trust shortcut. Callers MUST keep the
 *  manual text-add input as the always-works baseline. Never throws: a picker
 *  failure resolves to null so the manual path stays usable. */
export async function pickFolder(): Promise<string | null> {
  if (!inTauri()) return null;
  try {
    const picked = await invoke<string | null>("pick_folder");
    return picked ?? null;
  } catch {
    return null;
  }
}
