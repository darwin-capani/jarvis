# DARWIN Architecture

This is the authoritative architecture document. Where other documents or earlier plans disagree with this one, this one wins.

## Constraint corrections

These are engineering facts, not opinions. The project plan is built on them.

1. **The base OS is macOS, used as an invisible host kernel — required across all Apple Silicon.** MLX (the Apple-GPU Metal backend) and Core ML/ANE access exist only on macOS, so macOS is the host on every Apple Silicon chip, not a chip-specific choice. `darwind` runs as a LaunchAgent, the Phase-2 HUD is a fullscreen always-on-top app, and the macOS UI is never shown. There is no Linux, no Wayland, and no display-server replacement anywhere in this design. (Asahi Linux is additionally a non-starter on M4-generation silicon: the m1n1 bootloader does not work there and there is no timeline. But even on M1/M2 chips where Asahi does boot, MLX/Core ML/ANE still need macOS — so the macOS-host decision holds regardless of chip.)

2. **MLX does not run on Linux for Apple GPUs, and does not use the Neural Engine.** MLX's only non-Mac backend is CUDA; there is no Linux backend for Apple GPUs. MLX executes on the **Apple GPU via Metal** — the same Metal path on every Apple Silicon chip (M1 or later). The Neural Engine (ANE) is reachable only through Core ML. DARWIN reserves the ANE for Phase-3 auxiliary models — wake-word, VAD, and embeddings exported to Core ML, which Core ML may schedule onto the ANE (scheduling is Core ML's decision, not ours).

3. **"Zero-latency" is marketing, not physics.** The real, measurable targets (M4-class; M1/M2/M3 proportionally slower, since LLM decode is memory-bandwidth-bound) are:
   - local intent classification: **< 300 ms**
   - STT: **< 1 s** per utterance
   - first generated token: **< 500 ms**

## Boot experience

Power button to DARWIN with zero interaction:

```
power button
 └─▶ Apple iBoot (proprietary; non-replaceable on M4 — see correction 1)
      └─▶ macOS, as the invisible host kernel
           └─▶ auto-login (DARWIN user)
                └─▶ launchd LaunchAgents (KeepAlive — relaunched if they exit)
                     ├─ com.darwin.inference → boot/run_inference.sh → MLX server
                     └─ com.darwin.daemon    → boot/run_daemon.sh    → darwind
                          └─▶ DARWIN live: mic loop, telemetry, routing
```

Installed by `scripts/install_boot.sh` (dry-run by default; `--install` applies, `--uninstall` removes). The boot wrappers source `state/env.sh` (gitignored, chmod 600) for `ANTHROPIC_API_KEY`.

The Phase-2 HUD completes the takeover: launched the same way, it runs as a fullscreen shell-replacement app so the macOS UI is never seen. Until it ships, the stock desktop is visible behind a fully working DARWIN.

This is the maximum achievable boot integration on M4-generation silicon today. iBoot cannot be replaced and macOS cannot be removed (see Constraint corrections, items 1–2); everything after login **can** be owned, and this owns all of it.

## Component diagram

```
                ┌──────────────────────────── darwind (Rust, LaunchAgent) ────────────────────────────┐
                │                                                                                      │
 mic ──▶ audio loop ──▶ VAD (RMS gate: rms_threshold / silence_ms / min_speech_ms)                     │
                │             │ utterance WAV → state/tmp/                                             │
                │             ▼                                                                        │
                │   ┌──── inference IPC client ────┐        ┌── telemetry WS server 127.0.0.1:7177 ──┐ │
                │   │ state/ipc/inference.sock     │        │ broadcasts JSON events to HUD clients  │ │
                │   └───────────┬──────────────────┘        └────────────────────────────────────────┘ │
                │               │                                                                      │
                │   transcribe ─┴─▶ classify ──▶ router ──┬──▶ local handler (streamed op: converse)   │
                │                                         └──▶ Anthropic cloud (Messages API)          │
                │               gapless playback: rodio sink on a dedicated thread (afplay/say fallback)│
                │                                                                                      │
                │   micro-app host: seatbelt-sandboxed apps on per-app sockets (apps.rs, SANDBOX.md)   │
                │   self-heal watchdog (ships ON; PROPOSE-ONLY, inert without a cloud key)             │
                └──────────────────────────────────────────────────────────────────────────────────────┘
                                │
                                ▼
                 inference server (Python 3.11 + MLX, Metal GPU)
                 - whisper STT  - intent classifier  - local LLM  - Kokoro TTS
                 (SQLite memory at state/darwin.db is owned by darwind via memory.rs)
```

Pipeline: **audio loop → VAD → (optional instant opener) → STT → intent classify → router → (local handler + real actions → streamed `converse` | Anthropic cloud tool loop → per-sentence `speak`) → gapless rodio playback**. The instant-opener stage is opt-in (`[speech].instant_opener`, **ships OFF**); by default no canned opener fires and the streamed `converse` reply is the first audio. When it is enabled, the moment the VAD hands over an utterance — before transcription even starts — the daemon opens the reply session, waits the 300 ms opener breath (`[speech].opener_delay_ms`, concurrent with STT), and plays a pre-synthesized acknowledgement WAV from the opener bank (see Voice → Instant acknowledgment), so DARWIN is audibly responding a natural beat after the 350 ms VAD window closes. Local LLM-voiced replies then stream sentence-by-sentence out of one `converse` request into the **same** sink — the first content sentence is audible while the model is still decoding the rest. Every stage emits telemetry events; `pipeline.completed` sums them per utterance (including `first_audio_ms`, the latency the user actually perceives).

## Configuration

Canonical file: `config/darwin.toml`. Both `darwind` and the inference server read it at startup and fall back to hardcoded defaults (identical to the file's values) if it is missing. The daemon recognizes **~38 config sections** (see `config/darwin.toml` for the full schema and every key); this table covers the core pipeline sections and their headline keys:

| Section | Keys |
|---|---|
| `[audio]` | `rms_threshold = 0.015`, `silence_ms = 350`, `min_speech_ms = 250` |
| `[models]` | `llm = "mlx-community/Qwen3-4B-Instruct-2507-4bit"`, `stt` (MLX Whisper model; config-driven, benchmark-selected), `classifier = ""` (dedicated small resident model for `classify`; empty reuses the main LLM — see Routing policy) |
| `[router]` | `cloud_confidence_threshold = 0.6`, `conversation_route = "cloud_heavy"` (where the `conversation` intent is answered: `"cloud_heavy"` cloud Opus / `"cloud_fast"` cloud Haiku / `"local"` the 4B — see Routing policy) |
| `[cloud]` | `fast_model = "claude-haiku-4-5"`, `heavy_model = "claude-opus-4-8"`, `max_tokens = 4096` |
| `[speech]` | `engine = "kokoro"` (`"kokoro"` \| `"csm"` \| `"orpheus"` — see TTS engine evaluation), `model = ""` (explicit HF repo; empty = the engine's default), `voice = "bm_george"`, `speed = 1.2` (kokoro only), `sentence_pause_ms = 250` (daemon-inserted silence between consecutive clips), `instant_opener = false` (master gate; **ships OFF** — no canned opener fires by default, the `converse` stream is the first audio; the opener machinery below is opt-in), `opener_delay_ms = 300` (the opener breath — see Instant acknowledgment), `openers = ["Right away, sir.", "Of course.", "One moment.", "On it, sir.", "Let me see."]` (instant-acknowledgment bank) |
| `[inference]` | `preload = true` |
| `[self_heal]` | `enabled = true` (master gate; ships ON, PROPOSE-ONLY, inert without a cloud key), `mode = "propose"` (`"propose"` \| `"auto"` — `"auto"` additionally requires `enabled = true` and is dangerous: it applies the validated patch, rebuilds, and exits the daemon; KEEP `"propose"` — see Self-heal) |
| `[standing]` | `enabled = true` (subsystem master gate; ships ON — establishing is still confirmation-gated and every consequential step a run proposes still parks; when false the pure scheduler marks NOTHING due so no standing mission fires on the live tick — see Standing Missions) |
| `[telemetry]` | `port = 7177` |

`[models].classifier` is accuracy-gated: a candidate ships only after passing the intent eval — now **11 cases** (the original 7-case regression set, one per pre-web intent with every "heavy" case still routed heavy, plus 4 web cases that also check `args`: url/browser/query extraction, including the once-failing "Open the official apple website on safari.") — with at most one miss and **no heavy case routed light, no web case missing a usable url/query**. As of 2026-06-12 every small candidate failed the (then 7-case) gate (Qwen3-0.6B-4bit 4/7, Qwen2.5-0.5B-Instruct-4bit 4/7 + missed a heavy case, Llama-3.2-1B-Instruct-4bit 3/7; the main 4B scores 11/11 on the current set), so the key is empty and `classify` runs on the main LLM with its KV-cached prefix.

### Credential panel (HUD settings)

The HUD's gear → Settings is a **multi-credential registry** with a single source of truth mirrored Rust↔TS (`hud/src-tauri/src/credentials.rs` ↔ `hud/src/core/credentials.ts`). Each entry is `{ id, label, keychain_account, kind }`. The registry has grown well beyond v1 — the source of truth is `hud/src/core/credentials.ts` (mirrored into the Rust allowlist). Representative **bearer** creds `anthropic` (`anthropic_api_key`), `github` (`github_pat`), `slack` (`slack_bot_token`) are pasted, then **verified live before storing**; **oauth**-kind entries (e.g. the Google and X client-id/secret ids) are deferred Connect placeholders (not paste-verifiable). Verify-now endpoints: Anthropic `GET /v1/models`, GitHub `GET api.github.com/user` (with `User-Agent`), Slack `POST auth.test`. `verify_and_store(id, secret)` (the Enter action) writes to the Keychain **only when the secret verifies** — an unverified secret is never stored.

Keychain access is **in-process** via `security-framework` (service `com.darwin.daemon`), so no secret reaches any subprocess `argv`. The backend validates every `account` against the registry **allowlist** and rejects unknown accounts, so the frontend cannot write arbitrary Keychain items. Secrets are never logged and only leave the process inside the verify request's auth header. The Anthropic account stays exactly `anthropic_api_key` (the daemon reads it at startup — restart after changing it). **Storing a credential is not the same as the integration working**: this build verifies + persists tokens; the agents that consume them (e.g. Steve → GitHub PRs, see `docs/ROADMAP.md`) are a separate build.

## Inference IPC contract

Transport: Unix domain socket at `state/ipc/inference.sock`. Framing: newline-delimited JSON — one JSON object per line. The daemon is the client; the inference server is the listener.

Request:

```json
{"id": str, "op": "transcribe"|"classify"|"generate"|"converse"|"speak"|"extract_facts"|"consolidate",
 "path"?: str, "text"?: str, "max_tokens"?: int, "voice"?: str,
 "history"?: [{"speaker": "user"|"darwin", "text": str}],
 "facts"?: [str], "data"?: str, "response"?: str, "opener_spoken"?: str,
 "transcripts"?: [{"user": str, "darwin": str}]}
```

`op=consolidate` reads `transcripts` (≤ 40 exchanges, oldest first) and a differently-shaped `facts`: `[{"key": str, "value": str}, ...]`.

Response (all ops except `converse` — exactly one line per request):

```json
{"id": str, "ok": bool, "text"?: str, "path"?: str, "intent"?: str, "confidence"?: float, "complexity"?: "light"|"heavy", "args"?: object, "facts"?: [{"key": str, "value": str}], "error"?: str, "latency_ms": int}
```

Field usage by op:

| op | request fields | success response fields |
|---|---|---|
| `transcribe` | `path` (audio file, e.g. under `state/tmp/`) | `text`, `latency_ms` |
| `classify` | `text` | `intent`, `confidence`, `complexity`, `args` (the model's extracted-params object, passed through verbatim when it is a JSON object, `{}` on absence or any parse trouble — never fabricated server-side), `latency_ms` |
| `generate` | `text`, `max_tokens`?, `history`? (≤ 6 exchanges, oldest first), `facts`? (strings), `data`? (verified system data) | `text`, `latency_ms` |
| `converse` | `text`, `max_tokens`?, `history`?, `facts`?, `data`?, `voice`?, `opener_spoken`? (the opener line the daemon already played aloud — see the no-double-ack note below) | streamed — see the event table below |
| `speak` | `text`, `voice` (optional; default `[speech].voice`) | `path` (synthesized WAV under `state/tmp/`), `latency_ms` |
| `extract_facts` | `text` (user utterance), `response`? (what DARWIN replied) | `facts` (≤ 3 `{key, value}`), `latency_ms` |
| `consolidate` | `transcripts` (≤ 40 `{user, darwin}` exchanges, oldest first), `facts` (`[{key, value}]` — the stored facts table) | `upserts` (`[{key, value}]`), `deletes` (`[key]`), `latency_ms` |

On failure: `ok = false`, `error` set, `latency_ms` still populated. `id` is echoed verbatim so the daemon can correlate concurrent requests.

### `converse` — streamed generate+TTS in one request

The latency-critical op: one request fuses persona generation and sentence-by-sentence TTS, so the first sentence is playable while the model is still decoding the rest. The server responds with **multiple JSON lines**, all carrying the same `id`, in order:

| event | fields | when |
|---|---|---|
| `sentence` | `{"id", "event": "sentence", "seq": <0-based int>, "text": "<sentence>", "path": "<abs WAV path under state/tmp/>"}` | one per spoken sentence, emitted **as soon as that sentence is synthesized** |
| `done` (success) | `{"id", "event": "done", "ok": true, "text": "<full reply text>", "sentences": <int>, "first_sentence_ms": <int>, "latency_ms": <int>}` | terminal line |
| `done` (failure) | `{"id", "event": "done", "ok": false, "error": str, "latency_ms": int}` | terminal line on mid-stream failure; sentence events already emitted remain valid |

Implementation: one GPU-locked worker thread runs `mlx_lm.stream_generate` with the persona KV cache and the persona sampler; each time the **decimal-aware sentence splitter** (same `. ! ?` newline semantics as the daemon's `split_sentences`, with a hold-back for a possible decimal point split across tokens) completes a sentence, decode pauses, that sentence is synthesized (silence-trimmed, same engine-dispatched TTS path), the `sentence` event is handed to the asyncio writer via `loop.call_soon_threadsafe`, and decode resumes. At most **5 sentences** are synthesized per reply; later text is returned in `done.text` only. The persona KV cache is trimmed back to the static prefix even on mid-stream exceptions.

**No double-ack (`opener_spoken`):** when the daemon already played an instant opener for this reply, it passes that opener's text as `opener_spoken`. The server then (1) appends a bracketed continuation note to the final user turn — `[You already began your reply aloud with: '<opener>'. Continue naturally from there - do not repeat any acknowledgement or greeting; go directly to the substance.]` — and (2) **seeds the assistant turn of the prompt with the opener text itself**, so decoding continues mid-reply after it (measured: with the note alone, Qwen3-4B parrots the quoted opener verbatim; with the seed, replies start at the substance). Sentence events and `done.text` contain only the new content — exactly what the daemon must append after the opener WAV it already played. Verified behaviorally: status replies with `opener_spoken` set never begin with an acknowledgement or greeting and preserve all data numbers verbatim.

Daemon side (`inference.rs::converse`): send the request, then read lines until the matching `done` event — 30 s budget for the done event, 15 s max between lines. Each `sentence` event's WAV is handed to the playback sink immediately. A transport failure before any sentence event gets one stale-connection retry; after sentences were emitted, failures are final (no double-speak).

### `generate` / `converse` message assembly

The server builds a chat-format prompt from the extended request (`converse` uses the identical assembly, KV cache, and sampler):

1. **System message** = `inference/prompts/persona.txt` + (if `facts` present) `"\n\nWhat you know about the user (from prior conversations):\n- <fact>"` per fact.
2. **History** = the `history` entries as alternating user/assistant turns, oldest first.
3. **Final user turn** = `text`; if `data` is present, append `"\n\n[Verified system data — convey this naturally in your own words; do not invent or alter numbers: <data>]"`; if `opener_spoken` is present (`converse` only), additionally append the continuation note described above.

The persona-only prefix is **KV-cached** the same way the classifier prefix is — facts, history, and the user turn come after the cached prefix, so the persona never re-prefills. Measure warm `generate` latency with and without the cache when touching this path.

**Per-agent persona override (`converse` only).** The constellation (below) routes each turn to one of 27 agents, each with its own persona file. The daemon passes an optional `persona` **name** on the `converse` request (the agent id, e.g. `"vision"`); the server maps it to `inference/personas/<name>.txt` (validated as a lowercase slug before any path join — never raw-joined, so the field can never traverse) and uses that text as the **system prefix for that call instead of** `prompts/persona.txt`. Because an agent prefix differs from the base-persona prefix the shared KV cache was prefilled with, a persona-override call **bypasses the base KV cache** (full prompt tokenized fresh; the base cache is left untouched for the next default-persona turn). The persona text is loaded once per agent and cached in-process. When `persona` is absent/None the behavior is exactly as before (base persona + KV cache) — an old server simply ignores the extra field, so the daemon is backward-compatible and every agent harmlessly speaks the base DARWIN persona until the server is wired. Voice is independent: the agent's Kokoro `voice` rides the same request, so each agent both *phrases* (persona) and *sounds* (voice) like itself.

### `extract_facts`

Prompts the local LLM to extract **at most 3 durable facts** worth remembering about the user from the exchange — name, preferences, projects, hardware, recurring goals — with namespaced keys (`user.name`, `user.preference.<x>`, `user.project.<x>`, `context.<x>`). Trivia and one-off commands yield an empty list. **Corrections are honored:** when the user contradicts or corrects an earlier fact ("actually my name is...", "no, I prefer..."), the corrected fact is emitted under the **same key** with the new value — the correction is the durable fact, not the stale one. The server strict-JSON-parses the model output and falls back to an empty list on any parse failure; `max_tokens ≤ 120`.

### `consolidate` — self-learning reflection pass

The daemon's reflection task (see Learning loop → Reflection) periodically hands the server the last ≤ 40 transcripts plus the current facts table; the server asks the LLM (greedy decode, `max_tokens ≤ 400`, strict JSON with empty-arrays fallback) to merge duplicate/overlapping facts, prefer the newest phrasing on conflicts, generalize only clearly recurring patterns, and DELETE facts contradicted by later user statements — conservative by design: when unsure, change nothing. The parse layer enforces what the model is not trusted with: keys must be namespaced, deletes may only name keys present in the input facts, `meta.`-prefixed keys are dropped wherever they appear, no-op upserts and upsert/delete collisions are filtered, and total changes cap at 12 per pass (upserts take precedence).

**Habit mining.** The same pass mines clearly recurring user patterns into `user.habit.<slug>` facts (e.g. three morning status checks → `user.habit.morning_status_check = "asks for a system status most mornings"`). Strictly conservative, and enforced in two layers: the prompt instructs ≥ 3 separate occurrences across the provided transcripts, counted from the user's lines only (the assistant's replies are never evidence, even when they claim the user does something regularly); on top of that, `habit_supported` is a **deterministic backstop** that drops any `user.habit.*` upsert not evidenced by at least 3 separate user lines sharing a content word with the habit's slug+value — the 4B greedy model parrots worked examples, so the prompt alone cannot be trusted with this rule. Habit facts go through the same parse guards and caps as everything else and are deletable like any other fact.

## Routing policy

The local classifier returns `{intent, confidence, complexity, args}` — `args` is the model's extracted-params object (e.g. `{"url": "apple.com", "browser": "safari"}` for `web.open`), carried into the daemon as `Classification.args: serde_json::Value` with `serde(default)`, so an old server that omits the field deserializes as `Null` and the router treats `Null` exactly like `{}`. Route to cloud **iff** `complexity == "heavy"` **or** `confidence < cloud_confidence_threshold`; otherwise handle locally. Cloud requests use `[cloud].fast_model` by default and `[cloud].heavy_model` when `complexity == "heavy"`.

**Conversation routes to cloud Opus by default (`[router].conversation_route`).** The `conversation` intent — casual chat, greetings, opinions; the pure `llm_voice` conversation path, **not** actions/`system.query`/memory ops — is answered by the **cloud** by default, because the local 4B (Qwen3-4B) is near-deterministic on bare greetings even at max temperature ("Hi DARWIN" → the same warm line every time): a model-capacity ceiling. `conversation_route` (default `"cloud_heavy"`) selects the brain that answers chat: `"cloud_heavy"` → cloud Opus (`[cloud].heavy_model`) for genuinely varied, human personality; `"cloud_fast"` → cloud Haiku (`[cloud].fast_model`); `"local"` → the resident 4B. The cloud variants are a **plain persona completion** (`anthropic::complete_persona`: persona + recent history + namespaced facts + the utterance, `GENERATE_MAX_TOKENS` ≈ 200 so replies stay conversational) — **not** the tool loop, so a greeting can never trigger a tool call. They require the cloud key; **with no key, an unknown route value, or any cloud error, conversation degrades gracefully to the local 4B converse path** (the offline/Hulk path) — never silent. A conversation-to-cloud turn emits `route.cloud` (source `cloud`, with the model and `conversation: true`); the local fallback emits `route.local`. The constellation still shows the active agent — this only decides which brain phrases the reply. One config line (`conversation_route = "local"`) reverts chat to the 4B. The heavy/low-confidence cloud routing above, actions, stats, and memory ops are all unchanged.

**Anti-repeat hint — the variation mechanism (Opus takes no temperature).** Routing chat to Opus is necessary but not sufficient: a bare "Hi DARWIN" with an identical prompt still collapses onto one peaked sequence (measured: byte-identical "Hello, sir." on every Opus call). **Opus 4.8 accepts no `temperature`/`top_p`/`top_k` — sending any of them 400s** — so sampling pressure is not an available lever. The *prompt* is the only lever, so `complete_persona` takes an `avoid: &[String]` of DARWIN's recent replies; when non-empty it folds them into the `system` prompt as an explicit "do NOT reuse this wording: <list>; say something fresh" instruction (`avoid_instruction`, a pure helper). Because that note **changes the prompt on every call**, identical user input can no longer collapse to one output. `router.rs` pulls the last `AVOID_RECENT_REPLIES` (4) DARWIN replies from history (`recent_replies`, freshest-first, blanks dropped) and passes them as `avoid`; an empty list (a first turn) leaves the prompt untouched. The body still carries **no `tools`, no `thinking`, and no sampling param** — the avoid-list is the entire variation lever. This pairs with the **unpinned greeting persona** (see *Persona*): the prompt previously instructed a bare hello to usually be just "Hello, sir." and to "leave it at that", which peaked the distribution; that guidance was rewritten to greet warmly and *differently* each time with several distinct example greetings, while keeping every grounding rule (no fabricated weather/time-of-day/schedule). Prompt-level variation (unpinned persona + per-call avoid hint) is therefore the *only* mechanism that forces greeting variation, since the model offers no sampling knob.

Intents: `app.launch`, `app.control`, `system.query`, `file.op`, `docsearch.index`, `docsearch.forget`, `memory.store`, `memory.recall`, `web.open` (args `{url, browser?}` — the model supplies the canonical domain for well-known sites), `web.search` (args `{query, browser?}`), `conversation`. `docsearch.index` (re)builds and `docsearch.forget` clears the on-device index of the user's own allowlisted folders (distinct from `file.op`, a one-off OS file search). Both web intents are always `light`. The prompt lives in `inference/prompts/intent_classifier.txt`; its few-shots include the once-failing utterance "Open the official apple website on safari." verbatim.

### Local response flow — no fixed strings

Local intent handlers do not produce final prose. Each returns a `HandlerOutput { data: String, llm_voice: bool }`:

| intent | `data` | `llm_voice` |
|---|---|---|
| `system.query` | live stats string from the **SystemSnapshot cache** (see below) | true |
| `memory.store` | upsert, then `"Stored fact: <text>"` | true |
| `memory.recall` | the recalled facts (internal `meta.` keys filtered out) | true |
| `app.launch` / `app.control` | **real action**: extract the app name from the utterance (words after the trigger verb minus stopwords; the whole utterance feeds the fuzzy matcher as fallback), then decide what the utterance actually asks for **before any process spawns**: quit-class verbs (quit/close/exit/stop/kill) → `actions::quit_app` (a quit must never feed the launcher); an `app.launch` whose remainder smells of the web (words website/web/site, or a `.com`/`.org`/`http` fragment) → **rerouted to the `web.open` handling** with the classifier args (belt-and-suspenders: the original "open the official apple website on safari" failure stays dead even if the classifier says `app.launch`); else `actions::open_app` → the outcome string ("Opened Safari." / candidate list / not-found) | true |
| `web.open` | **real action**: `actions::open_url(args.url, args.browser)` when the classifier supplied a URL; otherwise fall back to a web search over the utterance's content words (the user clearly wanted the web — guessing a domain would be worse) | true |
| `web.search` | **real action**: `actions::search_url(args.query, args.browser)`, content words when `args.query` is absent | true |
| `file.op` | **real action**: Spotlight search (`actions::search_files`) on the utterance's content words, ≤ 5 hits newest first; if exactly one match comes back and the utterance says "open", `actions::open_path` opens it too | true |
| `conversation` | empty (cloud Opus by default — see *Conversation routes to cloud Opus by default* above; the local converse path is the offline fallback) | true |

When `llm_voice` is set on a local route, the daemon makes **one `converse` call** with `text` = the utterance, the handler's `data`, `history` = `memory.recent_exchanges(6)`, and `facts` = `memory.all_user_facts(12)` (internal `meta.` bookkeeping keys never reach a prompt) — so every spoken reply is phrased by the LLM in persona, informed by verified handler data, conversational context, and what DARWIN knows about the user, and starts playing while the model is still decoding. Converse replies are **not** pre-clipped by `clip_for_speech` (the persona keeps them short; the server's 5-sentence synthesis cap guards the rest) — clipping stays on the cloud/`speak` path.

Fallback ladder: if `converse` fails **before any audio played** (inference server down, mid-decode error with nothing emitted), the daemon falls back to the non-streamed `generate` → per-sentence `speak` path; if `generate` also fails, it speaks the raw `data` string directly — graceful degradation, not a canned personality. A converse failure **after** audio started keeps the sentences that played as the reply (never double-speaks). All fallback rungs keep appending to the same reply session the opener started.

**SystemSnapshot cache:** telemetry's 2 s `system_load_task` publishes every reading — `SystemSnapshot { cpu_percent, mem_used_bytes, mem_total_bytes, disk_free_bytes, disk_total_bytes, uptime_secs }` — into a shared `static RwLock` (`telemetry::latest_snapshot()`). The snapshot now carries the volume total alongside free bytes (both read from the SAME volume) so EDITH can ground a real disk-free percentage. The `system.query` handler reads that cache instantly; values up to one 2 s tick stale are invisible in spoken stats. Only when the cache is still empty (task not ticked yet, unit tests) does the old direct sysinfo read run — it needs two CPU refreshes ~250 ms apart, which is exactly the sleep this cache removed from the request path.

### Cloud response flow

`anthropic.rs` builds its context from the same ingredients as the local path: the persona text (the daemon reads `inference/prompts/persona.txt` from disk at startup — single source of truth for both processes) + the facts block as the `system` prompt, with `memory.recent_exchanges(6)` as real alternating chat turns and the live utterance last. Cloud routing goes through **`complete_with_tools`** — the tool-use loop below — so any phrasing of a request routed to the cloud can still act on the machine before the spoken answer comes back. On any cloud failure the router degrades to the local model (`generate` with a cloud-degrade data note) rather than going silent.

### Anthropic cloud — Messages API with the tool loop

`POST https://api.anthropic.com/v1/messages`

Headers:

```
x-api-key: <resolved key>
anthropic-version: 2023-06-01
content-type: application/json
```

**API key resolution** (`anthropic.rs`): the `ANTHROPIC_API_KEY` environment variable wins; otherwise the macOS Keychain item the HUD settings panel writes — service `com.darwin.daemon`, account `anthropic_api_key` — read via `/usr/bin/security find-generic-password -s com.darwin.daemon -a anthropic_api_key -w` (args-only `Command`, 5 s timeout, `kill_on_drop`; blank/whitespace values count as absent). Resolution runs **once** into a `OnceLock` — `main()` resolves eagerly at startup so the `daemon.started` telemetry event can carry `cloud_key_present: bool`, and the cloud router reuses the cached outcome. The key value itself never reaches logs or telemetry; only its presence (a bool) is ever reported. Because resolution is once-per-process, **changing the key (env or Keychain) requires a daemon restart**.

Body: `{"model", "max_tokens": 4096, "system", "messages", "tools": [{name, description, input_schema}, ...]}`. Model ids are **exactly** `claude-haiku-4-5` and `claude-opus-4-8` — no date suffixes; do not append any.

The loop: when the response has `stop_reason == "tool_use"`, every `tool_use` content block `{id, name, input}` is executed and the conversation continues with `messages += [{role: "assistant", content: <full response content>}, {role: "user", content: [{type: "tool_result", "tool_use_id": <id>, "content": <outcome>, "is_error": bool}, ...]}]`. Hard bounds: **at most 6 model calls** per reply (the first five may use tools; the sixth is forced to a text answer via `tool_choice: {"type": "none"}` — the Messages API requires the `tools` param whenever messages carry `tool_use`/`tool_result` blocks, so "dropping tools" is realized as tool_choice none), 60 s per call, **400 s for the whole loop** including action execution. The call cap was raised 3 → 6 this round for **bounded multi-step tool reasoning** (a multi-read plan finishes in one turn); `TOOL_LOOP_BUDGET` is pinned ≥ calls × per-call + 15 s tool time by the `loop_budget_covers_all_calls_plus_tool_time` test, so raising the cap forces the wall-clock budget up in lockstep. The final text is the spoken response. Every executed tool emits `("system", "action.executed", {tool, outcome})` telemetry. Unknown tools and bad arguments come back to the model as `is_error` tool_results, never as daemon failures.

## Agent constellation (one engine, 27 profiles)

The "company of AI agents" is **27 personas on the one local engine**, not 27 models. Honesty is the cardinal rule: every agent shares the same 4B LLM + Kokoro TTS; what differs per agent is a **persona prefix, a Kokoro voice, a HUD core hue, a tool allowlist, and a memory namespace**. No agent has a capability the engine does not. The roster is canonical in `config/agents.toml` (27 `[[agent]]` tables), parsed into a typed `AgentRegistry` (`daemon/src/agents.rs`, serde + `deny_unknown_fields`); a missing/malformed/invalid file falls back whole to a hardcoded `canonical()` roster (kept in lockstep with the shipped file by a unit test) and surfaces the issue as telemetry, so the team always loads. `validate()` enforces unique names, exactly one orchestrator (`tools = ["*"]`), `hue ≤ 360`, `namespace == "agent.<name>"`, and non-empty voice/tools. The HUD mirrors the same name/role/hue table in `hud/src/core/agents.ts` for the idle roster render; the daemon's live `agent.active` hue always wins at runtime.

**EDITH, the Proactive Sentinel (anticipation engine).** The 16th agent (`af_sky` voice, teal hue 170) is the first place DARWIN surfaces things **unprompted**, so it ships conservative and echo-safe. Its heart is a **pure, deterministic evaluator** (`daemon/src/anticipate.rs::evaluate`): given a verified `Signals` snapshot (upcoming calendar events with lead time, important-unread mail count, system-health readings, an optional market delta, presence) + an injected clock + the carried `FiredState`, it returns `Nothing | Surface(brief) | Speak(brief)`. The brief is **grounded** — composed only from signal fields, never a model output, never a fabricated fact. Guards (all unit-tested): a relevance threshold per trigger, per-trigger cooldown, a rolling-window rate limit + min-gap debounce, and quiet hours (default 22:00–07:00 local, fully silent — not even a card). It **ships ON for speech**: `[proactive].speak` defaults `true`, so the live loop ALSO voices the brief — strictly through the existing `speech.rs::speak` path (so `is_speaking()`/`SPEAKING`/`MUTE_TAIL`/barge all cover it) and never while already speaking — in addition to the `proactive.surface` HUD card; set it `false` for a HUD card only. EDITH **watches but never acts**: any consequential follow-up routes through `integrations::gate()` at the owning agent. The pure evaluator is what the tests cover; the live tick in `main.rs::anticipation_task` is runtime-gated. On-demand, the read-only `edith_brief` / `edith_watch` tools let her work when asked.

**Darwin-Prime delegation (`router.rs::select_agent`).** After classify, a **deterministic, unit-tested rule map** picks the handling agent: first intent-driven (`app.*` → oracle, `web.open`/`file.op` → vision, `system.query` → ultron, `memory.*` → pepper), then whole-word keyword cues over a plain `conversation` intent (bug/build/PR → steve, research/competitor/trends → vision, news/brief/morning → friday, content/post/caption → veronica, security/monitor → ultron, market/trade → gecko, workout/nutrition → hercules, meeting → herald, music/play → jerome, remind/schedule → pepper, workflow → oracle, greek/rush/chapter → athena, business/metrics → stark; anticipation cues "heads up / anticipate / proactive / watch for / keep an eye / alert me / what's coming / what should I know" → **edith**, checked before the broad chain and deliberately NOT stealing friday's brief/morning/news). Anything unmatched → **darwin** (the orchestrator is the catch-all). When the cloud is unreachable and the turn is not cloud-bound, a conversational turn routes to **hulk** (offline survival); concrete local actions still reach their owners. The selected agent is announced as `agent.active{name, role, hue}` so the HUD highlights it and lerps the core to its hue, then runs the **existing** converse/cloud pipeline — but with its own persona name (the per-agent `converse` override above), its own Kokoro voice, recall scoped to its namespace, and its tool allowlist enforced.

**Tool isolation (`router.rs::enforce_tool`).** On the local-action path the intent *is* the tool name. If the selected agent's allowlist does not include it, the turn is handed to the tool's real owner (`owner_of`, never the orchestrator who holds everything) and the new agent is re-announced — no agent ever acts through another agent's exclusive tool. (The cloud tool-loop still offers its full bounded tool set; the cloud agent is almost always darwin (wildcard). Threading per-agent allowlists into `complete_with_tools` is a documented follow-up.)

**Memory namespacing (`memory.rs::agent_scoped_facts`).** Facts an agent records are tagged `agent.<name>.*`; recall feeds the active agent **its own namespace plus shared (non-`agent.*`) facts only** — another agent's private namespace is never visible, and `meta.*` bookkeeping stays filtered on every path. `route.completed`/`intent.handled` telemetry carry the acting agent + namespace.

**Roll-call (`router.rs::roll_call`, the reel centerpiece).** Utterances like "roll call" / "introduce the team" / "assemble" are detected (`agents::is_roll_call`) **before** routing. The handler walks the roster in declaration order (darwin first), and for each agent emits `agent.active` (so the HUD cycles the highlight and the core color in turn) and speaks that agent's **one-line `INTRO:`** — read from the first `INTRO:` line of its persona file, with a grounded name+role fallback — in **its own Kokoro voice**, via sequential `speak` ops on one shared reply sink. It is interruptible: a process-wide cancel flag is checked before each agent and the loop yields, so a barge-in/shutdown stops it mid-team. Emits `rollcall.started` / `rollcall.interrupted` / `rollcall.completed`.

## Actions & tool loop

`daemon/src/actions.rs` is what lets DARWIN actually DO things. The same actuator set backs both routes: the local router wires `app.launch`/`app.control`/`web.open`/`web.search`/`file.op` intents straight to it, and the cloud tool loop mirrors it 1:1 as Messages-API tool definitions (plus two memory tools), so heavy or ambiguous requests can act too.

| tool / fn | does | returns |
|---|---|---|
| `open_app(name)` | fuzzy-matches `name` against `*.app` stems under `/Applications` + `/System/Applications` (listing cached 60 s; tiers **exact > prefix > substring > token-subset**, the last of which digs the app out of a whole utterance), then `open -a <resolved>` | "Opened <App>." / candidate list when > 1 close match / not-found |
| `quit_app(name)` | same fuzzy resolution, then an AppleScript that sends the quit event **only if the app is running** (`is running` does not launch the target; the name is escaped for the script literal) — a quit request can never launch anything | "Quit <App>." / "<App> is not running." / candidate list / not-found |
| `open_url(url, browser?)` | normalizes the URL (see the URL safety rules below), resolves `browser` when named (see browser resolution), then `open -a <Browser> <url>` / `open <url>` | "Opened <url> in <Browser/the default browser>." (+ a fallback note when the named browser could not be honored) |
| `search_url(query, browser?)` (`web_search` as a cloud tool) | percent-encodes the query (RFC 3986 unreserved bytes pass; everything else, UTF-8 included, becomes `%XX`) into `https://www.google.com/search?q=<enc>` and opens it through `open_url` — same browser resolution, same safety gate | "Searched the web for '<query>'. <open_url outcome>" |
| `search_files(query, limit ≤ 8)` | `mdfind -onlyin $HOME`, filename-contains first (`kMDItemFSName == "*<term>*"c`), plain content match as fallback; query sanitized (quotes/backslashes/asterisks stripped) | hits newest-first with kind ("PDF file", "folder", ...) |
| `open_path(path)` | `open <path>` only if the path exists and canonicalizes to under `$HOME` or `/Applications` (symlink escapes don't slip through) | "Opened <path>." / refusal |
| `set_volume(percent)` | `osascript -e "set volume output volume N"`, clamped 0–100 | confirmation |
| `system_status()` | the cached SystemSnapshot formatted as the status data string | stats string |
| `remember_fact{key, value}` (cloud tool only) | `memory.upsert_fact` | confirmation |
| `recall_facts{}` (cloud tool only) | `memory.all_user_facts(20)` formatted | fact list |

All actions return human-readable outcome strings suitable as converse `data` — ambiguity and not-found are *outcomes to say out loud*, not errors. Every spawned command is bounded by a **5 s timeout with `kill_on_drop`**.

**URL safety rules (hard contract, `normalize_url`):** only http/https ever reaches `open`, on **every** path into it — router intent, cloud tool, and search alike. The normalizer trims; rejects any embedded whitespace; prepends `https://` when no scheme is present; **rejects every scheme other than http/https** (`file:`, `javascript:`, `data:`, `ftp:`, case-insensitive — a colon followed by a digit reads as a port, so `localhost:8080` is not a scheme); and requires the host to contain a dot or be `localhost`. Anything else is a refusal outcome, not a launch.

**Browser resolution (`resolve_browser_choice`):** a named browser goes through the same `open_app` fuzzy matcher and is honored only on a **unique, browser-ish** match (token-based check against known browser names — substring matching would let "arc" light up inside unrelated apps). Ambiguous, not-found, or a non-browser match all **fall back to the default browser** with a note in the outcome string — failing the whole request over a browser nit would be worse.

**Safety boundary (hard contract):** benign-only. No shell passthrough — commands are spawned with explicit args via `tokio::process::Command`, never an interpreted shell string. No delete/move/write of user files — actions open, search, and observe. No keystroke synthesis. Web actions obey the URL safety rules above. The cloud model gets exactly this surface and nothing more.

## Voice

Responses are spoken with Kokoro TTS (Kokoro-82M via the `mlx-audio` package), running on the Metal GPU like every other model, per `[speech]` (`engine = "kokoro"`, `voice = "bm_george"`, `speed = 1.2`).

### Instant acknowledgment (opener bank)

The half-second-response trick: DARWIN acknowledges **before it knows what was said**.

- **Server side:** at every startup (inside preload, after the TTS warm) the server clears `state/openers/` and synthesizes each `[speech].openers` line to `state/openers/opener-<idx>.wav` with the current engine/voice/speed, silence-trimmed like every other WAV (measured: 5/5 lines, 0.82–1.05 s each, 60 ms residual lead/tail). Filename index = config-list index; a line that fails to synthesize leaves a gap, never a shift, so the daemon's index→text mapping always matches config order. Regenerating on every start means a voice/engine change Just Works.
- **Daemon side (`speech.rs::ReplySession`):** on `Event::Utterance` — before transcription — the event loop opens the reply session, which sleeps the **opener breath** (`[speech].opener_delay_ms = 300`), then engages the `SpeakingGuard`, picks a uniformly random opener WAV (never repeating the previous pick), and appends it to the gapless sink. That append is `first_audio_ms` when it fires. The rest of the pipeline (STT → classify → route → converse) appends content to the **same** session/sink behind it. The opener's text rides to the server as `opener_spoken` (no-double-ack, above).
- **Opener breath:** the instant acknowledgment used to fire a touch too fast after end-of-utterance; the 300 ms delay lands it a natural beat after the user stops talking. The breath runs **concurrently with transcription** — `ReplySession::begin` and the STT request are joined in one `tokio::join!`, so the delay is never serialized in front of STT. `first_audio_ms` includes the breath naturally (the pipeline clock starts at pickup).
- **Suppression:** no opener when one fired within the last **6 s** (rapid follow-ups would get acknowledgement spam), or when `state/openers/` is missing/empty/unreadable (the feature then stays dormant — pre-opener behavior). If STT comes back empty or a later stage fails after an opener fired, the session is closed and the orphaned opener is accepted (`opener.orphaned` telemetry).
- **Cost:** the only work between VAD hand-off and opener audio is the event-channel hop, the 300 ms breath, a directory scan, one ~40 KB file read, and the sink append. With the 350 ms VAD silence window, first audio lands ≈ 660–750 ms after the user stops speaking — by design a beat later than the pre-breath ~360–450 ms (the old, breathless timing read as interruptive in live use; the pre-opener first-audio was ~2 s).

### Sentence pacing

`[speech].sentence_pause_ms = 250` of **pure silence** is inserted by the daemon between consecutive clips of a reply — after the opener before the first content sentence, and between content sentences; never after the last clip. The silence is generated in memory (a rodio `Zero` source sized in samples; no files) and appended to the same sink, so pacing stays gapless-pipeline-friendly. 120 ms-equivalent back-to-back playback read as rushed in live use; 250 ms restores a natural cadence.

**Silence trim + edge fades (server):** Kokoro bakes ~0.26 s of leading and ~0.46 s of trailing silence into every utterance. The server trims leading/trailing samples with amplitude < **0.005** from **every** synthesized WAV (`speak`, `converse` sentences, openers, audition samples), keeping 60 ms of padding each side — measured residual lead/tail are ~60 ms each (target ≤ 120 ms). This is what makes back-to-back sentence playback gapless. After the trim, every clip gets **linear edge fades** on the float array before int16 conversion: fade-in over the first 120 samples (5 ms @ 24 kHz) and fade-out over the last 240 samples (10 ms) — the trim's hard cut at the threshold used to leave a step at clip edges that played as a click where the daemon joins clips. The ramps pin the first and last samples of every emitted WAV to 0 (contract: |amp| < 0.002 at both edges; every WAV funnels through `_write_wav`, so nothing escapes the fade). Measured 2026-06-12: all 5 regenerated openers and sampled converse sentence WAVs have first/last samples exactly 0.

**Gapless playback (daemon, `playback.rs`):** rodio replaces per-sentence afplay. rodio's `OutputStream` is `!Send`, so a dedicated playback thread (spawned lazily, command channel held in a `static OnceLock`) owns one persistent `OutputStream` for the daemon's life. Per spoken reply the thread keeps one `Sink`; each WAV is read fully into memory (and **deleted from disk immediately** once the sink owns it) and appended via `rodio::Decoder` over a `Cursor` — sentences play back-to-back with no process spawns and no gaps. The drain wait is bounded by total-appended-duration + 10 s margin.

The two reply paths:

1. **Local LLM-voiced replies** — one `converse` request streams sentence WAVs; each is appended to the sink the moment its event arrives, so playback overlaps the remaining decode.
2. **Cloud replies** — `anthropic::complete_with_tools` returns text; the daemon clips it for speech, splits sentences, sends each through the `speak` op, and appends the WAVs to the same rodio sink (the next sentence synthesizes while the current one plays).

Both run inside the SPEAKING mute window (`SpeakingGuard`, RAII): engaged at opener fire (or before the first content append on an opener-less reply), held until the sink reports empty, plus a 400 ms tail (room echo outlives playback) — the capture loop drops mic chunks for the duration, so DARWIN never transcribes itself. The whole reply — opener, pacing silence, content clips — lives in one `ReplySession` owned from utterance receipt to drain.

Playback fallback ladder: any rodio failure degrades the **rest of that reply** to the old afplay path (duration-bounded, `kill_on_drop`); if no sentence produces audio at all, the daemon falls back to macOS `say`. TTS breaking never silences the pipeline.

### Persona

The response register is set by `inference/prompts/persona.txt`: the composed British butler-AI of the Iron Man films. Addresses the user as "sir"; dry understatement and quiet wit; anticipatory and effortlessly competent; concise (1–3 sentences unless asked for depth); never servile filler, never exclamation marks, never "As an AI". The persona file also instructs the model to vary phrasing naturally between similar requests and to reference known facts and recent conversation when relevant.

**Greetings are unpinned.** The greeting guidance in `persona.txt` (and the orchestrator persona `inference/personas/darwin.txt`) used to pin a bare hello to a minimal fixed reply — "a simple Hello, sir. is plenty" / "leave it at that" — which peaked the distribution so hard that Opus returned byte-identical greetings. It now instructs DARWIN to greet **warmly and differently each time** — a brief but characterful welcome that varies its phrasing, may note the user is back or ask how they are, with occasional dry wit — and supplies several **distinct example greetings** so the model sees the range. The grounding rules are untouched: a greeting still invents no weather, time-of-day, schedule, surroundings, or status. This pairs with the per-call anti-repeat hint (see *Conversation routes to cloud Opus by default* in Routing policy): since Opus 4.8 accepts no temperature, the unpinned persona plus the avoid-list are the only levers that force greeting variation.

This file is the **single source of truth**: the inference server prepends it as the KV-cached system prefix on every `generate`, and the daemon reads the same file at startup for cloud prompts (see Routing policy). There are no fixed response strings anywhere in the daemon — every reply, local or cloud, is phrased by an LLM in this register.

### Voice id

Default voice is `bm_george` — Kokoro voice ids starting `bm_` are British male, matching the persona. Sample WAVs for auditioning live at `state/voice-samples/<voice>.wav` (`bm_george`, `bm_fable`, `bm_daniel`, `bm_lewis`, `am_michael`), all reading the same line: *"Good evening, sir. All systems are running at full capacity. Shall I begin the diagnostic?"* They are generated server-side and never auto-played; audition with `afplay state/voice-samples/<voice>.wav`, then set `[speech].voice`.

### TTS engine evaluation (gated)

Candidates for a more human voice dispatch through one synth function per engine (`_synth_kokoro` / `_synth_csm` / `_synth_orpheus` in `server.py`); `speak`, `converse` and the opener bank all share that dispatch, selected by `[speech].engine` with `[speech].model` as the repo override. **Adoption gate** (all must hold on the target machine): warm RTF (= synth_seconds / audio_seconds) ≤ 0.5, clean load, output survives the silence-trim path.

Measured 2026-06-12 on the M1 Pro 16 GB dev machine (mlx-audio 0.4.4, warm, GPU otherwise idle, shared audition line; samples at `state/voice-samples/<engine>-<voice>.wav`, re-verified independently):

| engine / voice | warm RTF (best of 2) | gate |
|---|---|---|
| `kokoro` / `bm_george` | 0.067–0.069 | **PASS** — stays default |
| `csm` (`mlx-community/csm-1b-8bit`) / `conversational_b` | 0.894–0.931 | FAIL (loads clean, trim OK, but slower than the 0.5 gate) |
| `orpheus` (`mlx-community/orpheus-3b-0.1-ft-4bit`) / `leo` | 1.28 | FAIL (slower than realtime) |

Both candidates are LLM-class TTS: in real use they would also share the GPU with the resident Qwen3-4B converse decode and add ~1.7–1.9 GB each on a 16 GB machine, so the idle-GPU numbers above flatter them. Kokoro remains the default; the dispatch and config hooks stay in place for a re-run on the M4 target. CSM's `conversational_b` speaker prompt is fetched from the ungated `mlx-community/csm-1b` mirror (the upstream `sesame/csm-1b` prompts are HF-auth-gated).

### Latency levers

- **Instant opener (the biggest one):** first audio is a canned acknowledgement appended at utterance receipt plus the 300 ms breath — ≈ 660–750 ms after the user stops speaking, before STT even finishes (the breath runs concurrently with it). Everything below now only has to beat the opener's ~0.8–1.05 s of playback plus the 250 ms pacing pause to feel seamless.
- Streamed `converse`: the first content sentence waits only for first-sentence decode + synth (measured ~460-480 ms warm for a casual reply on the M1 Pro dev machine; data-heavy replies run ~2 s when the model front-loads the numbers into a long first sentence).
- SystemSnapshot cache: `system.query` answers from telemetry's 2 s cache — the ~250 ms double-refresh sysinfo sleep left the request path.
- Silence trim: ~0.26 s baked lead / ~0.46 s baked tail removed from every WAV — first audible speech starts ~60 ms into playback, and sentence boundaries carry no dead air.
- Gapless rodio sink: no afplay process spawn (~150 ms) per sentence, no inter-sentence gaps.
- `[inference] preload = true` — models load at server start and stay resident; no first-utterance stall.
- Classifier: trimmed prompt with the prefix KV-cached across calls, `max_tokens = 160` (raised from 80 with the args contract — web/memory outputs echo utterance content inside `args`, and a JSON skeleton plus ~38 words already hit 80 tokens; greedy decode stops at EOS, so the higher cap only costs on rare long outputs; ~860-1180 ms warm on the 4B). `[models].classifier` exists for a faster dedicated model but every small candidate has so far failed the accuracy gate.
- VAD timing: `silence_ms = 350`, `min_speech_ms = 250` — end-of-utterance is detected 350 ms sooner than the original 700/300.
- Terse persona: shorter generations mean fewer decode tokens and shorter TTS clips.
- `[models].stt` is config-driven; run the smallest Whisper model that holds transcription quality on the target machine.

Real per-utterance numbers are emitted via the `pipeline.completed` telemetry event — read those, not the targets. `first_audio_ms` is the number the user feels.

## Telemetry

WebSocket server **hosted by the Rust daemon** on `127.0.0.1:7177` (`[telemetry].port`). It broadcasts JSON events to all connected clients (the Phase-2 HUD):

```json
{"ts": "<iso8601>", "source": "audio"|"local"|"cloud"|"system", "event": str, "data": object}
```

Examples: `{"source": "audio", "event": "utterance.captured", "data": {"path": "state/tmp/utterance-1.wav"}}`, `{"source": "local", "event": "intent.classified", "data": {"intent": "system.query", "confidence": 0.91, "complexity": "light"}}`, `{"source": "cloud", "event": "route.completed", "data": {"routed_to": "cloud", "response": "..."}}`. The daemon never blocks on slow clients; events are fire-and-forget per connection.

One event per utterance summarizes the whole pipeline: `{"source": "system", "event": "pipeline.completed", "data": {"stt_ms": int, "classify_ms": int, "route_ms": int, "first_audio_ms": int|null, "speak_ms": int, "total_ms": int}}` — emitted after speech playback finishes and also logged at info level (the log line includes `first_audio=<x>ms`). This is the ground truth for latency work. Field semantics:

- `first_audio_ms` — utterance pickup → first audio: **the instant-opener append when one fired**, else the first content clip (first sink append / afplay spawn): the latency the user actually perceives. `null` when nothing became audible.
- `route_ms` — routing **and** reply generation: for the streamed converse path it runs to the server's `done` event (so it overlaps playback); for cloud it is time inside `router::route`.
- `speak_ms` — first audio → playback drained, excluding the 400 ms mute tail.

Opener events: `{"source": "local", "event": "opener.played", "data": {"index": int, "text": str|null}}` when an instant opener fires, and `{"source": "local", "event": "opener.orphaned", "data": {"reason": str, "text": str|null}}` when the pipeline died after the opener played (empty STT, failed classify/route) — the orphaned acknowledgement is accepted by design. The 2 s `system.load` event now also carries `disk_free_bytes` and `uptime_secs` (the same reading feeds the SystemSnapshot cache).

**Live mic level for the HUD waveform:** `{"source": "audio", "event": "audio.level", "data": {"rms": float, "speaking": bool}}` from the capture loop — `rms` is the RMS over everything accumulated since the last emission, rounded to 4 decimal places; `speaking` is the daemon's `is_speaking()` (true while DARWIN itself is talking and the mic is gated). Rate-limited by `Instant` to **at most one event per 66 ms** (≈15/s) no matter the device's chunk cadence, and emitted even during the speaking mute so the HUD waveform stays alive.

`daemon.started` carries `{"root": str, "cloud_key_present": bool}` — the API key is resolved eagerly at startup (env var, then Keychain; see the cloud section) and only the presence bool ever appears on the feed.

The learning loop emits `{"source": "system", "event": "memory.learned", "data": {"key": str, "value": str}}` for every fact extracted and upserted after an exchange (see Learning loop below); the HUD renders these on its memory ticker.

**Micro-app events** (all `source: "system"`, relayed by the app host — see Micro-app runtime below): `app.started{name}` / `app.stopped{name}` (lifecycle), `app.crashed{name, restarts}` (gave up after the restart budget), `app.data{name, topic, payload}` (an app's accepted `items`/`status` line — `payload` is the app's `data` object; `topic` is one the app *declared*), `app.log{name, line}` (app stdout/stderr or explicit log line), and `app.auth_failed{name}` (an inbound line failed token verification and was dropped). The HUD's pure reducer (`hud/src/core/state.ts`) folds `app.started`/`app.stopped` into a `runningApps` set and `app.data` into a per-app `appFeeds` slice (items hard-capped); the Global-Scan panel renders from that slice.

## Self-heal v2 (ships ON; PROPOSE-ONLY; inert without a cloud key; battle-tested via the heal drill)

Config: `[self_heal] enabled = true` (master gate; ships ON, PROPOSE-ONLY, inert without a cloud key), `mode = "propose"` (`"propose"` | `"auto"`; unknown values behave as `"propose"`; KEEP `"propose"`). `"auto"` additionally requires `enabled = true` and is **dangerous by design** — see step 8b. v2 keeps the same two config keys (lockstep `config.rs` ↔ `darwin.toml`); it adds no new ones.

**Trigger** (`heal.rs` watchdog, every 10s over the `state/logs/daemon.log` tail): an **edge-triggered ERROR burst** — ≥ 5 ERROR-*level* lines (level token match, never message text) inside a 60s window — OR a single **total-loss line** (`audio capture stopped`: the capture thread exits once and is never respawned, so no burst can ever follow it). The recurring hard-failure paths (transcription/classify failures, routing failure, cloud completion failure, converse/generate outage) log at `error!` precisely so this detector can see a real outage; the watchdog's own output is WARN/INFO and cannot feed back into its trigger. With `enabled = false` a trigger only emits `heal.suppressed`.

When enabled, the v2 pipeline runs (one attempt per episode, and at most **one draft attempt per 6h** via `meta.heal_last_attempt`; a rate-limited or key-less trigger emits `heal.blocked{reason}`):

1. **Root-cause diagnosis** (pure) — extract the stable error signature(s) (volatile `error=…` tails trimmed so a recurring fault collapses to one), the cited `src/<file>.rs` paths + line numbers, the implicated subsystem (from the `darwin_core::<sub>` module-path target), and a window of log context. The **current contents of each cited source file** are then read from the crate (confined to `<crate>/src`) and attached, so a drafted diff can match the real lines and apply cleanly. Emits `heal.diagnosing{signature, files, subsystem}`.
2. **Multi-candidate draft** — the heavy model (`[cloud].heavy_model` = Opus) is asked for **N=3 ALTERNATIVE minimal unified diffs** (distinct approaches, named files only, no new dependencies, `a/`–`b/` paths). Requires the cloud key (else `heal.blocked{reason:"no_api_key"}`). Each candidate is split on `=== CANDIDATE i ===` markers and cleaned (fences/prose stripped); a block that is not a valid diff is dropped. The cloud is reached **only** through the `HealBrain` trait seam (`CloudBrain` in production; a mock in unit tests).
3. **Stage + validate EACH candidate independently** — every surviving candidate is copied into its **own** `state/heal/staging-<ts>-c<i>/` (`src/`, `Cargo.toml`, `Cargo.lock` — **never** `target/`), the diff applied with `/usr/bin/patch -p1 --batch` (any failed hunk discards it), then `cargo check && cargo test` under one 10-minute deadline. A candidate that fails the patch, the check, or the test is discarded. **These gates are byte-for-byte the v1 gates — never weakened.**
4. **Adversarial self-review** — for each *validated* survivor, a second Opus call judges whether the patch fixes the **root cause** (not just silences the symptom) and has obvious side effects, returning a verdict + confidence `0..1` (garbled → 0.0, clamped).
5. **Select** — among PASSED candidates, the highest review confidence wins; ties break toward the **minimal** patch (smallest add/remove count).
6. Outcomes:
   - **8a `mode = "propose"`** (default): write `state/heal/proposals/<ts>/{patch.diff, report.md, diagnosis.json, candidates.md, review.md}` (report = diagnosis, chosen diff, validation-output tail, review verdict + confidence, and the exact apply command), stamp `meta.heal_pending=<ts>`, emit `heal.proposal{ts, files, validated:true, confidence}`. The proactive first-contact brief reads `meta.heal_pending`, so DARWIN *tells* the user a fix is waiting. A human applies it one of two ways, both **human-gated** and both running the **same re-validation gate**:
     - **Terminal:** `scripts/apply_heal.sh <ts>` — shows the report + diff, asks for confirmation (`read -r`), RE-VALIDATES (fresh staging copy of `daemon/` sources + `/usr/bin/patch -p1 --batch` + `cargo check` + `cargo test`), and **only on green** applies to `daemon/`, rebuilds `--release`, clears `meta.heal_pending`, then `launchctl kickstart -k`s the daemon if its service is loaded (else prints "restart darwind manually").
     - **GUI (HUD Accept button):** `scripts/apply_heal.sh <ts> --yes`, spawned by the HUD-Tauri backend (`heal_apply` command). `--yes` skips **only** the `read -r` keystroke — the GUI's two-step confirm replaces it; every gate above still runs and a failure exits non-zero without touching `daemon/src`. The script emits structured `STAGE:`/`RESULT:` lines so the HUD can show the stages. The `<ts>` is validated **digits-only** in both the command and the script (no path traversal). This is NOT auto-heal: a human reviewed the diff and confirmed twice, and the patch is re-validated before it touches the live tree.
   - **8b `mode = "auto"`** (UNCHANGED from v1; no new live-auto-apply path): apply the same validated diff to the real `daemon/` via `patch -p1`, `cargo build --release`, emit `heal.applied{ts}`, then **exit the daemon cleanly** — under launchd `KeepAlive` that is a restart into the new binary; under `cargo run` it is a stop. An audit copy lands in `state/heal/applied/<ts>/`.
   - If no candidate passes every gate → `state/heal/rejected/<ts>/` + `heal.rejected{ts, stage}` with `stage ∈ {draft, patch, check, test, apply, build}`.

The gates exist because a model-authored patch applied to a privileged daemon is a real risk: nothing is drafted while disabled, nothing touches the live tree in propose mode (the propose path writes artifacts only), `"auto"` is opt-in twice (enabled **and** mode), every patch must survive real `patch`+`cargo check`+`cargo test` in staging first, and the cadence is capped at one cloud draft per 6h. The HUD's **SELF-REPAIR // PROPOSALS** panel (warn-amber attention state, segmented confidence gauge) is a **human-gated apply surface**, not auto-heal: it fetches and renders the **actual staged diff** for review (`heal_proposal_detail`), and its **ACCEPT & APPLY** button is **two-step** — the first click arms a distinct "CONFIRM — APPLY & REBUILD" state, and only the second click (after a short re-arm window so a double-click cannot skip the confirm) calls `heal_apply`, which runs the gated `scripts/apply_heal.sh <ts> --yes`. The re-validation gate (fresh staging copy + `cargo check` + full `cargo test`) is **mandatory and non-bypassable** — there is no flag that weakens it, and a validation failure refuses to touch `daemon/src` (the UI shows the failure in alert-red; the live code is untouched). The read-only `scripts/apply_heal.sh <ts>` command line is kept on the panel too (the terminal path remains). `self_heal` ships `enabled = true` but **PROPOSE-ONLY** (`mode = "propose"`, inert without a cloud key); this is a human-gated apply of an already-proposed patch, the **only** sanctioned change to the live tree besides the (doubly-opt-in, dangerous) `mode = "auto"`.

**Heal drill** (the one real-cloud verification path): `darwind --heal-drill` (and the `#[ignore]`d `heal::tests::heal_drill_real_cloud`) runs the **full** pipeline — diagnose → real Opus draft → stage → validate → adversarial review → propose — against a **planted compile fault in a throwaway temp crate** (never `daemon/`). It requires the key, writes a real proposal artifact, and asserts the planted source was untouched. The pure staging path is additionally unit-tested end to end against a synthetic crate with a planted compile bug (real `/usr/bin/patch`, real `cargo` in a tempdir — never the live `daemon/`); unit tests mock `HealBrain`, so `cargo test` makes no cloud call.

## Optimization-from-usage loop (ships ON; propose-only; live recording runtime-gated + PII-redacted)

Config: `[optimize] enabled = true` (master gate), `mode = "propose"` (`"propose"` | `"auto"`; `"auto"` is reserved and behaves as `"propose"` — there is **no** auto-apply-to-live path). Same propose-only contract as `[self_heal]`/`[forge]` (mode KEEPS `"propose"`), and the same lockstep `config.rs` ↔ `darwin.toml` keys. Live trace recording accrues only while the daemon runs with this ON, and is PII-redacted before storage.

**What it tunes — and what it does not.** This loop tunes **only agent routing/selection**: it learns a thin **cue-weight layer** over the per-agent keyword vocabulary in `agents.rs` `CUE_VOCAB` (each shipped cue sits at weight 1.0; a candidate nudges a few weights OR adds one learned cue word drawn from the existing agent vocabulary). It does **not** rewrite prompts, touch model weights, or tune any unrelated signal. This is a **bounded, measured, reversible** optimization-from-usage loop — **not** self-improving AGI.

**Trace Store** (`optimize.rs`: `TraceStore` / `Trace` / `Outcome` / `record_trace`) is the local record the loop learns from: per interaction, the (redacted) utterance, the agent that was picked, and the outcome (did the user correct/redirect on the next turn, or complete cleanly?). Every trace is **PII-redacted before storage** by `redact()` — emails, phone numbers, ≥6-digit runs, URLs with embedded credentials, and api-key-/token-shaped strings are stripped to `[redacted]`, and a trace **never** stores a secret or token. Retention is bounded (oldest rows evicted past the cap). With `enabled = false` the recorder is a **NO-OP** — nothing is stored and no corpus accrues. (Known scrubber limits — a borderline pure-alpha passphrase or a space-grouped PAN can survive — are documented at `redact()`; the scrubber is a bounded hand-scanned pass, not a guarantee, and the learned-cue miner additionally excludes any `[redacted]` span from ever becoming a cue.)

**Optimizer** (`run_optimizer`): split the usable traces **chronologically** into train + HELD-OUT (`split_usable`), generate a bounded candidate set off the train slice (`generate_candidates`, capped at `MAX_CANDIDATES = 24` — each candidate is the shipped baseline plus a small cue-weight nudge or one learned cue), score each against the held-out slice, and adopt the best **ONLY IF** it passes two gates: **GATE 1** — it beats the current baseline's held-out routing accuracy by `ADOPTION_MARGIN = 0.05`; **GATE 2** — it is **not worse on the success class** (no per-class regression). If nothing clears both gates, `optimize()` returns `None` and nothing is proposed. Because adoption is judged on held-out traces the candidate never trained on, an over-fit tuning can't be adopted — it **can't make DARWIN worse**.

**Propose-only, human-gated apply.** On a passing candidate the loop writes a reviewable proposal (`proposal.md` + `proposal.json`: the cue-weight/cue diff, the measured before/after held-out accuracy, the adoption margin, and provenance) and **STOPS** — it **never** silently mutates a live config or behavior. A human reviews and applies via `scripts/apply_optimization.sh <ts>`, which records the operator decision; adopting the change is a **reviewed, reversible source edit** to `agents.rs` `CUE_VOCAB` (and rebuild). Every adoption is reversible.

**Wired vs runtime-gated — honest status.** The Trace Store + optimizer are a fully-implemented, hermetically unit-tested **API** (the enabled/disabled recorder gate, the store round-trip + bounded-retention eviction, and the held-out scoring + both adoption gates all have known-answer tests using **mock traces**) **and are now wired into the live daemon pipeline**: `record_trace` is called from the per-turn bookkeeping path in `main.rs` — gated by `[optimize].enabled` (a NO-OP when OFF) **and** skipped for transient screen-read turns, consistent with the transcript/episodic/learning loop — and the cross-turn correction labeller (`optimize::is_correction`) re-labels the prior turn's trace `CorrectedNextTurn` only on an explicit redirect of the same intent. The held `TraceStore` is opened once at startup (`main.rs`), and `run_optimizer` runs in a **gated periodic background task** (`optimize_task`, spawned in `main.rs`) that short-circuits to `Disabled` when OFF and otherwise only writes a reviewable proposal — it **never** mutates the live routing config. The `#[allow(dead_code)]` on `mod optimize;` is **removed** now that the module is reachable. Crucially, live wiring only makes the machinery **reachable** — it does **not** auto-tune routing: the loop stays **propose-only** (mode KEEPS `"propose"`; there is no auto-apply-to-live path), and a passing candidate is only ever written as a reviewable proposal a human applies. The **actual learning is runtime-gated**: a real corpus accrues only while the daemon runs with `[optimize] enabled = true` (the shipped default), PII-redacted before storage; tests never accrue real traces, they feed mock/synthetic traces and assert the recorder gate, correction labelling, and proposal generation behave correctly.

## World Model, Standing Missions & the Capability Selector

These three subsystems let the user **talk naturally** — DARWIN decides what kind of request it is, answers or acts, and keeps a standing goal going (the subsystem ships ON; establishing one is still confirmation-gated) — while two non-negotiable safety rails keep autonomy human-gated.

**World Model** (`daemon/src/world_model.rs`) is a **shared** memory tier — entities, attributes, and relationships under the `user.world.*` key prefix — that **every agent can read** so they reason over the same ground truth ("what's the user working on", "what's blocked"). It is deliberately the only cross-agent-readable tier. Reads go exclusively through `recall_facts_limited(WORLD_PREFIX = "user.world.")`; `structure_rows` drops any foreign/malformed row, so private notes never surface. Writes are built only by `entity_key`/`rel_key` from pre-slugged components (`slugify`/`slug_attr` collapse every non-alphanumeric — including `.` — to `_`) under a **closed** entity-type enum, and route through `upsert_user_fact`, which rejects reserved `meta.*` keys. **Isolation invariant:** the per-agent **private** tier `agent.<ns>.*` is structurally excluded from the world model, from `agent_scoped_facts`, and from the world-context prompt injection — a private note can never be folded into the shared model or leak to another agent. Bounds/DoS caps: `MAX_ENTITIES = 512`, `MAX_RELATIONS = 1024` (new entities refused at cap; in-place updates always succeed), per-field length clamps, and a bounded read window. **Wired:** the world model is live — it's queried/updated through the cloud tool loop and injected as grounding context for missions (`grounded_world_live`) and sub-tasks (`mission.rs`).

**The Capability Selector** (`daemon/src/selector.rs`, dispatched by `router.rs::route`) is the front door: from one natural utterance it classifies the **mode** — world-query, world-update, mission (one-off multi-step), standing (recurring), or plain one-shot answer. `classify_mode` runs **deterministic cues first**; only a hard recurring cue reaches `Mode::Standing`. The **semantic fallback** is rail-protected (see Rail 1): a winner that is consequential (Standing) never routes — it returns `Clarify`; a Mission winner downgrades to one-shot unless a deterministic cue agrees; below-floor / near-tie / no-signal all stay one-shot; and explicit one-shot scoping ("just once", "for now") vetoes a recurring cue. **Wired:** the selector is live on every turn.

**Standing Missions** (`daemon/src/standing.rs`) are durable saved goal + schedule pairs that run on the **standing-missions scheduler tick** — a **dedicated runtime loop** (`standing_task` in `main.rs`, its own `STANDING_INTERVAL = 300s` cadence), **distinct from EDITH's anticipation tick**. A due mission runs through FURY's bounded mission engine (`mission.rs`): decompose → dispatch each sub-task to its owning specialist under that specialist's per-agent allowlist → synthesize, grounded on the shared World Model. **Runtime-gated, ships ON:** `[standing].enabled = true` is the subsystem master switch; while false the pure scheduler (`due_missions`) marks **nothing** due, so no standing mission fires on the live tick. Even ON, establishing a mission is confirmation-gated and every consequential step a run proposes still parks (it can never auto-send/post/spend). Bounds: at most 8 active missions, goal length and minimum interval (≥ 1h) clamped at due-check time so a hand-edited record can't hammer the tick. Standing records persist under `meta.standing.*`, excluded from every prompt feed, from `agent_scoped_facts`, and from the world model.

**The two rails (non-negotiable):**
1. **Clarify / safe-default when unsure.** The selector NEVER silently picks a mode that establishes autonomy or fires a consequential action from a low-confidence guess. The default-safe outcome is a plain one-shot answer or a single one-line clarifying question.
2. **No silent autonomy.** ESTABLISHING a standing mission is itself a **confirmed action** behind the cross-turn confirmation gate (`standing_create` is in `confirm::CONSEQUENTIAL_TOOLS`; both the cloud-tool path and the selector path require a spoken yes, and create nothing on a preview), and every **consequential step a mission RUNS** (post/send/spend/control) still parks behind the same confirmation gate **and** the armed-by-default `[integrations]` master switch (ON, but a confirmed action still needs a fresh per-action confirm) — exactly as a direct call would. A standing mission can therefore read and reason autonomously, but every outward action waits for a human yes.

**User experience:** the user just speaks. "What am I working on?" → world-query (reads the shared model). "Track that the Q3 launch is blocked on legal" → world-update. "Draft the release notes and open a PR" → a one-off mission. "Every morning, review my deadlines and flag anything slipping" → DARWIN recognizes a recurring intent and, with standing OFF, **previews** it; with standing ON, **asks for confirmation** before establishing — and when that mission later runs, any step that would post/send/spend pauses for a yes. When intent is ambiguous, DARWIN asks one short question rather than guessing.

## Micro-app runtime (the app host)

`daemon/src/apps.rs` is the host side of `docs/SANDBOX.md` — the substrate that runs micro-apps as **separate, seatbelt-sandboxed processes**, never inside the daemon's address space. It is wired into `main.rs` (registry discovery at startup, optional `[apps] autostart`) and the router (voice "open/close <app>" resolves against the app registry **before** the macOS app launcher). The first app on it is **Global-Scan** (`apps/global-scan/`, Python over the project venv).

Launch sequence per app:

1. **Manifest** — `apps/<name>/manifest.toml` parsed into a typed `AppManifest` (serde + toml, `deny_unknown_fields` so a typo'd permission can't silently change the sandbox; `[app].name` must equal the directory name — it keys the socket and the token).
2. **SBPL profile** — `generate_sbpl` derives a `sandbox-exec` (seatbelt) profile to `state/apps/<name>/<name>.sb`: **`(deny default)`** + the stock `bsd.sb` import, then *only* the manifest's grants — exec of the (canonicalized) interpreter, read of the app dir + runtime install prefix + `fs_read`, write of `fs_write` + the app's own socket, and network *only* to `net_hosts` (deny-then-allow-list) when non-empty. `sandbox-exec` is the deprecated-but-functional CLI over live kernel seatbelt; the derivation is the durable part. Honest limits (coarse host filtering, DNS side channel, same-UID socket trust, and the unscoped inference socket) are catalogued in `docs/SANDBOX.md`.
3. **Capability token** — `HMAC-SHA256(session_key, name ‖ canonical(permissions) ‖ nonce)`. The session key is per-boot OS entropy in a `OnceLock` — never logged, never on telemetry, never in an app's env; only the derived token reaches the child (`DARWIN_APP_TOKEN`, with `DARWIN_APP_SOCKET`). The nonce rotates per launch. Every inbound socket line is verified constant-time; a bad/missing/forged/stale/cross-app token drops the line and emits `app.auth_failed`.
4. **Spawn + supervise** — `/usr/bin/sandbox-exec -f <profile> <interp> <entry>` with `kill_on_drop`, stdout/stderr relayed as `app.log`. Health = socket connected. Crash → restart, bounded **≤3 / 5 min** (`RestartGovernor`), then give up with `app.crashed`. `stop()` wakes the supervisor, reaps the child, removes the socket, and clears the token.
5. **Relay** — accepted `items`/`status` lines become `app.data{name,topic,payload}` on the `7177` WS (apps cannot publish to an undeclared topic); the HUD panel renders purely from that relay and never opens its own socket.

The pure, unit-tested units are manifest parse, SBPL generation (default-deny + exact allows + no stray grants asserted), token mint/verify (forgery/tamper/cross-app/stale all rejected), restart-rate math, and inbound-line classification. One hermetic macOS integration test exercises the real bind → token handshake → relay → stop round trip with a stand-in child (it skips cleanly where `sandbox-exec`/`bsd.sb` are absent).

## Learning loop

DARWIN learns from everything the user says. After speaking a response, the daemon spawns a **non-blocking** task (the pipeline never waits on learning):

1. `extract_facts(utterance, response)` over the inference socket — the local LLM distills at most 3 durable facts (see the op definition above). For converse replies, `response` is the full `done.text`.
2. For each fact: `memory.upsert_fact(key, value)` — update by key if present, else insert.
3. Telemetry: emit `("system", "memory.learned", {key, value})` and a tracing info line per fact.

The loop closes on the next exchange: every reply — local `converse`/`generate` or cloud — is assembled with `facts = memory.all_user_facts(12)` (most recent, `meta.` bookkeeping keys excluded) and `history = memory.recent_exchanges(6)`, so learned facts and conversational context inform everything DARWIN says. To feed `recent_exchanges`, the `transcripts` table stores what DARWIN replied (`response` column, below) alongside what the user said.

### Reflection (memory consolidation)

`daemon/src/reflect.rs` spawns a background task at startup that periodically asks the server's `consolidate` op to clean the facts table — merging duplicates, preferring the newest phrasing, deleting facts the user later contradicted, and mining clearly recurring user patterns into `user.habit.<slug>` facts (≥ 3 occurrences, user lines only — see the `consolidate` op above for the rules and the deterministic backstop). Schedule: wait **90 s** after daemon start (startup belongs to preloading, not housekeeping), then run iff the `meta.last_reflection` stamp (unix seconds, stored *as a fact* but excluded from every prompt feed — all prompt reads filter keys starting `meta.`) is absent, unparseable, or **> 20 h** old; re-check every **6 h**. A run loads the last 40 transcripts plus all user facts, calls `consolidate` on its **own `InferenceClient`** (never contends with a live reply), applies the upserts via `upsert_fact` and the deletes via `delete_fact` (refusing `meta.` keys in both directions), emits `("system", "memory.consolidated", {upserts, deletes})`, and stamps `meta.last_reflection`. Every failure path is warn-and-continue — the task never panics or wedges the daemon, and a failed round leaves the stamp untouched so the next 6 h check retries.

## Memory subsystem

SQLite database at `state/darwin.db`, owned by the Rust daemon (`daemon/src/memory.rs` opens it directly). `scripts/init_memory.py` creates the `state/` tree and the same schema idempotently; the inference server never touches the DB. Tables (keep `scripts/init_memory.py` and `daemon/src/memory.rs` in sync):

```sql
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY,
    ts          TEXT NOT NULL,            -- iso8601
    source      TEXT NOT NULL,            -- audio | local | cloud | system
    kind        TEXT NOT NULL,            -- event name, e.g. "utterance.captured"
    payload     TEXT                      -- JSON or text body
);

CREATE TABLE IF NOT EXISTS facts (
    id          INTEGER PRIMARY KEY,
    ts          TEXT NOT NULL,
    key         TEXT NOT NULL,            -- e.g. "user.preferred_units"
    value       TEXT NOT NULL,
    confidence  REAL DEFAULT 1.0
);

CREATE TABLE IF NOT EXISTS transcripts (
    id          INTEGER PRIMARY KEY,
    ts          TEXT NOT NULL,
    wav_path    TEXT,                     -- source utterance WAV under state/tmp/
    text        TEXT NOT NULL,            -- final STT text
    intent      TEXT,
    routed_to   TEXT,                     -- local | cloud
    response    TEXT                      -- what DARWIN replied (feeds recent_exchanges)
);
```

The `response` column arrived after first ship: `memory.rs` runs an idempotent migration at open (`ALTER TABLE transcripts ADD COLUMN response TEXT`, ignoring the duplicate-column error on already-migrated DBs), and `scripts/init_memory.py` includes the column for fresh installs.

`events` mirrors the telemetry stream for offline analysis; `facts` is the durable key-value memory written by the learning loop (and readable with `sqlite3 state/darwin.db 'SELECT key,value FROM facts ORDER BY id DESC;'`); `transcripts` is the exchange log feeding context windows and evaluation.

`memory.rs` API surface used by the response and learning flows:

- `upsert_fact(key, value)` — UPDATE by key if present, else INSERT.
- `delete_fact(key)` — DELETE by key; returns whether a row existed (the reflection loop logs already-gone keys as no-ops).
- `get_fact(key)` — exact-key read, used for internal bookkeeping like `meta.last_reflection`.
- `recent_exchanges(n)` — `Vec<(user_text, response)>` from `transcripts` where `response IS NOT NULL`, newest `n` by id, returned oldest-first (ready to use as `generate` history).
- `all_user_facts(limit)` — `Vec<(key, value)>`, most recent, **excluding** keys starting `meta.`; every prompt feed (generate, converse, cloud, the `recall_facts` tool) goes through this.
- `all_facts(limit)` — the unfiltered view, kept for tests and inspection.

## Process model

- `darwind`: LaunchAgent, owns the mic, the telemetry server, the router, the micro-app host (`apps.rs`, per `docs/SANDBOX.md` — Global-Scan ships on it), and the self-heal watchdog (gated; `KeepAlive` is what turns auto-mode's clean exit into a restart into the freshly built binary).
- Inference server: sibling process on python3.11 + MLX; restarted by launchd (KeepAlive) when deployed as a LaunchAgent. `darwind` only reconnects to the socket per request and keeps running while the server is down.
- Micro-apps: separate processes under `sandbox-exec` seatbelt profiles — see `docs/SANDBOX.md`.
- HUD: separate fullscreen app, a pure telemetry client — see `docs/ROADMAP.md` Phase 2.
