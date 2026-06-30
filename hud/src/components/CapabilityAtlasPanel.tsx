import type { CapabilityAtlas, CapabilityEntry } from "../core/events";
import Frame from "./Frame";

/**
 * CAPABILITY // ATLAS — the unified ARMED/INERT capability surface (the
 * legibility layer of the self-extension engine; fed by capability.atlas from
 * daemon/src/atlas.rs). It unions skills + agents + micro-apps + integrations
 * and shows, for each, whether it is usable NOW (armed) or shipped-ON-but-INERT
 * with the one dependency it is missing.
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. No control here arms, connects, runs, or changes anything; it
 *     only SHOWS the capability surface so the user can see what is live and what
 *     is one key away.
 *   - SECRET-FREE. The wire snapshot carries only capability names + credential
 *     PRESENCE (never a value); this component renders exactly those fields.
 *
 * The reducer only ever sets `capabilityAtlas` from a defensively-parsed
 * capability.atlas event (parseCapabilityAtlas drops malformed entries and never
 * returns a secret), so the fields here can be trusted.
 */

// Display order + label per capability kind. Integrations lead because that is
// where the armed/inert story is richest. An unknown future kind falls into a
// trailing "OTHER" group so a new daemon kind is never silently dropped.
const KIND_ORDER: { key: string; label: string }[] = [
  { key: "integration", label: "INTEGRATIONS" },
  { key: "app", label: "MICRO-APPS" },
  { key: "agent", label: "AGENTS" },
  { key: "skill", label: "SKILLS" },
];

export default function CapabilityAtlasPanel({
  atlas,
}: {
  atlas: CapabilityAtlas | null;
}) {
  // No snapshot yet (daemon has not emitted capability.atlas) — render nothing,
  // mirroring the other event-fed review panels.
  if (atlas === null) return null;

  const known = new Set(KIND_ORDER.map((g) => g.key));
  const groups = KIND_ORDER.map((g) => ({
    ...g,
    items: atlas.capabilities.filter((c) => c.kind === g.key),
  }));
  const other = atlas.capabilities.filter((c) => !known.has(c.kind));
  if (other.length > 0) groups.push({ key: "other", label: "OTHER", items: other });

  const pct = atlas.total > 0 ? Math.round((atlas.armed / atlas.total) * 100) : 0;

  return (
    <div className="atlas-panel">
      <Frame title="CAPABILITY // ATLAS" tag="REVIEW ONLY">
        <div className="atlas-body">
          {!atlas.enabled || atlas.capabilities.length === 0 ? (
            <div className="atlas-off dim-note">
              No capability surface yet. The atlas populates once the daemon emits
              its startup snapshot.
            </div>
          ) : (
            <>
              <div className="atlas-summary">
                <span className="atlas-count">
                  <span className="atlas-armed-n">{atlas.armed}</span>
                  <span className="atlas-slash"> / </span>
                  <span className="atlas-total-n">{atlas.total}</span>
                </span>
                <span className="atlas-count-label">ARMED</span>
                <span className="atlas-bar" aria-hidden="true">
                  <span className="atlas-bar-fill" style={{ width: `${pct}%` }} />
                </span>
              </div>
              {groups
                .filter((g) => g.items.length > 0)
                .map((g) => (
                  <AtlasGroup key={g.key} label={g.label} items={g.items} />
                ))}
            </>
          )}
        </div>
      </Frame>
    </div>
  );
}

function AtlasGroup({ label, items }: { label: string; items: CapabilityEntry[] }) {
  const armed = items.filter((i) => i.armed).length;
  return (
    <div className="atlas-group">
      <div className="atlas-group-head">
        <span className="atlas-group-label">{label}</span>
        <span className="atlas-group-count">
          {armed}/{items.length}
        </span>
      </div>
      {items.map((c) => (
        <AtlasRow key={`${c.kind}:${c.name}`} cap={c} />
      ))}
    </div>
  );
}

function AtlasRow({ cap }: { cap: CapabilityEntry }) {
  return (
    <div className={`atlas-row ${cap.armed ? "armed" : "inert"}`}>
      <span className="atlas-dot" aria-hidden="true" />
      <span className="atlas-name">{cap.name}</span>
      <span className="atlas-detail" title={cap.detail}>
        {cap.detail}
      </span>
      <span className={`atlas-pill ${cap.armed ? "armed" : "inert"}`}>
        {cap.armed ? "ARMED" : "INERT"}
      </span>
    </div>
  );
}
