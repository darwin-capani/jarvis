/**
 * The agent constellation — static roster mirroring the daemon's canonical
 * map (config/agents.toml, CONTRACT part A). Pure data + helpers: no DOM,
 * React, three.js, or Tauri imports, so the panel seeds the full 27-agent
 * team immediately (before any agent.active event arrives) and the reducer
 * can validate incoming hues against a known set.
 *
 * The DAEMON is the source of truth for the live hue carried on each
 * agent.active event; this table is the HUD-side mirror used for the idle
 * roster render and as a fallback when an event omits a hue. Keep name/role/
 * hue in lockstep with config/agents.toml — if the daemon roster changes,
 * this list must be updated to match (the agent.active hue always wins at
 * runtime, but a stale role label here would mislead the panel).
 */

/** One roster entry. `hue` is degrees 0..360 for the R3F core + panel glow. */
export interface AgentProfile {
  /** Lowercase canonical id (matches the daemon namespace `agent.<name>`). */
  name: string;
  /** One-line role descriptor shown in the constellation panel. */
  role: string;
  /** Kokoro voice id (informational on the HUD side; the daemon speaks). */
  voice: string;
  /** Core/panel hue in degrees, 0..360. */
  hue: number;
}

/**
 * The 27 agents, in roll-call order (darwin first — the Prime Orchestrator).
 * Hues are the identity colors; note ultron uses deep-orange 15 rather than
 * 0/red, because RED is reserved exclusively for alerts on this HUD.
 */
export const ROSTER: readonly AgentProfile[] = [
  { name: "darwin", role: "Prime Orchestrator", voice: "bm_george", hue: 190 },
  { name: "friday", role: "Daily Intel", voice: "bf_emma", hue: 35 },
  { name: "veronica", role: "Content + Comms", voice: "af_bella", hue: 320 },
  { name: "vision", role: "Research + OSINT", voice: "bf_isabella", hue: 265 },
  { name: "ultron", role: "Security + Automation", voice: "am_onyx", hue: 15 },
  { name: "athena", role: "Greek-Life Strategy", voice: "af_nova", hue: 50 },
  { name: "stark", role: "Business Intel", voice: "am_adam", hue: 205 },
  { name: "steve", role: "CTO + Builds", voice: "am_michael", hue: 150 },
  { name: "oracle", role: "Workflows", voice: "bm_lewis", hue: 280 },
  { name: "gecko", role: "Markets + Capital", voice: "bm_daniel", hue: 120 },
  { name: "hercules", role: "Fitness + Nutrition", voice: "am_fenrir", hue: 90 },
  { name: "pepper", role: "Personal EA + Reflection", voice: "bf_alice", hue: 300 },
  { name: "hulk", role: "Offline Survival", voice: "am_echo", hue: 110 },
  { name: "herald", role: "Meetings", voice: "bm_fable", hue: 220 },
  { name: "jerome", role: "Leisure + DJ", voice: "af_river", hue: 340 },
  { name: "edith", role: "Proactive Sentinel", voice: "af_sky", hue: 170 },
  { name: "fury", role: "Mission Orchestrator", voice: "am_eric", hue: 235 },
  { name: "cassandra", role: "Forecast & Simulation", voice: "af_aoede", hue: 250 },
  { name: "mnemosyne", role: "Semantic Memory", voice: "af_kore", hue: 130 },
  { name: "sage", role: "Deep Research", voice: "am_puck", hue: 245 },
  { name: "vitalis", role: "Health & Biometrics", voice: "af_heart", hue: 160 },
  { name: "karen", role: "Comms Autopilot", voice: "af_sarah", hue: 200 },
  { name: "dume", role: "Home & Environment", voice: "am_liam", hue: 140 },
  { name: "midas", role: "Personal Treasury", voice: "am_santa", hue: 100 },
  { name: "voyager", role: "Travel & Logistics", voice: "bf_lily", hue: 180 },
  { name: "aegis", role: "Defense & Privacy", voice: "af_nicole", hue: 210 },
  { name: "babel", role: "Translation & Interpretation", voice: "af_jessica", hue: 155 },
] as const;

/** The Prime Orchestrator id — the default active agent and idle-core owner. */
export const PRIME_AGENT = "darwin";

/** Default idle core hue (cyan), matching visuals.ts HUE_CYAN and darwin. */
export const DEFAULT_AGENT_HUE = 190;

/** name -> profile lookup, built once. */
const BY_NAME: ReadonlyMap<string, AgentProfile> = new Map(
  ROSTER.map((a) => [a.name, a]),
);

/** Roster profile for a name, or null for an unknown agent. Lowercased so a
 *  daemon emitting "VISION" still resolves. */
export function agentProfile(name: string): AgentProfile | null {
  return BY_NAME.get(name.trim().toLowerCase()) ?? null;
}

/** Clamp/normalize a hue to an integer in [0, 360). Non-finite -> fallback. */
export function normalizeHue(hue: number, fallback = DEFAULT_AGENT_HUE): number {
  if (!Number.isFinite(hue)) return fallback;
  return ((Math.round(hue) % 360) + 360) % 360;
}
