import { useMemo, useState } from "react";
import type { SkillEntry, SkillsCatalog } from "../core/events";
import Frame from "./Frame";

/**
 * SKILLS // MARKETPLACE — the review/visibility surface for DARWIN's hand-written
 * in-tree skill library (daemon/src/skills/). It browses the catalog by CATEGORY,
 * lets the user search/filter by name, and shows each skill's one-line "when to
 * use" description, its category, and a CONSEQUENTIAL badge (it parks behind the
 * confirmation gate when invoked) or a SOURCE-GATED badge (it reports it needs a
 * data source until one is configured). The header states the REAL shipped count
 * and the live [skills] master-switch state.
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. There is NO button here that invokes a skill, edits one, or
 *     changes a setting. The panel only SHOWS the catalog so the user can see what
 *     the library offers and which entries are gated. A skill runs ONLY through the
 *     model's skill_invoke meta-tool, where a consequential one still parks.
 *   - SECRET-FREE. A pure in-tree skill carries nothing secret, and the snapshot is
 *     bounded to the discovery surface (name, category, description, the two
 *     markers). The defensive parser (parseSkillsCatalog) surfaces ONLY those
 *     fields, so the panel can never render an unexpected/secret value.
 *   - HONEST counts + OFF state. The header shows the genuine shipped total, never
 *     a marketing figure ("13,700 skills"); with [skills].enabled = false the panel
 *     shows an explicit "off" note (the catalog is still listed so the user can see
 *     what WOULD be offered).
 *
 * The reducer only ever sets `skills` from a defensively-parsed `skills.catalog`
 * event, so this component can trust the fields it is handed.
 */

/** Human label for a category slug; falls back to the slug itself for any future
 *  heading the daemon adds (defensive — never assumes the closed set). */
const CATEGORY_LABEL: Record<string, string> = {
  utilities: "Utilities",
  text: "Text",
  datetime: "Date/Time",
  units: "Units",
  mathx: "Math",
  knowledge: "Knowledge",
  finance: "Finance",
  fun: "Fun",
};

function categoryLabel(slug: string): string {
  return CATEGORY_LABEL[slug] ?? slug;
}

export default function SkillsPanel({ skills }: { skills: SkillsCatalog | null }) {
  // No snapshot yet (daemon has not emitted skills.catalog) — render nothing
  // rather than a placeholder, mirroring the other event-fed panels.
  if (skills === null) return null;
  return <SkillsPanelBody catalog={skills} />;
}

/** Inner body — split out so the hooks (search + category filter state) only run
 *  once we know we have a snapshot, keeping the null early-return above hook-free. */
function SkillsPanelBody({ catalog }: { catalog: SkillsCatalog }) {
  // Active category filter ("" = all) and the case-insensitive name/description
  // search. Both are local view state — they never mutate the catalog or the
  // daemon; this panel is review-only.
  const [activeCategory, setActiveCategory] = useState<string>("");
  const [query, setQuery] = useState<string>("");

  const needle = query.trim().toLowerCase();
  const filtered = useMemo<SkillEntry[]>(() => {
    return catalog.skills.filter((s) => {
      if (activeCategory !== "" && s.category !== activeCategory) return false;
      if (needle === "") return true;
      return (
        s.name.toLowerCase().includes(needle) ||
        s.description.toLowerCase().includes(needle)
      );
    });
  }, [catalog.skills, activeCategory, needle]);

  // The category chips: every heading the snapshot reports, with its count, even
  // when zero (the daemon emits all eight). Defensive — drive the chips off the
  // categories array rather than re-deriving from the skills list.
  const categories = catalog.categories;

  return (
    <div className="skills-panel">
      <Frame title="SKILLS // MARKETPLACE" tag="REVIEW ONLY">
        <div className="skills-body">
          <div className="skills-head">
            <span className="skills-count">
              {catalog.count} skill{catalog.count === 1 ? "" : "s"}
            </span>
            <span className={`skills-pill ${catalog.enabled ? "ok" : "off"}`}>
              {catalog.enabled ? "ENABLED" : "DISABLED"}
            </span>
          </div>

          {!catalog.enabled ? (
            <div className="skills-off dim-note">
              The skill library is OFF (<code>[skills].enabled = false</code>). The
              catalog below is shown for reference, but the <code>skill_list</code>{" "}
              / <code>skill_invoke</code> meta-tools are not offered to any agent
              until you enable it in darwin.toml.
            </div>
          ) : null}

          {/* Category chips — browse by heading. "All" plus every category with
              its count. A chip with zero skills is shown but disabled. */}
          <div className="skills-cats">
            <button
              type="button"
              className={`skills-cat ${activeCategory === "" ? "on" : ""}`}
              onClick={() => setActiveCategory("")}
            >
              All <i className="skills-cat-n">{catalog.count}</i>
            </button>
            {categories.map((c) => (
              <button
                key={c.slug}
                type="button"
                className={`skills-cat ${activeCategory === c.slug ? "on" : ""}`}
                disabled={c.count === 0}
                onClick={() => setActiveCategory(c.slug)}
              >
                {categoryLabel(c.slug)} <i className="skills-cat-n">{c.count}</i>
              </button>
            ))}
          </div>

          {/* Search/filter by name or description. Local view state only. */}
          <div className="skills-search">
            <input
              type="text"
              className="skills-search-in"
              placeholder="filter skills…"
              value={query}
              aria-label="filter skills by name or description"
              onChange={(e) => setQuery(e.target.value)}
            />
            {needle !== "" || activeCategory !== "" ? (
              <span className="skills-search-n dim-note">
                {filtered.length} match{filtered.length === 1 ? "" : "es"}
              </span>
            ) : null}
          </div>

          {/* The catalog list — filtered + searched. */}
          <div className="skills-list">
            {filtered.length === 0 ? (
              <span className="skills-empty dim-note">
                {catalog.skills.length === 0
                  ? "no skills in the library yet"
                  : "no skill matches this filter"}
              </span>
            ) : (
              filtered.map((s) => <SkillRow key={s.name} skill={s} />)
            )}
          </div>

          <div className="skills-foot dim-note">
            DARWIN's skill library is a hand-written, in-tree set of small, pure,
            deterministic capabilities — not a populated community marketplace. The
            count above is the genuine shipped total. It is infinitely extensible
            (add a skill in-tree, via an external manifest, or through Self-Forge).
            This panel is review-only: a skill runs only when the model invokes it,
            and a CONSEQUENTIAL one parks for a spoken confirmation first.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One skill row: name + category + the safety marker (if any) + description.
 *  Renders ONLY the secret-free discovery fields. */
function SkillRow({ skill }: { skill: SkillEntry }) {
  return (
    <div className="skills-skill">
      <div className="skills-skill-head">
        <span className="skills-skill-name">{skill.name}</span>
        <span className="skills-skill-cat">{categoryLabel(skill.category)}</span>
        {skill.consequential ? (
          <span
            className="skills-pill conseq"
            title="consequential — parks for a spoken confirmation before it acts"
          >
            GATED
          </span>
        ) : null}
        {skill.sourceGated ? (
          <span
            className="skills-pill gated"
            title="source-gated — reports it needs a data source until one is configured"
          >
            NEEDS SOURCE
          </span>
        ) : null}
      </div>
      {skill.description !== "" ? (
        <div className="skills-skill-desc">{skill.description}</div>
      ) : (
        <div className="skills-skill-desc dim-note">(no description)</div>
      )}
    </div>
  );
}
