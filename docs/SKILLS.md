# DARWIN Skill Framework — the open standard

This is the authoritative spec for DARWIN's **skill library**: a hand-written,
in-tree set of small, pure capabilities, plus the open standard that lets the set
grow. Where other notes disagree with this document, this one wins.

## Honest scope (read this first)

The skill library is a **genuine, hand-written, in-tree library** of skills
(daemon/src/skills/), surfaced to the model through two meta-tools, and an **open
standard** anyone can extend — in-tree, via an external manifest, or through MCP /
Self-Forge. It is **infinitely extensible**.

It is **not** a populated community marketplace, and there is **no** "13,700
skills" claim anywhere in the product. The catalog reports the **real shipped
count** — the genuine number of `SkillDef`s aggregated from the category modules
— and nothing else. When the model lists skills it says "hand-written in-tree
library … not a community marketplace." Honesty is load-bearing: a skill that
would need a live external source it does not have returns "needs a data source",
never a fabricated value.

## What a skill is

A **skill** is one unit of capability: a `snake_case` name, a category, a
description of *when* to use it, trigger cues, a **pure** `run` function over
JSON args, a `consequential` flag, and an optional `source_gated` marker. The
canonical type is `daemon/src/skills/mod.rs::SkillDef`.

### The `SkillDef` shape

```rust
pub struct SkillDef {
    pub name:          &'static str,        // unique, snake_case (guard-enforced)
    pub category:      Category,            // one of the 8 category slugs
    pub description:   &'static str,        // WHEN to use it (one line, for skill_list)
    pub cues:          &'static [&'static str], // trigger phrasings (discovery/docs)
    pub run:           RunFn,               // fn(&serde_json::Value) -> anyhow::Result<String>
    pub consequential: bool,                // mutates/acts outside the process? (default false)
    pub source_gated:  bool,                // needs an external feed not bundled? (default false)
}

// The run signature every skill has — a PURE map from JSON args to a result.
pub type RunFn = fn(&serde_json::Value) -> anyhow::Result<String>;
```

A skill is built with the constructor + builders:

```rust
SkillDef::new(name, category, description, cues, run)   // pure, read-only (the common case)
    .consequential()   // optional: it acts outside the process -> routed through the gate
    .source_gated()    // optional: needs an external feed -> returns "needs a data source"
```

A skill may be `consequential` **or** `source_gated`, never both — those are
distinct gating models and the registry guard rejects the combination.

## Purity + determinism (the design rule)

A skill's `run` is a **pure function**: no network, no clock-without-injection, no
randomness-without-seed, no ambient I/O. That is what makes the library flawless
and hermetically testable.

- Need "now"? Take an **injected** instant as an argument (ISO string or epoch
  seconds). Never read the wall clock. (See the `datetime` category contract.)
- Need randomness? Take a **required `seed`** argument and use a deterministic
  PRNG. The proof skill `dice_roll` does exactly this. (See `fun`.)
- Need a live external source (online dictionary, FX rate, weather)? Either
  **omit** the skill this round, or ship it read-only and `source_gated`: its
  `run` returns a "needs a data source" notice until one is configured, and it
  **never fabricates**.

## Consequential skills + the gate

A skill that **mutates or acts outside the process** is `consequential`. It
routes through the **exact same** cross-turn confirmation gate a built-in
consequential tool uses:

1. On a first `skill_invoke`, with the master switch
   (`[integrations].allow_consequential`, ships **ON** — armed by default) on, the
   meta-tool **parks** the exact `{agent, tool, input}` and hands back a spoken
   confirmation prompt. The skill's `run` does **not** fire.
2. Only a real human "yes" on a **later** turn replays the parked action, with the
   gate now `Execute`. The replay runs the skill verbatim — nothing re-derived
   from the confirming utterance.
3. With the shipped default (the switch **on**) a parked consequential skill
   fires only after a fresh per-action spoken confirm replays it as `Execute`.
   Setting the switch **false** is the operator action that forces
   `integrations::gate(confirm)` to always be `DryRun`, so a consequential skill
   then only ever **previews** and fires nothing.

The park decision keys on the **named skill**, not the meta-tool: `skill_invoke`
itself is a pure dispatcher and is *not* in `confirm::CONSEQUENTIAL_TOOLS`;
`anthropic.rs::skill_invoke_is_consequential` widens the park condition to cover a
consequential skill. A `consequential` skill parks "exactly like a built-in
consequential tool" — that invariant is pinned by tests.

**No skill ships consequential or source-gated this round** — the shipped library
is entirely pure + read-only. The flags and the gate are the framework's
extension contract for the library phase and beyond.

## The two meta-tools

The library is exposed through **exactly two** tools added to `anthropic.rs`
(`tool_defs` / `execute_tool` / the mirror test), so the catalog never bloats the
tool surface no matter how many skills exist:

- **`skill_list`** `{category?}` — READ. Returns the catalog (name + category +
  marker + one-line description) so the model can discover and choose. Optional
  `category` narrows to one heading. States the real shipped count.
- **`skill_invoke`** `{name, args?, confirm?}` — dispatch into the registry. A
  pure skill runs immediately and deterministically; a consequential skill parks
  for a spoken yes; a source-gated skill reports it needs a data source; an
  unknown skill is a friendly error. Leave `confirm` absent/false — only the
  confirmation replay sets it true.

### Per-agent allowlist

Pure read-only skills are broadly safe, but the meta-tools sit on a sensible
allowlist: the orchestrator **darwin** (via its `["*"]` wildcard) plus the
utility/knowledge agents **friday**, **mnemosyne**, and **sage**. A consequential
skill set is never silently granted to every agent — and a consequential skill
still parks behind the confirmation gate regardless of which agent invoked it.

## Categories (8)

The 8 category modules are each `daemon/src/skills/<cat>.rs`, each with a public
`skills()` returning a `Vec<SkillDef>`. The registry aggregates them in this
order:

| slug        | what lives here                                                        |
|-------------|------------------------------------------------------------------------|
| `utilities` | general glue: encoders, counters, deterministic helpers (proof skills) |
| `text`      | pure text inspection + transforms (case, reverse, trim, slug)          |
| `datetime`  | calendar/clock arithmetic over **injected** instants (never wall clock)|
| `units`     | unit conversions with exact / well-known constant factors              |
| `mathx`     | extended math: bases, gcd/lcm, combinatorics, prime checks             |
| `knowledge` | reference lookups over **bundled** data; live sources are source-gated |
| `finance`   | money math that is **pure arithmetic**; never a live quote             |
| `fun`       | playful helpers, deterministic **with a seed**                         |

The proof skills (`base64_encode`, `word_count`, `dice_roll`) live in `utilities`
and exercise the whole path (registry → `skill_list` → `skill_invoke` → pure run)
end-to-end.

## Adding a skill in-tree (the recipe)

Skills are added by editing **one category file** — `mod.rs` never changes — so
multiple authors fill different categories with no shared edits.

1. Open the category file, e.g. `daemon/src/skills/text.rs`.
2. Write a **pure** private `fn`:

   ```rust
   fn reverse_text(args: &Value) -> Result<String> {
       let text = args.get("text").and_then(Value::as_str)
           .ok_or_else(|| anyhow!("reverse_text needs a 'text' string argument"))?;
       Ok(text.chars().rev().collect())
   }
   ```

3. Add a `SkillDef` to that file's `skills()` vec:

   ```rust
   SkillDef::new(
       "reverse_text",
       Category::Text,
       "Reverse a string character by character. Use when the user wants text reversed.",
       &["reverse", "backwards", "flip the text"],
       reverse_text,
   ),
   ```

4. Add unit tests in the same file (known vectors + a bad-args case + a
   determinism check). The registry guard (snake_case + global uniqueness, and
   not-both-flags) runs at startup and is pinned by tests, so a duplicate or
   malformed name fails fast.

That's it — the registry picks the skill up automatically and `skill_list`
surfaces it.

## The external-manifest extension point (open standard)

In-tree skills are the foundation; the **manifest** is how the library grows
beyond the tree. A skill manifest is a declarative description an external loader
reads. The standard manifest fields mirror `SkillDef` (a manifest can only ever
declare what a `SkillDef` carries — there is no hidden capability):

```toml
# skill.toml — one skill (or [[skill]] array for many)
name          = "reverse_text"     # unique snake_case id
category      = "text"             # one of the 8 category slugs
description    = "Reverse a string character by character."
cues          = ["reverse", "backwards"]
consequential = false              # default false; true -> routes through the gate
source_gated  = false              # default false; true -> "needs a data source" until configured
# `run` is NOT inline code: a manifest references a vetted, sandboxed
# implementation (an in-tree handler id, or an MCP tool, or a Self-Forge app),
# never arbitrary code loaded blind. An external `run` runs under the micro-app
# SBPL sandbox (docs/SANDBOX.md), default-deny, with only the capabilities the
# manifest declares.
```

**Loader status (this round).** The in-tree registry + this standard are real and
shipped. Full external-manifest *loading* is a thin read-first stub: the standard
is documented and the in-tree path is the source of truth. A future loader reads
`skill.toml` manifests from a skills directory, validates each against the
`SkillDef` contract (snake_case, unique, not-both-flags), binds `run` to a vetted
sandboxed handler, and registers it — applying the **same** gating model: a
consequential manifest skill parks; a source-gated one reports it needs a source.
A manifest can never smuggle in an ungated side effect.

## Other extension paths

- **MCP** (docs: `[mcp]` in config) — an MCP server's tools are a parallel,
  runtime-discovered tool surface with the same consequential-park gate and
  per-server agent allowlist. Skills and MCP tools share the no-double-underscore
  rule so the two namespaces never collide.
- **Self-Forge** — DARWIN can draft a new sandboxed micro-app (propose-only,
  human-gated); a forged tool can back a manifest skill's `run`.

## Config

```toml
[skills]
enabled = true   # SHIPS ON — pure skills are safe to offer by default. This only
                 # governs whether the meta-tools are offered. A consequential skill
                 # is ALWAYS parked behind the confirmation gate + the armed-by-default
                 # [integrations].allow_consequential switch (ON, but a confirmed action
                 # still needs a fresh per-action confirm) when invoked — this
                 # flag never lets a side-effecting skill fire unconfirmed.
```

## Invariants (pinned by tests)

- Every skill name is `snake_case` and **globally unique** (registry guard).
- A skill is never both `consequential` and `source_gated`.
- The 8 category modules each compile and expose `skills()` (empty is fine).
- `skill_list` returns the catalog with the **real** count and honest framing.
- `skill_invoke` runs a pure skill **deterministically**; an unknown skill is a
  friendly error; bad args are a friendly error.
- A consequential skill **parks** (never auto-runs) — proven with a test skill
  whose `run` panics if ever executed without a confirmed `Execute` gate.
- The two meta-tools are in the tool surface (mirror test) and on the
  darwin/friday/mnemosyne/sage allowlists only.
