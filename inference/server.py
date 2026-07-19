#!/opt/homebrew/bin/python3.11
"""DARWIN inference server.

Asyncio Unix-domain-socket server speaking newline-delimited JSON at
state/ipc/inference.sock per the shared contract:

  Request:  {"id": str,
             "op": "transcribe"|"classify"|"generate"|"extract_facts"|"speak"
                   |"converse"|"consolidate"|"embed"|"rerank"|"clone_voice"
                   |"describe_image"|"generate_image",
             "path"?: str, "text"?: str, "max_tokens"?: int, "voice"?: str,
             op=speak may also carry the OPTIONAL ElevenLabs cloud voice tier:
               "backend"?: "elevenlabs", "voice_id"?: str, "model"?: str,
               "el_key"?: str (the xi-api-key, request-body only, NEVER logged),
               "lang"?: str (Babel target language; a NON-English lang selects an
               EL multilingual model — ignored on Kokoro / when the tier is OFF).
               Absent/"kokoro" -> on-device Kokoro. On any cloud error the server
               FALLS BACK to Kokoro (never fails the turn).
             op=transcribe may also carry the OPTIONAL gated cloud-STT tier:
               "backend"?: "elevenlabs_scribe", "model"?: "scribe_v1",
               "el_key"?: str (xi-api-key, request-body only, NEVER logged).
               Absent/"whisper" -> on-device mlx_whisper. On any cloud
               error/missing-key the server FALLS BACK to mlx_whisper (never
               fails the turn). HONESTY: the cloud path sends the user's VOICE
               AUDIO off the device — MORE sensitive than TTS text.
             op=clone_voice (CONSENT-GATED) carries {"path": owner sample wav,
               "text": display name, "el_key": xi-api-key}; the daemon only emits
               it after an explicit authorization-bound consent confirm. HONESTY:
               the audio SAMPLE leaves the device to ElevenLabs.
             op=describe_image (OPTIONAL on-device VLM, ships OFF) carries
               {"path": LOCAL image path (the daemon path-confines it first),
               "question"?: str (optional VQA; absent => a general scene
               description), "max_tokens"?: int}. On success responds {"id",
               "ok": true, "text": <description/answer>, "model": <vlm id>}; when
               the VLM is unavailable (mlx-vlm not installed or the checkpoint
               not downloaded) responds {"id", "ok": false, "reason":
               "vlm_unavailable", "error": <honest msg>} — NEVER a fabricated
               description, so the daemon falls back (OCR/classification).
               DISTINCT from OCR (OCR = text glyphs; VLM = visual understanding).
               HONESTY: the image is read LOCALLY and handed only to the
               on-device MLX VLM — the pixels NEVER leave the device.
             op=generate_image (OPTIONAL on-device text->image, ships OFF)
               carries {"prompt": str (the text to render), "size"?: int
               (square px), "steps"?: int (sampling steps), "seed"?: int}. On
               success responds {"id", "ok": true, "path": <abs path under
               state/images/>, "model": <image model id>, "size", "steps",
               "seed"}; when the diffusion model is unavailable (the diffusion
               package not installed or the checkpoint not downloaded) responds
               {"id", "ok": false, "reason": "image_model_unavailable", "error":
               <honest msg>} — NEVER a fabricated image and NEVER a cloud call,
               so the daemon surfaces an honest "the on-device image model isn't
               set up" message. HONESTY: image generation is 100% ON-DEVICE (MLX
               diffusion) — the prompt and the generated pixels stay on the
               machine and NEVER leave the device; there is NO cloud image API.
             "history"?: [{"speaker": "user"|"darwin", "text": str}, ...],
             "facts"?: [str], "data"?: str, "response"?: str,
             "opener_spoken"?: str, "texts"?: [str] (op=embed),
             "transcripts"?: [{"user": str, "darwin": str}, ...]}
             (op=consolidate reads "transcripts" (<=40, oldest first) and a
              differently-shaped "facts": [{"key": str, "value": str}, ...])
  op=clone_voice responds {"id", "ok": true, "voice_id": str} on a successful
    clone (the daemon stores voice_id, which is NON-secret), or {"id", "ok":
    false, "error": str} on a clean no-clone (the user keeps Kokoro / their
    existing voice). The el_key is NEVER echoed.

  Response: {"id": str, "ok": bool, "text"?: str, "intent"?: str,
             "confidence"?: float, "complexity"?: "light"|"heavy",
             "args"?: object (classify only: the model's extracted params,
             passed through verbatim; {} when absent or malformed),
             "path"?: str, "facts"?: [{"key": str, "value": str}],
             "upserts"?: [{"key": str, "value": str}], "deletes"?: [str],
             "vectors"?: [[float]] (op=embed: one L2-normalized vector per
             input text, in input order),
             "embedder"?: str + "dim"?: int + "fell_back"?: bool (op=embed WIRE
             CONTRACT: the ACTIVE backend's vector-space id — the fixed
             "coreml-bge-small-en-v1.5" (384-dim) for the Core ML path, or the
             MODEL-DERIVED "llm-meanpool:<llm_id>:<quant>[:<adapter>]" for the mean-pool path
             (so an LLM/quant swap changes the stamp) — and its vector dimension,
             compared by STRING EQUALITY so the daemon/docsearch key the vector
             SPACE and refuse cross-space comparison; fell_back is true iff the
             Core ML backend was configured but unavailable, so this is the honest
             mean-pool fallback),
             "scores"?: [float] + "reranker"?: str + "fell_back"?: bool (op=rerank
             WIRE CONTRACT — STAGE TWO of the two-stage retrieval stack: one
             cross-encoder relevance score per passage in INPUT order (higher = more
             relevant; the daemon re-orders its dense top-K by these), the reranker
             model id "coreml-ms-marco-minilm-l6-v2" that produced them (or "" when
             fell_back — no model scored), and fell_back true iff the reranker was
             configured but unavailable, so the scores are order-preserving and the
             daemon keeps its dense order),
             "model"?: str (op=describe_image: the VLM id used; op=generate_image:
             the image model id used),
             "size"?: int, "steps"?: int, "seed"?: int (op=generate_image: the
             resolved generation params),
             "reason"?: str (op=describe_image unavailable: "vlm_unavailable";
             op=generate_image unavailable: "image_model_unavailable"),
             "error"?: str, "latency_ms": int}

  op=converse alone responds with MULTIPLE JSON lines (same id, in order):
    {"id", "event": "sentence", "seq": <0-based int>, "text": str,
     "path": "<abs wav path under state/tmp/>"}     one per spoken sentence,
                                                    emitted as soon as that
                                                    sentence is synthesized
    {"id", "event": "done", "ok": true, "text": "<full reply>",
     "sentences": int, "first_sentence_ms": int, "latency_ms": int}
  On mid-stream failure the terminal line is
    {"id", "event": "done", "ok": false, "error": str, "latency_ms": int}
  and any sentence events already emitted remain valid.

Ops:
  transcribe     WAV path -> text. Default + fallback: mlx_whisper (on-device,
                 stdlib-wave decode, no ffmpeg). When backend=elevenlabs_scribe
                 with a key: POST the audio to ElevenLabs Scribe; on ANY
                 error/missing-key fall back to mlx_whisper (never fail). HONESTY:
                 the cloud path sends the user's voice audio off the device.
  clone_voice    CONSENT-GATED: POST an owner audio sample to ElevenLabs
                 /v1/voices/add and return the new voice_id; clean no-clone on
                 any failure (the user keeps Kokoro). The audio sample leaves the
                 device. NON-secret voice_id out; el_key never echoed.
  classify       utterance -> {intent, confidence, complexity, args}; args is
                 the model's extracted-params JSON object passed through
                 verbatim — {} when the model omitted it or emitted a
                 non-object, never fabricated server-side. Runs on the
                 dedicated small [models].classifier model when configured
                 (empty string reuses the main LLM); either way the static
                 classifier prefix is KV-prompt-cached so only the utterance
                 and suffix are prefilled per request
  generate       text (+ optional history oldest-first, facts, data) ->
                 persona-voiced reply. System role = prompts/persona.txt plus
                 a facts block; history becomes alternating user/assistant
                 turns; data is appended to the final user turn as verified
                 system data the model must convey without altering numbers.
                 The persona-only prefix is KV-prompt-cached like the
                 classifier prefix (facts/history/text prefill after it).
                 Default max_tokens 160, override honored.
  converse       streamed generate+TTS in one request: decodes the persona
                 reply (same prompt assembly, KV cache and sampler as
                 generate) and, at every completed decimal-aware sentence
                 boundary (. ! ? newline — matching the daemon's
                 split_sentences), pauses decoding, synthesizes that sentence
                 to a silence-trimmed WAV, emits a "sentence" event, and
                 resumes. At most 5 sentences are synthesized; later text is
                 returned in the done event only. Optional "opener_spoken"
                 (the opener line the daemon already played from the opener
                 bank) appends a bracketed continuation note to the final
                 user turn so the reply goes straight to the substance
                 instead of acknowledging twice. If the requesting peer
                 disconnects mid-decode, the loop notices (EOF on the
                 connection's read side, polled per chunk) and aborts —
                 no further tokens or sentences are produced for a client
                 that is gone (generate's cached path aborts the same way).
  extract_facts  user utterance (+ optional response) -> at most 3 durable
                 {key, value} facts with namespaced keys (user.name,
                 user.preference.<x>, user.project.<x>, context.<x>);
                 strict-JSON parse with empty-list fallback. Corrections are
                 honored: when the user contradicts an earlier fact
                 ("actually my name is..."), the corrected fact is emitted
                 under the same key with the new value.
  consolidate    self-learning reflection pass: recent transcripts (<=40,
                 oldest first) + the current facts table -> {"upserts":
                 [{key, value}], "deletes": [key]}. Greedy decode, strict
                 JSON with empty-arrays fallback; conservative by design
                 (merge duplicates, prefer the newest phrasing on conflict,
                 delete facts contradicted later, change nothing when
                 unsure). Also mines clearly recurring user patterns into
                 user.habit.<slug> facts — only when the SAME kind of user
                 request appears >=3 separate times in the provided
                 transcripts, counted from the user's lines only (never the
                 assistant's); habit facts are deletable like any other.
                 Deletes may only name keys present in the input
                 facts, upserts+deletes are capped at 12 total, and
                 "meta."-prefixed keys are ignored entirely.
  embed          [str] -> [[float]]: one L2-normalized embedding VECTOR per
                 input text, ON-DEVICE. Backend = [inference].embedder. DEFAULT
                 is a purpose-built Core ML sentence embedder (BAAI/bge-small-
                 en-v1.5, 384-dim, ANE-eligible; inference/coreml_embed.py) —
                 convert-on-first-use, cached under the HF model root, inputs
                 tokenized to coreml_embed.SEQ (512) tokens. HONEST FALLBACK to
                 the legacy path — mean-pool of the resident LLM's last hidden
                 states (inner decoder stack output; reuses the loaded generate
                 model, NO new download; each input capped at EMBED_MAX_TOKENS) —
                 when the Core ML embedder cannot build/load, reported on the
                 wire (fell_back). Batch capped at EMBED_MAX_BATCH; deterministic
                 (a forward pass, no sampling). The response also carries the
                 ACTIVE backend's vector-space id ("embedder") + "dim" so
                 docsearch keys the vector SPACE. MNEMOSYNE's
                 NeuralEmbeddingProvider uses these for cosine-similarity recall
                 ranking; when this server is down it falls back to lexical BM25.
  speak          text -> 24kHz WAV under state/tmp/ (TTS on MLX via
                 mlx-audio; the daemon owns playback and deletion). Synthesis
                 dispatches on [speech].engine — "kokoro" (default), "csm"
                 (Sesame) or "orpheus" — through one synth function per
                 engine; speak, converse and the opener bank all share that
                 dispatch. All synthesized WAVs have leading/trailing samples
                 below amplitude 0.005 trimmed, keeping 60ms padding each
                 side — Kokoro bakes ~0.26s lead / ~0.46s tail of silence
                 into every utterance otherwise — and then get linear edge
                 fades (5ms in / 10ms out) so every emitted WAV starts and
                 ends at near-zero amplitude: click-free clip joins.

Opener bank: at startup (inside preload, after the TTS warm) the server
clears state/openers/ and synthesizes every [speech].openers line to
state/openers/opener-<idx>.wav with the current engine/voice/speed,
silence-trimmed. The daemon plays a random opener the instant an utterance
ends and maps the filename index back to the configured openers list, so
indexes always match the config order (a failed line leaves a gap, never a
shift). Regenerating on every start means a voice/engine change Just Works.

Models load lazily on first use and stay resident; with [inference]
preload=true a background thread warms STT, LLM (+classifier and persona
prompt caches) and TTS at startup. MLX imports are deferred so this file
compiles and imports without the MLX venv (python3.11 required at runtime —
no mlx wheels on 3.14; MLX runs on the Apple GPU via Metal). main() refuses
to bind the socket when the MLX stack is missing (e.g. when run with the
bare Homebrew interpreter instead of .venv/bin/python).
"""

import asyncio
import itertools
import json
import logging
import math
import os
import re
import signal
import sys
import threading
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONFIG_PATH = PROJECT_ROOT / "config" / "darwin.toml"
SOCKET_PATH = PROJECT_ROOT / "state" / "ipc" / "inference.sock"
LOG_PATH = PROJECT_ROOT / "state" / "logs" / "inference.log"
TMP_DIR = PROJECT_ROOT / "state" / "tmp"
OPENERS_DIR = PROJECT_ROOT / "state" / "openers"
PROMPT_PATH = Path(__file__).resolve().parent / "prompts" / "intent_classifier.txt"
PERSONA_PATH = Path(__file__).resolve().parent / "prompts" / "persona.txt"
# Per-agent persona prefixes for the constellation. op=converse may carry a
# "persona" NAME (an agent id from config/agents.toml); the engine maps it to
# inference/personas/<name>.txt and uses that text as the system prefix for
# THAT call. Each distinct agent prefix gets its OWN KV prompt cache (keyed by
# agent name, LRU-bounded) — so an agent's persona-prefix KV is prefilled once
# and REUSED on that agent's later turns, instead of being recomputed from
# scratch every call. The base-persona cache (keyed to prompts/persona.txt)
# stays separate and untouched. Absent/None -> base persona + base cache.
PERSONAS_DIR = Path(__file__).resolve().parent / "personas"
# Agent name -> persona text, loaded lazily and cached. A valid agent id is a
# lowercase ASCII slug; the name is validated before it touches the filesystem
# so "persona" can never be used for path traversal.
import re as _re_personas
_AGENT_NAME_RE = _re_personas.compile(r"^[a-z][a-z0-9_]{0,31}$")
_AGENT_PERSONA_CACHE = {}


def load_agent_persona(name):
    """Return the persona text for agent `name`, or None if the name is
    invalid or the file is missing/unreadable. Cached per process. The name
    must be a lowercase slug (validated, never path-joined raw) so this can be
    fed straight from an untrusted request field without traversal risk."""
    if not isinstance(name, str):
        return None
    name = name.strip().lower()
    if not _AGENT_NAME_RE.match(name):
        return None
    if name in _AGENT_PERSONA_CACHE:
        return _AGENT_PERSONA_CACHE[name]
    path = PERSONAS_DIR / f"{name}.txt"
    try:
        text = path.read_text(encoding="utf-8").strip() or None
    except OSError:
        log.warning("persona file for agent %r not found at %s; using base persona", name, path)
        text = None
    _AGENT_PERSONA_CACHE[name] = text
    return text

# Contract fallback defaults (used when config/darwin.toml is missing).
DEFAULT_LLM = "mlx-community/Qwen3-4B-Instruct-2507-4bit"
DEFAULT_STT = "mlx-community/whisper-small-mlx"
DEFAULT_TTS = "mlx-community/Kokoro-82M-bf16"
# Contract default repo for the OPTIONAL on-device vision-language model
# (op=describe_image). A Qwen2-VL-class checkpoint that mlx-vlm can load. This
# is only a DEFAULT id — the model is a multi-GB download that ships OFF, so the
# op honestly reports "unavailable" until both mlx-vlm AND this checkpoint are
# present on the device. The pixels never leave the device.
DEFAULT_VLM = "mlx-community/Qwen2-VL-2B-Instruct-4bit"
# Contract default model for the OPTIONAL on-device text->image generator
# (op=generate_image). A FLUX.1-schnell-class checkpoint mflux can load on MLX.
# Like the VLM, this is only a DEFAULT id — the model is a multi-GB download
# that ships OFF/opt-in, so the op honestly reports "unavailable" until both the
# diffusion package AND this checkpoint are present on the device. The prompt
# and the generated pixels never leave the device (image gen is LOCAL only;
# there is NO cloud image API).
DEFAULT_IMAGE_MODEL = "schnell"
# Contract default repo for the OPTIONAL small DRAFT model used by #37
# speculative/draft decoding ([inference].draft_model). A tiny instruct
# checkpoint mlx_lm can load to PROPOSE tokens the main LLM verifies. This is
# only a DEFAULT id (the canonical one the installer pre-downloads + the config
# names); speculative stays HONESTLY inert (normal gen, speculative=false
# reported) until BOTH this checkpoint is present AND mlx_lm's speculative
# support is available. The real speedup is device/model-dependent and
# on-device only. Kept in lockstep with config/darwin.toml [inference].draft_model
# and the installer's read_model_id DEFAULT_DRAFT.
DEFAULT_DRAFT = "mlx-community/Qwen3-0.6B-4bit"
DEFAULT_VOICE = "bm_george"
DEFAULT_SPEED = 1.2
# Sane kokoro speed range (audit fix): the engine divides predicted phoneme
# durations by speed (mlx-audio kokoro.py), so speed <= 0 produces
# div-by-zero/inf durations and broken synthesis with no config-time signal,
# and the retry-at-1.0 guard only catches broadcast_shapes ValueErrors.
SPEED_MIN = 0.5
SPEED_MAX = 2.0
DEFAULT_TTS_ENGINE = "kokoro"

# ---------------------------------------------------------------------------
# ElevenLabs cloud VOICE TIER (OPTIONAL, OFF-by-default, credential+runtime
# gated). The daemon decides WHETHER to use it (voice_tier::resolve_voice_backend:
# tier on + key present + non-offline + agent mapped) and passes
# {backend:"elevenlabs", voice_id, model, el_key} on the speak request. Here we
# ONLY make the synthesis call when the daemon asked for it, and on ANY error we
# FALL BACK to on-device Kokoro so a turn is never failed by the cloud leg.
#
# HONESTY: when this path runs, the sentence text LEAVES the device (a cloud round
# trip). Kokoro stays the private/offline default + the fallback. The key is read
# ONLY from the request the daemon passed (from the Keychain) and rides ONLY the
# xi-api-key header — never logged, never on a URL/query. We request PCM output
# (pcm_24000) so the bytes are raw 16-bit/24kHz mono, which we wrap as the SAME
# pipeline WAV (via _write_wav) — no mp3 decoder dependency needed.
ELEVENLABS_API_BASE = "https://api.elevenlabs.io/v1/text-to-speech"
ELEVENLABS_OUTPUT_FORMAT = "pcm_24000"
ELEVENLABS_SAMPLE_RATE = 24000
ELEVENLABS_DEFAULT_MODEL = "eleven_flash_v2_5"
ELEVENLABS_TIMEOUT_S = 15.0
# The DENYLIST of substrings that must never appear in a log line about a TTS
# request — the key, and the header that carries it. Used by the redaction guard.
_ELEVENLABS_HEADER = "xi-api-key"

# STREAMING TTS (OPT-IN, latency tier): the SAME TTS surface as the blocking seam
# but hitting POST /v1/text-to-speech/{voice_id}/stream, which returns the audio as
# chunked-transfer binary instead of one buffered body. We request the SAME PCM
# output (pcm_24000) so the concatenated chunks decode through the IDENTICAL
# _pcm16_to_float32 + _write_wav pipeline — no new decoder, no new pip dependency
# (stdlib urllib reads the chunked body for us). This LOWERS time-to-first-byte but
# is INERT by default: `STREAM_TTS` ships False, so the default speak path is the
# existing blocking _elevenlabs_synth_pcm BYTE-FOR-BYTE, and any streaming error
# FALLS BACK to it. `optimize_streaming_latency` (0-4; deprecated upstream but still
# accepted) trades a little quality for lower latency; we pin a conservative level.
# A true bidirectional WebSocket (wss .../stream-input multi-context) is intentionally
# NOT used: the stdlib has no WebSocket client, so a clean WS path would require a new
# pip dependency (e.g. `websockets`) — out of scope for this dependency-free seam. The
# HTTP /stream path already delivers the latency win with urllib alone.
ELEVENLABS_STREAM_OUTPUT_FORMAT = "pcm_24000"
ELEVENLABS_STREAM_LATENCY = 3
ELEVENLABS_STREAM_CHUNK_BYTES = 16384
# Module-level DEFAULT for the streaming tier. SHIPS OFF — flip to True (or pass
# stream=True per call) to opt in. While False, speak() behaves exactly as today.
STREAM_TTS = False

# --- build 2/2 ElevenLabs endpoints + model selection ----------------------
# CLONE: POST /v1/voices/add (multipart name + audio sample) -> {"voice_id"}.
# The voice_id is NON-secret (the daemon stores it like any EL voice id); the
# key is secret and rides ONLY the xi-api-key header. HONESTY: cloning uploads
# the owner's audio SAMPLE to the cloud — the daemon only reaches here after an
# explicit, consent-gated, authorization-bound trigger (no impersonation, never
# automatic). On any error -> no voice_id, and the user keeps Kokoro/their voice.
ELEVENLABS_CLONE_URL = "https://api.elevenlabs.io/v1/voices/add"
# SCRIBE STT: POST /v1/speech-to-text (the audio file + model_id=scribe_v1) ->
# {"text"}. HONESTY: this sends the user's VOICE AUDIO to the cloud — MORE
# sensitive than TTS text. It runs ONLY when the daemon's gated cloud-STT tier
# named backend=elevenlabs_scribe with a key; on ANY error/missing-key the
# transcribe op FALLS BACK to on-device mlx_whisper (never fails the turn).
ELEVENLABS_SCRIBE_URL = "https://api.elevenlabs.io/v1/speech-to-text"
ELEVENLABS_SCRIBE_MODEL = "scribe_v1"
ELEVENLABS_SCRIBE_BACKEND = "elevenlabs_scribe"
# Multilingual TTS: the English-centric default (eleven_flash_v2_5) does not
# carry non-English prosody well; when the speak request names a non-English
# target language (Babel's interpret/translate output), select a multilingual
# model instead. eleven_multilingual_v2 is the stable multilingual model.
ELEVENLABS_MULTILINGUAL_MODEL = "eleven_multilingual_v2"
# #33 ADAPTIVE PROSODY: the ONLY ElevenLabs model that supports inline audio-tags
# ("[calm]", "[urgently]") + stability/style voice-settings. MUST mirror the
# daemon's `prosody::ELEVENLABS_V3_MODEL`. The rich prosody surface (audio_tag,
# stability, style) is consumed ONLY when the resolved model is exactly this id;
# on flash/multilingual the daemon never SENDS those fields (they are EL-v3-gated
# on the daemon side too), and this server-side check is defense in depth so a
# stray tag can never be spoken literally on a non-v3 model. The COARSE rate/gain
# hints are honoured on every backend.
ELEVENLABS_V3_MODEL = "eleven_v3"
# Coarse delivery-hint bounds, mirrored on the daemon side. The daemon clamps
# before sending; the server clamps again (defense in depth) so a degenerate
# value can never reach the synth path or the WAV gain.
SPEAK_RATE_MIN = 0.5
SPEAK_RATE_MAX = 2.0
SPEAK_VOLUME_MIN = 0.05
SPEAK_VOLUME_MAX = 1.0

# SOUND EFFECTS: POST /v1/sound-generation ({"text": prompt, ...}) -> audio.
# Generates a NON-speech cue (chime/alert/ambient/whoosh) from a text prompt. Like
# TTS, the key rides ONLY the xi-api-key header (never URL/query/log) and we request
# PCM (pcm_24000) so the bytes wrap into the SAME pipeline WAV via _write_wav. There
# is NO on-device SFX generator, so this is a PURE cloud capability: gated on the key
# + the daemon's [voice].cloud_sfx switch, and INERT (honest 'unavailable', never
# faked) without a key. EL caps generated length at ~22s; duration/influence are
# OPTIONAL and clamped.
ELEVENLABS_SFX_URL = "https://api.elevenlabs.io/v1/sound-generation"
ELEVENLABS_SFX_OUTPUT_FORMAT = "pcm_24000"
ELEVENLABS_SFX_DURATION_MIN = 0.5
ELEVENLABS_SFX_DURATION_MAX = 22.0

# AUDIO ISOLATION (voice isolator / background-noise removal): POST
# /v1/audio-isolation as multipart (the `audio` file part) -> cleaned audio. Strips
# background noise/ambience and leaves only the voice. Like SFX, the key rides ONLY
# the xi-api-key header (never URL/query/log) and we request PCM (pcm_24000) so the
# cleaned bytes wrap into the SAME pipeline WAV via _pcm16_to_float32 -> _write_wav.
# There is NO on-device isolator, so this is a PURE cloud capability: gated on the
# key, and INERT (honest 'unavailable', never faked) without one. HONESTY: the
# user's AUDIO leaves the device here (like Scribe STT). Endpoint confirmed against
# the 2026 ElevenLabs docs: https://elevenlabs.io/docs/api-reference/audio-isolation/convert
ELEVENLABS_AUDIO_ISOLATION_URL = "https://api.elevenlabs.io/v1/audio-isolation"
ELEVENLABS_AUDIO_ISOLATION_OUTPUT_FORMAT = "pcm_24000"

# VOICE DESIGN (prompt-to-voice): a TWO-STEP cloud flow that mints a brand-new
# NON-secret voice_id from a TEXT DESCRIPTION (no audio sample uploaded, unlike
# clone). Step 1 POST /v1/text-to-voice/design ({"voice_description", optional
# "text"}) -> {"previews": [{"generated_voice_id", ...}, ...]}; we take the FIRST
# preview's generated_voice_id. Step 2 POST /v1/text-to-voice ({"voice_name",
# "voice_description", "generated_voice_id"}) -> {"voice_id"} which is the saved
# library voice. Like TTS/clone, the key rides ONLY the xi-api-key header (never
# URL/query/log). There is NO on-device voice designer, so this is a PURE cloud
# capability: gated on the key, INERT (honest 'unavailable', never a faked id)
# without one. EL requires the description be 20-1000 chars; bad/short input just
# raises in the seam and surfaces as a clean ok:false (never a fabricated voice).
ELEVENLABS_DESIGN_URL = "https://api.elevenlabs.io/v1/text-to-voice/design"
ELEVENLABS_DESIGN_CREATE_URL = "https://api.elevenlabs.io/v1/text-to-voice"
ELEVENLABS_DESIGN_DESC_MIN = 20
ELEVENLABS_DESIGN_DESC_MAX = 1000

# PRONUNCIATION DICTIONARIES: a TEXT-ONLY cloud capability (no audio leaves the
# device on the create leg) that mints a NON-secret pronunciation dictionary from
# a list of replacement rules. Step 1 POST /v1/pronunciation-dictionaries/add-from-
# rules ({"name", "rules": [{"string_to_replace", "type": "alias"|"phoneme",
# "alias"/"phoneme", ...}]}) -> {"id", "version_id", ...}. Both ids are NON-secret
# (the daemon stores them like any EL id); the key is secret and rides ONLY the
# xi-api-key header (never URL/query/log). The minted (id, version_id) pair is then
# applied to a TTS turn via the speak body field `pronunciation_dictionary_locators`
# ([{"pronunciation_dictionary_id", "version_id"}], up to 3, applied in order) —
# folded ADDITIVELY into the TTS payload by `_build_elevenlabs_payload` ONLY when a
# non-empty locators list is threaded through, so a speak with no locators is
# byte-for-byte today's. There is NO on-device equivalent: this is a PURE cloud
# capability, gated on the key, INERT (honest 'unavailable', never a fabricated id)
# without one. EL requires a non-empty rules list; bad input just raises in the seam
# and surfaces as a clean ok:false (never a fabricated dictionary).
ELEVENLABS_PRONUNCIATION_URL = (
    "https://api.elevenlabs.io/v1/pronunciation-dictionaries/add-from-rules"
)
# EL applies pronunciation dictionaries to a TTS turn in order and caps the count.
ELEVENLABS_PRONUNCIATION_MAX_LOCATORS = 3

# MUSIC (prompt-to-music / compose): POST /v1/music ({"prompt": <=4100 chars,
# "music_length_ms": 3000..600000}) -> generated audio. Composes a NON-speech music
# track from a text prompt. Like SFX/TTS, the key rides ONLY the xi-api-key header
# (never URL/query/log) and we request PCM (pcm_24000) so the bytes wrap into the
# SAME pipeline WAV via _pcm16_to_float32 -> _write_wav. There is NO on-device music
# generator, so this is a PURE cloud capability: gated on the key + the daemon's
# [voice].cloud_music switch (+ non-Local tier), and INERT (honest 'unavailable',
# never faked) without a key.
#
# CHANNEL LAYOUT (flawlessness point): the pipeline's _pcm16_to_float32 + _write_wav
# are MONO. EL's `output_format` query param fully determines the returned layout;
# every `pcm_*` format (pcm_24000 included) is INTERLEAVED-FREE single-channel PCM16
# — identical to the SFX/TTS surfaces that already decode cleanly through the mono
# path. By PINNING output_format=pcm_24000 (a mono format) we GUARANTEE mono bytes
# at the wire, so there is no stereo interleave to misread as mono. Defense in depth:
# the seam ALSO verifies the byte count is an even number of int16 samples (a stereo
# stream would still be even, but a pinned mono format never yields stereo) and the
# decode path is the exact 16-bit mono one used everywhere else. We never request an
# mp3/stereo format, so the "interleaved stereo read as mono" hazard cannot arise.
# Endpoint confirmed against the 2026 ElevenLabs docs:
# https://elevenlabs.io/docs/api-reference/music/compose
ELEVENLABS_MUSIC_URL = "https://api.elevenlabs.io/v1/music"
ELEVENLABS_MUSIC_OUTPUT_FORMAT = "pcm_24000"
ELEVENLABS_MUSIC_LENGTH_MIN_MS = 3000
ELEVENLABS_MUSIC_LENGTH_MAX_MS = 600000
ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS = 30000
ELEVENLABS_MUSIC_PROMPT_MAX = 4100


def _is_non_english_lang(lang):
    """True when `lang` names a language that is NOT English (so the EL TTS leg
    should pick a multilingual model). Conservative: a missing/empty lang, a
    non-string, or any English spelling/code ("English", "en", "en-US", "eng")
    returns False -> the English-centric default is kept. PURE; no network.

    Babel passes the target language as a human name ("Spanish", "French") or an
    ISO-ish code; we only need the English-vs-not decision here, so we match a
    small set of English aliases and treat everything else non-empty as
    non-English (the daemon only sets `lang` when it actually wants a target
    language spoken)."""
    if not isinstance(lang, str):
        return False
    norm = lang.strip().lower()
    if not norm:
        return False
    # Strip a region suffix ("en-us" / "en_GB") down to the primary subtag.
    primary = norm.replace("_", "-").split("-", 1)[0]
    english_aliases = {"en", "eng", "en-us", "en-gb", "english"}
    return norm not in english_aliases and primary not in english_aliases


def _clamp_optional_float(value, lo, hi):
    """Clamp `value` to [lo, hi] when it is a finite real number, else None. A bad
    type / NaN / inf reads as 'absent' (the neutral default) rather than an error,
    so a malformed delivery hint can never reach the synth path. PURE; no network."""
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return None
    f = float(value)
    if f != f or f in (float("inf"), float("-inf")):
        return None
    return max(lo, min(hi, f))


def _normalize_speak_shape(req):
    """Extract + validate the OPTIONAL #33/#34 EXPRESSIVENESS shaping off a speak
    request, returning (audio_tag, stability, style, rate, volume) where each is the
    validated value or None (== 'not set' == the neutral default). PURE; no network.

    A field that is absent, the wrong type, or out of range reads as None so the
    synth path stays byte-for-byte today's when nothing valid was sent — exactly the
    posture for the shipped-OFF default, where the daemon sends none of these. The
    rich EL-v3 fields (audio_tag/stability/style) are CARRIED THROUGH here but only
    ACTED ON when the resolved model is the EL-v3 model (the speak op enforces that);
    rate/volume are coarse hints honoured on every backend. This is the seam the
    daemon's shaped params flow through — there is NO network call here."""
    audio_tag = req.get("audio_tag")
    if not (isinstance(audio_tag, str) and audio_tag.strip()):
        audio_tag = None
    stability = _clamp_optional_float(req.get("stability"), 0.0, 1.0)
    style = _clamp_optional_float(req.get("style"), 0.0, 1.0)
    rate = _clamp_optional_float(req.get("rate"), SPEAK_RATE_MIN, SPEAK_RATE_MAX)
    volume = _clamp_optional_float(req.get("volume"), SPEAK_VOLUME_MIN, SPEAK_VOLUME_MAX)
    return audio_tag, stability, style, rate, volume


def _redact_elevenlabs(s):
    """Best-effort scrub of anything key/header-shaped from a string before it can
    reach a log line. The cloud-leg error path logs only the exception class name
    through this, so even a future change can't accidentally surface the header
    name or a key fragment. Pure."""
    if not isinstance(s, str):
        s = str(s)
    return s.replace(_ELEVENLABS_HEADER, "[redacted-header]")


def _normalize_pronunciation_locators(locators):
    """Validate + normalize a list of pronunciation-dictionary LOCATORS for a TTS
    turn, returning a fresh list of {"pronunciation_dictionary_id", "version_id"}
    dicts (at most EL's cap) or [] when nothing valid was passed. PURE; no network.

    Each locator must carry a non-empty string `pronunciation_dictionary_id`; the
    `version_id` is OPTIONAL (omitted -> EL uses the latest version) and only kept
    when it is a non-empty string. A non-list, an empty list, or entries missing a
    dictionary id all read as 'none' so a malformed value can never reach the wire —
    and, crucially, so the ABSENT case folds NOTHING into the payload (keeping the
    no-locators speak path byte-for-byte today's). Order is preserved (EL applies
    locators in order); the list is truncated to EL's max so an over-long input is
    clamped rather than rejected."""
    if not isinstance(locators, list):
        return []
    out = []
    for loc in locators:
        if not isinstance(loc, dict):
            continue
        did = loc.get("pronunciation_dictionary_id")
        if not (isinstance(did, str) and did.strip()):
            continue
        entry = {"pronunciation_dictionary_id": did}
        vid = loc.get("version_id")
        if isinstance(vid, str) and vid.strip():
            entry["version_id"] = vid
        out.append(entry)
        if len(out) >= ELEVENLABS_PRONUNCIATION_MAX_LOCATORS:
            break
    return out


def _build_elevenlabs_payload(voice_id, model, text, audio_tag=None, stability=None,
                              style=None, locators=None):
    """Build the ElevenLabs TTS request JSON (as a dict) for `voice_id`/`model`.

    #33 ADAPTIVE PROSODY: the rich surface is EL-v3-GATED here too (defense in
    depth) — `audio_tag` is prefixed inline to the text AND `stability`/`style` ride
    `voice_settings` ONLY when the resolved model is exactly the EL-v3 model. On any
    other model (flash/multilingual) the tag/settings are IGNORED so a tag can never
    be spoken literally and a non-v3 model never gets v3-only settings. PURE; no
    network — split out from the seam so the payload shaping is unit-testable without
    HTTP. The daemon already withholds these fields off the v3 path, so this is a
    belt-and-suspenders gate, not the primary one.

    PRONUNCIATION DICTIONARIES: `locators` is OPTIONAL — when a non-empty list of
    valid {pronunciation_dictionary_id[, version_id]} locators is threaded through,
    it is folded ADDITIVELY into `pronunciation_dictionary_locators` (on EVERY model
    — pronunciation rules are not v3-gated). When ABSENT (None / empty / all
    invalid) the field is OMITTED entirely so the payload is BYTE-FOR-BYTE today's —
    keeping the existing no-locators tests + the default speak path unchanged."""
    resolved = model or ELEVENLABS_DEFAULT_MODEL
    body = {"text": text, "model_id": resolved}
    if resolved == ELEVENLABS_V3_MODEL:
        # Rich v3 path: inline audio-tag prefix + stability/style voice-settings.
        if isinstance(audio_tag, str) and audio_tag.strip():
            body["text"] = f"{audio_tag.strip()} {text}"
        settings = {}
        if isinstance(stability, (int, float)) and not isinstance(stability, bool):
            settings["stability"] = float(stability)
        if isinstance(style, (int, float)) and not isinstance(style, bool):
            settings["style"] = float(style)
        if settings:
            body["voice_settings"] = settings
    # ADDITIVE + model-agnostic: only present when a non-empty locators list survives
    # validation, so the no-locators path is unchanged byte-for-byte.
    norm_locators = _normalize_pronunciation_locators(locators)
    if norm_locators:
        body["pronunciation_dictionary_locators"] = norm_locators
    return body


def _elevenlabs_synth_pcm(voice_id, model, api_key, text, timeout_s=ELEVENLABS_TIMEOUT_S,
                          audio_tag=None, stability=None, style=None, locators=None):
    """THE network seam (and the ONLY place that touches the ElevenLabs network).

    POST the text to ElevenLabs TTS for `voice_id` and return raw PCM16 mono bytes
    at 24kHz (output_format=pcm_24000). The `api_key` rides ONLY the `xi-api-key`
    request header — never the URL/query, never a log line. Raises on any
    HTTP/transport error; the caller (the speak op) catches EVERYTHING and falls
    back to Kokoro, so this never fails a turn on its own.

    #33 ADAPTIVE PROSODY: `audio_tag`/`stability`/`style` are the EL-v3 rich surface;
    they are folded into the payload by `_build_elevenlabs_payload`, which acts on
    them ONLY when `model` is the EL-v3 model (EL-v3-gated, defense in depth).

    PRONUNCIATION DICTIONARIES: `locators` is the OPTIONAL list of pronunciation-
    dictionary locators ([{pronunciation_dictionary_id[, version_id]}]) the daemon
    threads through; `_build_elevenlabs_payload` folds it in ADDITIVELY only when it
    is a non-empty valid list, so a speak with no locators is byte-for-byte today's.

    This function is the mock seam: tests monkeypatch
    `server._elevenlabs_synth_pcm` to return canned bytes (or raise) WITHOUT
    touching the network — there is no real HTTP in any test. It is deliberately
    tiny and dependency-free (stdlib urllib only) so the gating + fallback logic
    around it is what gets unit-tested.

    CREDENTIAL+RUNTIME GATED: it is reached ONLY from the speak op's
    backend=="elevenlabs" branch, which the daemon enters only after its own
    tier+key+offline gate. There is no other caller."""
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request. The daemon should
        # never reach here without a key, but if it did we refuse rather than POST.
        raise ValueError("ElevenLabs synthesis requires an API key (none supplied)")
    if not voice_id:
        raise ValueError("ElevenLabs synthesis requires a voice id")

    url = f"{ELEVENLABS_API_BASE}/{voice_id}?output_format={ELEVENLABS_OUTPUT_FORMAT}"
    payload = json.dumps(
        _build_elevenlabs_payload(voice_id, model, text, audio_tag, stability, style, locators)
    ).encode("utf-8")
    req = urllib.request.Request(url, data=payload, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "audio/pcm")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        return resp.read()


def _elevenlabs_synth_pcm_stream(voice_id, model, api_key, text,
                                 timeout_s=ELEVENLABS_TIMEOUT_S, audio_tag=None,
                                 stability=None, style=None, locators=None,
                                 latency=ELEVENLABS_STREAM_LATENCY,
                                 chunk_bytes=ELEVENLABS_STREAM_CHUNK_BYTES):
    """THE STREAMING network seam (OPT-IN low-latency sibling of
    `_elevenlabs_synth_pcm`). POST `text` to the ElevenLabs STREAMING TTS endpoint
    (`/v1/text-to-speech/{voice_id}/stream`) and return raw PCM16 mono @24kHz bytes
    by reading the CHUNKED-TRANSFER response incrementally and concatenating the
    chunks. The wire body is IDENTICAL to the blocking seam (same
    `_build_elevenlabs_payload`), only the URL differs (it gains `/stream` plus an
    `optimize_streaming_latency` query param) — so the returned bytes are the same
    pcm_24000 the blocking path produces and they decode through the IDENTICAL
    `_pcm16_to_float32` + `_write_wav` pipeline.

    LATENCY WIN: chunked transfer lets the first audio bytes arrive before the whole
    clip is synthesized, lowering time-to-first-byte. We still buffer the full clip
    here (the daemon owns playback of a finished WAV), but the server can begin
    writing/forwarding sooner. STDLIB-ONLY (urllib) — no WebSocket, no new pip
    dependency; `urlopen`'s file-like response yields the de-chunked bytes via
    `resp.read(chunk_bytes)`.

    KEY HYGIENE — identical to the blocking seam: `api_key` rides ONLY the
    `xi-api-key` request header (constant `_ELEVENLABS_HEADER`), NEVER the URL/query
    (only the NON-secret output_format + latency level go on the query string) and
    NEVER a log line. Raises on any HTTP/transport error; the caller catches
    EVERYTHING and FALLS BACK to the blocking seam, so streaming never fails a turn.

    Mock seam: tests monkeypatch `server._elevenlabs_synth_pcm_stream` to return
    canned bytes (or raise) WITHOUT touching the network — there is no real HTTP in
    any test.

    CREDENTIAL+RUNTIME GATED: reached ONLY from the speak op's elevenlabs branch
    when the streaming tier is opted in (stream=True / STREAM_TTS), and only after
    the daemon's tier+key+offline gate."""
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request (same gate as blocking).
        raise ValueError("ElevenLabs streaming synthesis requires an API key (none supplied)")
    if not voice_id:
        raise ValueError("ElevenLabs streaming synthesis requires a voice id")

    # Only NON-secret shaping goes on the query string; the key stays in the header.
    lat = int(latency) if isinstance(latency, (int, float)) and not isinstance(latency, bool) else ELEVENLABS_STREAM_LATENCY
    lat = max(0, min(4, lat))
    url = (
        f"{ELEVENLABS_API_BASE}/{voice_id}/stream"
        f"?output_format={ELEVENLABS_STREAM_OUTPUT_FORMAT}"
        f"&optimize_streaming_latency={lat}"
    )
    payload = json.dumps(
        _build_elevenlabs_payload(voice_id, model, text, audio_tag, stability, style, locators)
    ).encode("utf-8")
    req = urllib.request.Request(url, data=payload, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "audio/pcm")
    chunks = []
    step = chunk_bytes if isinstance(chunk_bytes, int) and chunk_bytes > 0 else ELEVENLABS_STREAM_CHUNK_BYTES
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        # Read the chunked-transfer body incrementally and concatenate the PCM. An
        # empty read signals the de-chunked stream is complete.
        while True:
            chunk = resp.read(step)
            if not chunk:
                break
            chunks.append(chunk)
    return b"".join(chunks)


def _pcm16_to_float32(pcm_bytes):
    """Decode raw little-endian PCM16 mono bytes to a float32 array in [-1, 1],
    the same shape the engine's synth functions produce — so ElevenLabs audio
    flows through the IDENTICAL _write_wav path (fades, atomic write, pipeline WAV
    format). Pure; no network. An odd trailing byte (truncated frame) is dropped."""
    import numpy as np

    if len(pcm_bytes) % 2:
        pcm_bytes = pcm_bytes[: len(pcm_bytes) - 1]
    pcm = np.frombuffer(pcm_bytes, dtype="<i2")
    return (pcm.astype(np.float32) / 32768.0)


def _build_sfx_payload(prompt, duration_s=None, prompt_influence=None):
    """Build the ElevenLabs sound-generation request JSON (a dict) for `prompt`.
    `duration_s` (clamped to EL's 0.5-22s window) and `prompt_influence` (0-1) are
    OPTIONAL — absent/invalid values are omitted so EL auto-chooses. PURE; no network
    — split out so the payload shaping is unit-testable without HTTP."""
    body = {"text": prompt}
    d = _clamp_optional_float(duration_s, ELEVENLABS_SFX_DURATION_MIN, ELEVENLABS_SFX_DURATION_MAX)
    if d is not None:
        body["duration_seconds"] = d
    pi = _clamp_optional_float(prompt_influence, 0.0, 1.0)
    if pi is not None:
        body["prompt_influence"] = pi
    return body


def _elevenlabs_sfx(prompt, api_key, duration_s=None, prompt_influence=None,
                    timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE sound-effects network seam (the ONLY place SFX touches the EL network).

    POST `prompt` to /v1/sound-generation and return raw PCM16 mono @24kHz bytes
    (output_format=pcm_24000); the caller wraps them into the pipeline WAV. The
    `api_key` rides ONLY the `xi-api-key` request header — never the URL/query,
    never a log line. Raises on any HTTP/transport error; the caller
    (`sound_effect`) catches EVERYTHING and returns no audio, so a failure never
    fabricates a cue and never crashes a turn.

    Mock seam: tests monkeypatch `server._elevenlabs_sfx` to return canned bytes
    (or raise) WITHOUT touching the network. Stdlib-only (urllib).

    CREDENTIAL+RUNTIME GATED: reached ONLY from the sound_effect op, which the
    daemon enters only after its [voice].cloud_sfx + key gate."""
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request.
        raise ValueError("ElevenLabs sound generation requires an API key (none supplied)")
    if not (isinstance(prompt, str) and prompt.strip()):
        raise ValueError("ElevenLabs sound generation requires a non-empty prompt")

    url = f"{ELEVENLABS_SFX_URL}?output_format={ELEVENLABS_SFX_OUTPUT_FORMAT}"
    payload = json.dumps(_build_sfx_payload(prompt, duration_s, prompt_influence)).encode("utf-8")
    req = urllib.request.Request(url, data=payload, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "audio/pcm")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        return resp.read()


def _clamp_music_length_ms(length_ms):
    """Clamp `length_ms` into EL's [3000, 600000] ms window, returning an int.
    A None/bad-type/non-finite value reads as the DEFAULT (30000ms) rather than an
    error, so a malformed length can never reach the wire. PURE; no network."""
    if not isinstance(length_ms, (int, float)) or isinstance(length_ms, bool):
        return ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS
    f = float(length_ms)
    if f != f or f in (float("inf"), float("-inf")):
        return ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS
    clamped = max(ELEVENLABS_MUSIC_LENGTH_MIN_MS, min(ELEVENLABS_MUSIC_LENGTH_MAX_MS, int(f)))
    return clamped


def _build_music_payload(prompt, length_ms=None):
    """Build the ElevenLabs music (compose) request JSON (a dict) for `prompt`.

    `music_length_ms` is ALWAYS present and clamped into EL's [3000, 600000] ms
    window (a None/invalid length folds to the 30000ms default). An empty / blank /
    non-string prompt, or one longer than EL's 4100-char cap, is REJECTED with a
    ValueError BEFORE any payload is shaped (so a degenerate prompt never reaches the
    wire). PURE; no network — split out so the payload shaping is unit-testable
    without HTTP."""
    if not (isinstance(prompt, str) and prompt.strip()):
        raise ValueError("ElevenLabs music requires a non-empty prompt")
    if len(prompt) > ELEVENLABS_MUSIC_PROMPT_MAX:
        raise ValueError(
            "ElevenLabs music prompt exceeds %d chars" % ELEVENLABS_MUSIC_PROMPT_MAX
        )
    return {
        "prompt": prompt,
        "music_length_ms": _clamp_music_length_ms(length_ms),
    }


def _elevenlabs_music(prompt, api_key, length_ms=ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS,
                      timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE music network seam (the ONLY place compose-music touches the EL network).

    POST `prompt` to /v1/music and return raw PCM16 MONO @24kHz bytes
    (output_format=pcm_24000); the caller wraps them into the pipeline WAV. The
    `api_key` rides ONLY the `xi-api-key` request header — never the URL/query,
    never a log line. Raises on any HTTP/transport error; the caller
    (`compose_music`) catches EVERYTHING and returns no audio, so a failure never
    fabricates a track and never crashes a turn.

    CHANNEL LAYOUT (flawlessness point): we PIN output_format=pcm_24000, a MONO
    PCM16 format, so the bytes decode through the IDENTICAL _pcm16_to_float32 mono
    path as TTS/SFX — there is no stereo interleave to misread as mono. As defense
    in depth we ALSO assert the byte count is a whole number of int16 samples (even
    length); a truncated/odd response raises rather than silently producing skewed
    audio. We NEVER request an mp3/stereo format, so the interleaved-stereo hazard
    cannot arise.

    Mock seam: tests monkeypatch `server._elevenlabs_music` to return canned bytes
    (or raise) WITHOUT touching the network. Stdlib-only (urllib).

    CREDENTIAL+RUNTIME GATED: reached ONLY from the compose_music op, which the
    daemon enters only after its [voice].cloud_music + key gate."""
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request.
        raise ValueError("ElevenLabs music requires an API key (none supplied)")
    if not (isinstance(prompt, str) and prompt.strip()):
        raise ValueError("ElevenLabs music requires a non-empty prompt")

    # _build_music_payload re-validates the prompt and clamps the length; pinning a
    # MONO output_format here is what guarantees the mono channel layout downstream.
    url = f"{ELEVENLABS_MUSIC_URL}?output_format={ELEVENLABS_MUSIC_OUTPUT_FORMAT}"
    payload = json.dumps(_build_music_payload(prompt, length_ms)).encode("utf-8")
    req = urllib.request.Request(url, data=payload, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "audio/pcm")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        pcm = resp.read()
    # Defense in depth: pcm_24000 is mono 16-bit, so a valid body is an even number
    # of bytes (one int16 per sample). An odd length means a truncated/garbled
    # stream — raise rather than feed skewed bytes into the mono decoder.
    if pcm and (len(pcm) % 2) != 0:
        raise ValueError("ElevenLabs music returned a truncated PCM16 stream")
    return pcm


def _multipart_body(fields, file_field, file_name, file_bytes, content_type="audio/wav"):
    """Build an RFC-2388 multipart/form-data body (boundary, body bytes) from
    plain text `fields` (dict) plus one file part. Stdlib-only (no `requests`),
    so the clone seam stays dependency-free and the whole thing is unit-testable
    without the network. Returns (content_type_header, body_bytes). PURE."""
    import os as _os

    boundary = "----darwinclone" + _os.urandom(16).hex()
    crlf = b"\r\n"
    parts = []
    for name, value in fields.items():
        parts.append(b"--" + boundary.encode("ascii") + crlf)
        parts.append(
            ('Content-Disposition: form-data; name="%s"' % name).encode("utf-8") + crlf + crlf
        )
        parts.append(str(value).encode("utf-8") + crlf)
    parts.append(b"--" + boundary.encode("ascii") + crlf)
    parts.append(
        (
            'Content-Disposition: form-data; name="%s"; filename="%s"'
            % (file_field, file_name)
        ).encode("utf-8")
        + crlf
    )
    parts.append(("Content-Type: %s" % content_type).encode("ascii") + crlf + crlf)
    parts.append(file_bytes + crlf)
    parts.append(b"--" + boundary.encode("ascii") + b"--" + crlf)
    return "multipart/form-data; boundary=" + boundary, b"".join(parts)


def _elevenlabs_clone_voice(name, sample_path, api_key, timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE clone network seam (the ONLY place clone touches the ElevenLabs net).

    POST the owner audio SAMPLE at `sample_path` to /v1/voices/add as multipart
    (name=`name` + the audio file) and return the new voice_id (a string). The
    `api_key` rides ONLY the `xi-api-key` request header — never the URL/query,
    never a log line. Raises on any HTTP/transport/parse error; the caller
    (`clone_voice`) catches EVERYTHING and returns a clean no-clone result, so a
    failure never produces a voice and the user keeps their existing voice.

    This is the mock seam: tests monkeypatch `server._elevenlabs_clone_voice` to
    return a stub voice_id (or raise) WITHOUT touching the network. Stdlib-only.

    CONSENT+CREDENTIAL+RUNTIME GATED: reached ONLY from the clone_voice op, which
    the daemon enters only after an explicit, authorized consent confirm on a
    user-owned sample. HONESTY: the audio sample LEAVES the device here."""
    import json as _json
    import urllib.request

    if not api_key:
        # Never send a keyless cloud request (defense in depth).
        raise ValueError("ElevenLabs voice cloning requires an API key (none supplied)")
    if not name:
        raise ValueError("ElevenLabs voice cloning requires a display name")
    with open(sample_path, "rb") as f:
        sample_bytes = f.read()
    if not sample_bytes:
        raise ValueError("owner voice sample is empty; nothing to clone")
    file_name = os.path.basename(sample_path) or "sample.wav"
    content_type, body = _multipart_body(
        {"name": name}, "files", file_name, sample_bytes, content_type="audio/wav"
    )
    req = urllib.request.Request(ELEVENLABS_CLONE_URL, data=body, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", content_type)
    req.add_header("Accept", "application/json")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read()
    data = _json.loads(raw.decode("utf-8"))
    voice_id = data.get("voice_id") if isinstance(data, dict) else None
    if not isinstance(voice_id, str) or not voice_id:
        raise ValueError("ElevenLabs clone response had no voice_id")
    return voice_id


def _elevenlabs_isolate_audio(audio_path, api_key, timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE audio-isolation network seam (the ONLY place isolation touches the EL
    net). POST the audio file at `audio_path` to /v1/audio-isolation as multipart
    (the `audio` file part) and return the CLEANED audio bytes. We request PCM
    (output_format=pcm_24000) so the bytes are raw PCM16 mono @24kHz — the caller
    wraps them into the pipeline WAV via _pcm16_to_float32 -> _write_wav, exactly
    like SFX. The `api_key` rides ONLY the `xi-api-key` request header — never the
    URL/query, never a log line. Raises on any HTTP/transport error; the caller
    (`isolate_audio`) catches EVERYTHING and returns no audio, so a failure never
    fabricates a cleaned clip and never crashes a turn.

    This is the mock seam: tests monkeypatch `server._elevenlabs_isolate_audio` to
    return canned bytes (or raise) WITHOUT touching the network. Stdlib-only (urllib).

    CREDENTIAL+RUNTIME GATED: reached ONLY from the isolate_audio op, which the
    daemon enters only after its key gate. HONESTY: the user's AUDIO leaves the
    device here (like Scribe STT). There is NO on-device isolator."""
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request.
        raise ValueError("ElevenLabs audio isolation requires an API key (none supplied)")
    with open(audio_path, "rb") as f:
        audio_bytes = f.read()
    if not audio_bytes:
        raise ValueError("audio file is empty; nothing to isolate")
    file_name = os.path.basename(audio_path) or "audio.wav"
    # The cleaned audio comes back as raw PCM (pcm_24000) so it decodes through the
    # IDENTICAL _pcm16_to_float32 -> _write_wav path as TTS/SFX.
    url = f"{ELEVENLABS_AUDIO_ISOLATION_URL}?output_format={ELEVENLABS_AUDIO_ISOLATION_OUTPUT_FORMAT}"
    content_type, body = _multipart_body(
        {}, "audio", file_name, audio_bytes, content_type="audio/wav"
    )
    req = urllib.request.Request(url, data=body, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", content_type)
    req.add_header("Accept", "audio/pcm")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        return resp.read()


def _build_design_payload(description, text=None):
    """Build the ElevenLabs text-to-voice DESIGN request JSON (a dict) for step 1.
    `voice_description` is required (EL needs 20-1000 chars); `text` is OPTIONAL
    (a sample line EL speaks in the preview) and is included only when it is a
    non-empty string. PURE; no network — split out so the payload shaping is
    unit-testable without HTTP."""
    body = {"voice_description": description}
    if isinstance(text, str) and text.strip():
        body["text"] = text
    return body


def _build_design_create_payload(name, description, generated_voice_id):
    """Build the ElevenLabs text-to-voice CREATE request JSON (a dict) for step 2 —
    saves a chosen preview into the library. All three fields are REQUIRED by EL:
    `voice_name` (display name), `voice_description` (same description used to
    design), `generated_voice_id` (from the step-1 preview). PURE; no network."""
    return {
        "voice_name": name,
        "voice_description": description,
        "generated_voice_id": generated_voice_id,
    }


def _elevenlabs_design_voice(description, name, api_key, text=None,
                             timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE voice-design network seam (the ONLY place design touches the EL net).

    Runs the TWO-STEP prompt-to-voice flow and returns the new NON-secret
    voice_id (a string):
      1. POST /v1/text-to-voice/design ({voice_description, optional text}) and
         read the FIRST preview's `generated_voice_id`.
      2. POST /v1/text-to-voice ({voice_name, voice_description,
         generated_voice_id}) and read the saved library `voice_id`.
    The `api_key` rides ONLY the `xi-api-key` request header — never the URL/query,
    never a log line. Raises on any HTTP/transport/parse error (or a missing
    preview/voice_id); the caller (`design_voice`) catches EVERYTHING and returns a
    clean no-voice result, so a failure never fabricates a voice id and never
    crashes a turn.

    This is the mock seam: tests monkeypatch `server._elevenlabs_design_voice` to
    return a stub voice_id (or raise) WITHOUT touching the network. Stdlib-only.

    CREDENTIAL+RUNTIME GATED: reached ONLY from the design_voice op, which the
    daemon enters only after its key gate. Unlike clone, NO audio sample leaves the
    device — the voice is minted purely from the TEXT description."""
    import json as _json
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request.
        raise ValueError("ElevenLabs voice design requires an API key (none supplied)")
    if not (isinstance(description, str) and description.strip()):
        raise ValueError("ElevenLabs voice design requires a non-empty description")
    if not (isinstance(name, str) and name.strip()):
        raise ValueError("ElevenLabs voice design requires a display name")

    def _post_json(url, body):
        payload = _json.dumps(body).encode("utf-8")
        req = urllib.request.Request(url, data=payload, method="POST")
        # The key rides ONLY this header. Never a URL/query param, never logged.
        req.add_header(_ELEVENLABS_HEADER, api_key)
        req.add_header("Content-Type", "application/json")
        req.add_header("Accept", "application/json")
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            return _json.loads(resp.read().decode("utf-8"))

    # Step 1: design previews from the text description.
    designed = _post_json(ELEVENLABS_DESIGN_URL, _build_design_payload(description, text))
    previews = designed.get("previews") if isinstance(designed, dict) else None
    if not isinstance(previews, list) or not previews:
        raise ValueError("ElevenLabs design response had no previews")
    first = previews[0]
    generated_voice_id = first.get("generated_voice_id") if isinstance(first, dict) else None
    if not isinstance(generated_voice_id, str) or not generated_voice_id:
        raise ValueError("ElevenLabs design preview had no generated_voice_id")

    # Step 2: save the chosen preview into the library as a real voice.
    created = _post_json(
        ELEVENLABS_DESIGN_CREATE_URL,
        _build_design_create_payload(name, description, generated_voice_id),
    )
    voice_id = created.get("voice_id") if isinstance(created, dict) else None
    if not isinstance(voice_id, str) or not voice_id:
        raise ValueError("ElevenLabs create-voice response had no voice_id")
    return voice_id


def _build_pronunciation_payload(name, rules):
    """Build the ElevenLabs add-from-rules request JSON (a dict) for `name`/`rules`.
    `name` (a non-empty identifier) and `rules` (a non-empty list of rule dicts) are
    BOTH required by EL. Each rule is passed through as-is — EL validates the per-rule
    shape ({string_to_replace, type:'alias'|'phoneme', alias/phoneme, ...}); we only
    enforce the outer shape so a malformed top-level value never reaches the wire.
    PURE; no network — split out so the payload shaping is unit-testable without HTTP."""
    if not (isinstance(name, str) and name.strip()):
        raise ValueError("pronunciation dictionary requires a non-empty name")
    if not isinstance(rules, list) or not rules:
        raise ValueError("pronunciation dictionary requires a non-empty rules list")
    return {"name": name, "rules": rules}


def _elevenlabs_create_pronunciation(name, rules, api_key, timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE pronunciation-dictionary network seam (the ONLY place this capability
    touches the EL net). POST {name, rules} to /v1/pronunciation-dictionaries/add-
    from-rules and return the NON-secret (dictionary_id, version_id) pair (both
    strings). The `api_key` rides ONLY the `xi-api-key` request header — never the
    URL/query, never a log line. Raises on any HTTP/transport/parse error (or a
    missing id/version_id); the caller (`create_pronunciation`) catches EVERYTHING
    and returns a clean no-dictionary result, so a failure never fabricates an id and
    never crashes a turn.

    Unlike clone/Scribe, NO audio leaves the device — the dictionary is minted purely
    from the text rules. This is the mock seam: tests monkeypatch
    `server._elevenlabs_create_pronunciation` to return a stub pair (or raise) WITHOUT
    touching the network. Stdlib-only (urllib).

    CREDENTIAL+RUNTIME GATED: reached ONLY from the create_pronunciation op, which the
    daemon enters only after its key gate."""
    import json as _json
    import urllib.request

    if not api_key:
        # Defense in depth: never send a keyless cloud request.
        raise ValueError("ElevenLabs pronunciation dictionary requires an API key (none supplied)")
    if not (isinstance(name, str) and name.strip()):
        raise ValueError("ElevenLabs pronunciation dictionary requires a non-empty name")
    if not isinstance(rules, list) or not rules:
        raise ValueError("ElevenLabs pronunciation dictionary requires a non-empty rules list")

    payload = _json.dumps(_build_pronunciation_payload(name, rules)).encode("utf-8")
    req = urllib.request.Request(ELEVENLABS_PRONUNCIATION_URL, data=payload, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read()
    data = _json.loads(raw.decode("utf-8"))
    dictionary_id = data.get("id") if isinstance(data, dict) else None
    version_id = data.get("version_id") if isinstance(data, dict) else None
    if not isinstance(dictionary_id, str) or not dictionary_id:
        raise ValueError("ElevenLabs pronunciation response had no id")
    if not isinstance(version_id, str) or not version_id:
        raise ValueError("ElevenLabs pronunciation response had no version_id")
    return dictionary_id, version_id


def _elevenlabs_scribe_transcribe(audio_path, api_key, model=ELEVENLABS_SCRIBE_MODEL,
                                  timeout_s=ELEVENLABS_TIMEOUT_S):
    """THE Scribe STT network seam (the ONLY place STT touches the ElevenLabs
    net). POST the audio file at `audio_path` to /v1/speech-to-text as multipart
    (the file + model_id) and return the transcript text. The `api_key` rides
    ONLY the `xi-api-key` request header — never the URL/query, never a log line.
    Raises on any HTTP/transport/parse error; the caller (`transcribe`) catches
    EVERYTHING and FALLS BACK to mlx_whisper, so a cloud failure never fails the
    turn.

    This is the mock seam: tests monkeypatch `server._elevenlabs_scribe_transcribe`
    to return canned text (or raise) WITHOUT touching the network. Stdlib-only.

    CREDENTIAL+RUNTIME GATED: reached ONLY when the daemon's gated cloud-STT tier
    named backend=elevenlabs_scribe with a key. HONESTY: the user's VOICE AUDIO
    leaves the device here — MORE sensitive than TTS text. mlx_whisper is the
    private/offline default + the fallback."""
    import json as _json
    import urllib.request

    if not api_key:
        raise ValueError("ElevenLabs Scribe STT requires an API key (none supplied)")
    with open(audio_path, "rb") as f:
        audio_bytes = f.read()
    if not audio_bytes:
        raise ValueError("audio file is empty; nothing to transcribe")
    file_name = os.path.basename(audio_path) or "audio.wav"
    content_type, body = _multipart_body(
        {"model_id": model or ELEVENLABS_SCRIBE_MODEL},
        "file",
        file_name,
        audio_bytes,
        content_type="audio/wav",
    )
    req = urllib.request.Request(ELEVENLABS_SCRIBE_URL, data=body, method="POST")
    # The key rides ONLY this header. Never a URL/query param, never logged.
    req.add_header(_ELEVENLABS_HEADER, api_key)
    req.add_header("Content-Type", content_type)
    req.add_header("Accept", "application/json")
    with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        raw = resp.read()
    data = _json.loads(raw.decode("utf-8"))
    text = data.get("text") if isinstance(data, dict) else None
    if not isinstance(text, str):
        raise ValueError("ElevenLabs Scribe response had no text")
    # #31: Scribe MAY carry a per-word `words` stream with `speaker_id`s when it
    # diarized. We surface it (untouched) so the daemon's gated [voice].diarize path
    # can CONSUME the real labels — never fabricated. None/absent when Scribe sent no
    # word detail (then the daemon's honest single-stream fallback applies). The
    # words stream is text + timings + speaker ids only — never the audio.
    words = data.get("words") if isinstance(data, dict) else None
    if not isinstance(words, list):
        words = None
    return text, words


# #31 MULTI-SPEAKER DIARIZATION — the PURE Scribe-label assignment.
#
# Scribe (op=transcribe, backend=elevenlabs_scribe) returns a `words` stream where
# each word may carry a `speaker_id` (and start/end timings) when it diarized. This
# PURE function CONSUMES those labels into per-speaker turns: contiguous words sharing
# a speaker_id are coalesced into one turn; a change of speaker starts a new turn. It
# is the Python mirror of the daemon's `diarize::diarize` (diarize.rs) — same honesty
# rails:
#   * report ONLY the speakers Scribe reported (distinct turns iff Scribe distinguished
#     them);
#   * a word with no speaker_id is attributed to "unknown" (NEVER fabricated as a
#     distinct speaker), joining/extending the surrounding unknown run;
#   * spacing / audio_event tokens are skipped (never their own turn);
#   * timings are carried only when present (never invented).
#
# This is OFF-by-default + EL-SCRIBE-GATED at the daemon ([voice].diarize); on-device
# whisper has no diarization model and gets the honest single-stream labeling instead.
# PURE: no I/O, no network — exercised by `_selftest_diarize`.
DIARIZE_UNKNOWN_SPEAKER = "unknown"


def _diarize_scribe_words(data):
    """Map a parsed Scribe response dict to a list of per-speaker turns
    [{"speaker_id", "text", "start", "end"}]. PURE; consumes only the labels Scribe
    reported, never fabricating distinct speakers. An absent/empty `words` stream
    falls back to ONE "unknown" turn carrying the whole `text` (the response did not
    diarize, so we do not pretend it did); empty text -> []."""
    if not isinstance(data, dict):
        return []
    words = data.get("words")
    word_items = []
    if isinstance(words, list):
        for w in words:
            if not isinstance(w, dict):
                continue
            kind = w.get("type")
            # A missing type is treated as a word; spacing/audio_event are skipped.
            if kind is not None and kind != "word":
                continue
            word_items.append(w)
    if not word_items:
        text = (data.get("text") or "").strip()
        if not text:
            return []
        return [{"speaker_id": DIARIZE_UNKNOWN_SPEAKER, "text": text, "start": None, "end": None}]

    turns = []
    for w in word_items:
        sid = w.get("speaker_id")
        speaker = sid.strip() if isinstance(sid, str) and sid.strip() else DIARIZE_UNKNOWN_SPEAKER
        wtext = (w.get("text") or "").strip()
        start = w.get("start") if isinstance(w.get("start"), (int, float)) else None
        end = w.get("end") if isinstance(w.get("end"), (int, float)) else None
        if turns and turns[-1]["speaker_id"] == speaker:
            if wtext:
                turns[-1]["text"] = (turns[-1]["text"] + " " + wtext).strip()
            if end is not None:
                turns[-1]["end"] = end if turns[-1]["end"] is None else max(turns[-1]["end"], end)
        else:
            turns.append({"speaker_id": speaker, "text": wtext, "start": start, "end": end})
    # Drop any turn that ended up textless.
    return [t for t in turns if t["text"].strip()]


# ---------------------------------------------------------------------------
# On-device VISION-LANGUAGE MODEL seam (op=describe_image, OPTIONAL/OFF).
#
# This is the ONLY place mlx-vlm is imported. It is IMPORT-GUARDED: if mlx-vlm
# is not installed (it is an optional dep), the import fails and we return None
# so the op reports an honest "vision-language model not available" — py_compile
# and the whole server work WITHOUT mlx-vlm present. This is also the MOCK SEAM:
# tests monkeypatch `server._load_mlx_vlm` to inject a fake loader (or to force
# the unavailable path) so the op dispatch + the fallback are exercised with NO
# real model, NO MLX, and NO network.
#
# HONESTY: when this runs for real, the VLM runs ON-DEVICE via MLX — the image
# pixels are read locally and NEVER leave the device. The model is a multi-GB
# download (Qwen2-VL-class) gated on enough RAM, which is why the op ships OFF.
def _load_mlx_vlm():
    """Return the mlx-vlm callables we need (load, generate, apply_chat_template,
    load_config) as a dict, or None if mlx-vlm is not installed.

    Import-guarded: a missing mlx-vlm is the NORMAL ships-OFF state, not an
    error — the caller turns a None return into an honest structured
    "unavailable" response. We import the few symbols by their documented
    mlx-vlm locations and tolerate minor version layout differences."""
    try:
        from mlx_vlm import generate as _generate
        from mlx_vlm import load as _load
        from mlx_vlm.prompt_utils import apply_chat_template as _apply_chat_template
        from mlx_vlm.utils import load_config as _load_config
    except Exception:
        # ImportError (not installed) OR any layout error in an unexpected
        # mlx-vlm version: treat as unavailable rather than crashing the op.
        return None
    return {
        "load": _load,
        "generate": _generate,
        "apply_chat_template": _apply_chat_template,
        "load_config": _load_config,
    }


# ---------------------------------------------------------------------------
# #37 SPECULATIVE DECODING — import-guarded availability probe for mlx_lm's
# speculative/draft generation support. This is the ONLY place we check that
# mlx_lm can do draft generation; it is IMPORT-GUARDED exactly like
# _load_mlx_vlm / _load_mlx_diffusion: an mlx_lm without speculative support (or
# not installed) returns False so the generate path honestly falls back to
# NORMAL generation and reports speculative=False — py_compile and the whole
# server work WITHOUT speculative support. This is also the MOCK SEAM: tests
# monkeypatch `server._mlx_speculative_available` to force on/off so the
# should_use_speculative decision + the fallback are exercised with NO real
# model, NO MLX, and NO draft load.
#
# HONESTY: even when this is True and a draft model is configured, the actual
# speedup is device/model-dependent and only real on-device — it is NEVER
# measured or claimed headlessly. The server reports the path that ACTUALLY ran.
def _mlx_speculative_available():
    """Whether mlx_lm exposes draft/speculative generation we can drive. Returns
    True iff the symbols are importable; any ImportError / layout mismatch (an
    mlx_lm too old for speculative decoding, or not installed) -> False so the
    generate path falls back to normal generation honestly."""
    try:
        # mlx_lm threads a `draft_model` through generate/stream_generate for
        # speculative decoding. We probe the import surface only; we do NOT load
        # a model or run a generate here (that is the device-gated live path).
        from mlx_lm import generate as _generate  # noqa: F401
        from mlx_lm import stream_generate as _stream_generate  # noqa: F401
    except Exception:
        return False
    return True


# ---------------------------------------------------------------------------
# On-device IMAGE-GENERATION (text->image) seam (op=generate_image, OPTIONAL/OFF).
#
# This is the ONLY place the MLX diffusion package is imported. It is
# IMPORT-GUARDED exactly like _load_mlx_vlm above: the diffusion package
# (mflux — a Stable-Diffusion/FLUX-class text->image engine on MLX) is an
# OPTIONAL dep. If it is not installed, the import fails and we return None so
# the op reports an honest "image model not available" — py_compile and the
# whole server work WITHOUT it. This is also the MOCK SEAM: tests monkeypatch
# `server._load_mlx_diffusion` to inject a fake generator (or to force the
# unavailable path) so the op dispatch + the fallback are exercised with NO
# real model, NO MLX, and NO image generation.
#
# HONESTY: when this runs for real, image generation is 100% ON-DEVICE via MLX —
# the prompt and the generated pixels stay on the machine and NEVER leave the
# device (there is NO cloud image API anywhere in this path). The model is a
# multi-GB download gated on enough RAM and is slow on smaller chips, which is
# why the op ships OFF/opt-in.
def _load_mlx_diffusion():
    """Return the MLX diffusion callable(s) we need as a dict, or None if the
    diffusion package is not installed.

    Import-guarded: a missing diffusion package is the NORMAL ships-OFF state,
    not an error — the caller turns a None return into an honest structured
    "unavailable" response. We import from mflux (a FLUX/Stable-Diffusion-class
    on-device MLX text->image engine) and tolerate minor version layout
    differences. NEVER falls back to a cloud image API: absent package == honest
    unavailable, full stop."""
    try:
        from mflux import Config as _Config
        from mflux import Flux1 as _Flux1
    except Exception:
        # ImportError (not installed) OR any layout error in an unexpected
        # version: treat as unavailable rather than crashing the op.
        return None
    return {
        "Flux1": _Flux1,
        "Config": _Config,
    }


# Opener bank ([speech].openers fallback): short acknowledgement lines
# synthesized to state/openers/opener-<idx>.wav at preload so the daemon can
# play one the instant an utterance ends, before STT even starts. Must stay
# in lockstep with [speech].openers in config/darwin.toml.
DEFAULT_OPENERS = [
    "Right away, sir.",
    "Of course.",
    "One moment.",
    "On it, sir.",
    "Let me see.",
]

# TTS engine registry: per-engine default HF repo and default voice (the most
# DARWIN-suitable voice each engine offers). [speech].engine selects the
# engine, [speech].model overrides the repo ("" = this default). Adoption is
# GATED: a non-kokoro engine ships as default only after measuring warm
# RTF (= synth_seconds / audio_seconds) <= 0.5 on the target machine, a clean
# load, and output that survives the silence-trim path.
# 2026-06-12 eval (M1 Pro 16GB, mlx-audio 0.4.4, warm, GPU otherwise idle,
# same audition line; samples in state/voice-samples/<engine>-<voice>.wav):
#   kokoro/bm_george           RTF 0.069 (runs 0.071/0.069)  PASS
#   csm-1b-8bit/conv_b         RTF 0.931 (runs 1.123/0.931)  FAIL (loads
#       clean, trim OK; ~9s synth for 9.7s audio)
#   orpheus-3b-4bit/leo        RTF 1.28  (runs 1.298/1.280)  FAIL (loads
#       clean, trim OK; slower than realtime)
# Both candidates are LLM-class TTS — in real use they would also share the
# GPU with the resident Qwen3-4B converse decode and add ~1.7-1.9GB each on a
# 16GB machine, so the idle-GPU numbers above flatter them. Kokoro stays the
# default; this registry is the dispatch/config hook for a re-run on newer,
# higher-bandwidth Apple Silicon.
TTS_ENGINE_DEFAULTS = {
    "kokoro": {"model": DEFAULT_TTS, "voice": DEFAULT_VOICE},
    # Sesame CSM 1B (8-bit MLX). Voice = repo speaker prompt; conversational_b
    # is the male prompt. speed is not supported by the model.
    "csm": {"model": "mlx-community/csm-1b-8bit", "voice": "conversational_b"},
    # Orpheus 3B ft (4-bit MLX). Male voices: leo (warm), dan, zac; no British
    # voice exists. speed is not supported by the model.
    "orpheus": {"model": "mlx-community/orpheus-3b-0.1-ft-4bit", "voice": "leo"},
}
# Dedicated classify model ("" = reuse the main LLM). Must stay in lockstep
# with [models].classifier in config/darwin.toml. Gated: only ships non-empty
# after passing the 7-utterance accuracy eval (>=6/7, all heavy cases heavy).
# 2026-06-12 (M1 Pro, mlx_lm 0.31.3): every small candidate FAILED the gate —
# Qwen3-0.6B-4bit 4/7 (parrots the prompt's last few-shot example for memory
# and greeting utterances; ~290ms vs ~870ms on the 4B), Qwen2.5-0.5B-Instruct
# -4bit 4/7 + classified the script-writing case light, Llama-3.2-1B-Instruct
# -4bit 3/7. The main 4B scores 7/7, so classify stays on it for now.
DEFAULT_CLASSIFIER = ""

# 160, not 80: the args contract means web.search/memory.store outputs echo
# utterance content inside args, and a JSON skeleton + ~38 words already hit
# 80 tokens — truncation there turns a valid classification into the
# cloud-escalation fallback. The prefix is KV-cached and greedy decode stops
# at EOS, so the higher cap only costs anything on the rare long outputs.
CLASSIFY_MAX_TOKENS = 160
GENERATE_DEFAULT_MAX_TOKENS = 160
EXTRACT_FACTS_MAX_TOKENS = 120

# op=embed: hard cap on how many tokens of one input we encode before
# mean-pooling its hidden states. MNEMOSYNE embeds short facts and a short
# query, so this is generous — it exists only to bound a pathological input
# (a giant pasted blob) from holding the GPU lock or blowing memory. Inputs
# longer than this are truncated to the first EMBED_MAX_TOKENS tokens before
# the forward pass (an embedding of the lead is still useful; the alternative
# is an unbounded forward). The batch is also capped (EMBED_MAX_BATCH) so one
# request cannot enqueue an unbounded number of forward passes.
EMBED_MAX_TOKENS = 512
EMBED_MAX_BATCH = 256
# op=embed runs the whole request as ONE padded forward per chunk instead of a
# forward per text (the daemon sends a query + K candidate facts together, so
# batching amortizes the per-forward launch/graph overhead — ~2.2x on M1 Pro for
# a realistic batch). We chunk the batch so the padded [chunk, maxlen, hidden]
# activation tensor stays bounded no matter how large EMBED_MAX_BATCH is, and so
# one long input only pads its own chunk. Right-padding + the model's default
# causal mask means each real token attends only to earlier REAL tokens, so a
# real token's hidden state is identical to the unpadded run; pad positions are
# excluded from the mean-pool. Vectors match the per-text path up to fp
# reduction-order noise (cosine >= 0.9999) — behaviour-preserving for retrieval.
#
# A chunk is closed by WHICHEVER cap hits first:
#   - EMBED_BATCH_CHUNK rows, or
#   - EMBED_CHUNK_TOKEN_BUDGET PADDED tokens (rows x the chunk's max length), or
#   - EMBED_CHUNK_WASTE_FACTOR: padded tokens would exceed this multiple of the
#     chunk's REAL tokens (the padding-waste guard).
# The token budget is the activation bound: without it, 32 rows x 512 tokens =
# 16384 padded tokens in ONE forward — an activation spike the old per-text path
# (never more than 512 tokens in flight) could not hit. The waste guard bounds
# AMPLIFICATION: without it, a crafted short/long mix (31 one-token texts + one
# 128-token text = 32 rows x maxlen 128) computes 4096 padded positions for 159
# real tokens — ~26x the PADDED TOKENS (~3x the GPU time, per-forward overhead
# absorbs the rest — review-measured) of the per-text path for the same
# request, all under the engine lock. With the guard, a multi-row
# chunk's padded cost is <= EMBED_CHUNK_WASTE_FACTOR x its real tokens BY
# CONSTRUCTION, so no length mix can multiply lock-hold time past that factor;
# a single row is always a legal chunk of one (exact cost, nothing to waste).
# A realistic short-fact batch (~15 tokens/text, near-uniform) triggers neither
# cap and still packs EMBED_BATCH_CHUNK rows into one forward.
EMBED_BATCH_CHUNK = 32
EMBED_CHUNK_TOKEN_BUDGET = 4096
EMBED_CHUNK_WASTE_FACTOR = 2

# ---- op=embed BACKEND SELECTION ([inference].embedder) ----------------------
# The op=embed backend is config-selectable. Two vector SPACES exist; the
# op=embed response carries the ACTIVE backend's vector-space id + dim, which the
# daemon/docsearch store with the index and compare by STRING EQUALITY only (an
# OPAQUE space id) to refuse cross-space comparison:
#   * EMBEDDER_COREML — a purpose-built Core ML contrastive sentence embedder
#     (BAAI/bge-small-en-v1.5, 384-dim; see inference/coreml_embed.py). ONE
#     pinned model at ONE seq, so its emitted space id is the FIXED string below.
#     Retrieval quality (synthetic-but-representative, committed under
#     inference/benchmarks/coreml_eval/) recall@1/3/5 0.82/0.92/0.99, MRR 0.96 vs
#     the 4B path's 0.25/0.46/0.56, MRR 0.42 — dramatically better AND several
#     times faster (~6x single / ~3x per text on a K=8 batch at seq=512; see
#     coreml_embed.py). The state-of-the-art choice, so it is the DEFAULT.
#   * EMBEDDER_LLM_MEANPOOL — the legacy path: mean-pool the resident LLM's last
#     hidden states. Reuses the already-loaded generate model (no extra
#     download), but slower and much weaker at retrieval.
#
# CONFIG SELECTOR vs EMITTED SPACE ID — these are two different things:
#   - EMBEDDER_COREML / EMBEDDER_LLM_MEANPOOL are the [inference].embedder CONFIG
#     values (a stable user choice; validated by validate_embedder).
#   - The id EMITTED on the wire as the vector-space stamp is the CONFIG value
#     for the Core ML path (one pinned model), but for the mean-pool path it is
#     MODEL-DERIVED (`_meanpool_space_id`: llm-meanpool:<llm_id>:<quant>[:<adapter>]), because
#     that path mean-pools WHATEVER [models].llm loads — a fixed string would
#     keep matching across an LLM/quant swap while the space silently changed, so
#     the stamp must reflect the resident model.
EMBEDDER_COREML = "coreml-bge-small-en-v1.5"
EMBEDDER_LLM_MEANPOOL = "llm-qwen3-4b-meanpool"
ALLOWED_EMBEDDERS = (EMBEDDER_COREML, EMBEDDER_LLM_MEANPOOL)
# DEFAULT = the Core ML bge embedder (faster AND higher quality — the SOTA
# choice). HONEST FALLBACK: if it cannot be built/loaded, op=embed serves the
# mean-pool path instead and REPORTS the fallback (embedder id + fell_back flag +
# a log warn) — never silently.
DEFAULT_EMBEDDER = EMBEDDER_COREML
# The Core ML embedder's fixed output dimension (bge-small hidden size). Mirrors
# coreml_embed.DIM; kept here so the pure selection logic needs no heavy import.
COREML_EMBED_DIM = 384

# ---- op=rerank BACKEND ([inference].reranker) -------------------------------
# STAGE TWO of the two-stage retrieval stack: the daemon recalls the top-K
# candidates by dense bge cosine (op=embed, above), then op=rerank RE-SCORES those
# K with a Core ML cross-encoder (cross-encoder/ms-marco-MiniLM-L-6-v2 — full
# query x passage attention, ONE relevance logit per pair; see
# inference/coreml_rerank.py) and the daemon re-orders by that score. This is the
# standard SOTA RAG stack (cheap recall -> precise rerank).
#
# MEASURE-FIRST: it ships ON because the two rankings were MEASURED head to head on
# the committed synthetic-but-representative retrieval eval and the rerank WON on
# every metric (nDCG@10 0.955->0.984, recall@1 0.82->0.87, recall@3 0.92->0.98,
# MRR 0.96->0.99; probe + results under inference/benchmarks/coreml_rerank_eval/).
# Same discipline that DROPPED speculative decoding + quantized-KV when they
# measured as losses — a rerank that did not win would be a NO-GO.
#
# HONEST FALLBACK: if the cross-encoder cannot be built/loaded, op=rerank reports
# fell_back=True (and an EMPTY reranker id — no model produced the order) and the
# daemon keeps the dense retrieval order, surfaced + logged, never silently.
# `[inference].reranker` (bool, ships True) gates it: the DAEMON reads it (config.rs
# InferenceConfig) to decide whether to CALL op=rerank at all, and the server reads
# it (below) to decide whether to WARM the cross-encoder at preload.
RERANKER_COREML = "coreml-ms-marco-minilm-l6-v2"
# Ships ON — the measured winner. False disables the second stage (dense order
# only). The Core ML cross-encoder is still an HONEST FALLBACK away when ON but
# unbuildable.
DEFAULT_RERANKER_ENABLED = True
# Hard cap on how many candidate passages one op=rerank request may carry. The
# daemon reranks a bounded top-K (K is small: 20/50), so this only guards a
# pathological caller from enqueueing an unbounded number of forwards. Mirrors
# coreml_rerank.MAX_PASSAGES; surfaced as an error (never a silent clamp).
RERANK_MAX_PASSAGES = 256


def validate_embedder(value):
    """PURE. Return `value` if it is a known [inference].embedder CONFIG value,
    else raise ValueError. The config-load seam turns the ValueError into a warn
    + keep-default. (The daemon does NOT value-validate this key — it only lists
    it in config.rs KNOWN_KEYS so a typo under [inference] is diagnosed; the
    Python server is the sole validator of the value.)"""
    if value in ALLOWED_EMBEDDERS:
        return value
    raise ValueError(
        f"unknown embedder {value!r}; allowed: {', '.join(ALLOWED_EMBEDDERS)}"
    )


def plan_embedder(choice, coreml_available, meanpool_id):
    """PURE selection/fallback decision for op=embed. Given the configured
    `choice`, whether the Core ML embedder is usable, and the MODEL-DERIVED
    mean-pool space id, return (embedder_id, dim_or_None, fell_back):
      * choice=coreml + available   -> (EMBEDDER_COREML, COREML_EMBED_DIM, False)
      * choice=coreml + UNavailable -> (meanpool_id, None, True)   # honest fallback
      * choice=mean-pool            -> (meanpool_id, None, False)
    `dim` is None for the mean-pool path because its true dimension is the
    resident model's hidden size, known only from a produced vector; the caller
    fills it from the actual vectors. The Core ML dim is fixed (384). The
    mean-pool id is passed in (not a constant) so it reflects the resident model
    — see `_meanpool_space_id`."""
    if choice == EMBEDDER_COREML:
        if coreml_available:
            return (EMBEDDER_COREML, COREML_EMBED_DIM, False)
        return (meanpool_id, None, True)
    return (meanpool_id, None, False)


def _pack_embed_chunks(lengths, row_cap=None, token_budget=None, waste_factor=None):
    """PURE, ORDER-PRESERVING chunker for the batched embed forward: split the
    row index space [0, len(lengths)) into contiguous [start, end) ranges where
    each range holds at most `row_cap` rows, at most `token_budget` PADDED
    tokens (rows x the range's max length — the true activation footprint of the
    padded forward), AND — for multi-row ranges — padded tokens no more than
    `waste_factor` x the range's REAL tokens (the amplification guard). Greedy:
    extend the current chunk until adding the next row would break a cap, then
    close it. A single row always forms a legal chunk, whatever its length (it
    cannot be split, and alone it wastes nothing). Returns a list of (start, end).

    INVARIANT (tested): for every returned multi-row range,
    rows x max(lengths) <= min(token_budget, waste_factor x sum(lengths)) — so a
    crafted short/long mix cannot multiply the GPU cost of the padded forward
    past waste_factor of what the per-text path would have paid."""
    if row_cap is None:
        row_cap = EMBED_BATCH_CHUNK
    if token_budget is None:
        token_budget = EMBED_CHUNK_TOKEN_BUDGET
    if waste_factor is None:
        waste_factor = EMBED_CHUNK_WASTE_FACTOR
    ranges = []
    start = 0
    cur_max = 0
    real = 0  # real (unpadded) tokens in the current chunk
    for i, n in enumerate(lengths):
        rows = i - start + 1
        new_max = n if n > cur_max else cur_max
        # The padded cost if row i joins the current chunk, vs all three caps.
        padded = rows * new_max
        if i > start and (
            rows > row_cap
            or padded > token_budget
            or padded > waste_factor * (real + n)
        ):
            ranges.append((start, i))
            start = i
            new_max = n
            real = 0
        cur_max = new_max
        real += n
    if start < len(lengths):
        ranges.append((start, len(lengths)))
    return ranges


def _plan_embed_batches(lengths, row_cap=None, token_budget=None, waste_factor=None):
    """PURE planner for the batched embed forwards: pack rows LENGTH-SORTED so
    chunkmates have near-equal lengths, then return each chunk as a list of
    ORIGINAL indices (the caller computes in plan order and writes each result
    back to its original slot, so output order is untouched).

    Why sort: the greedy packer alone admits crafted short/long mixes with up to
    ~26x padded-token amplification (e.g. one 128-token text + 31 one-token
    texts packs as 32 rows x maxlen 128 = 4096 padded positions for 159 real
    tokens — review-confirmed), multiplying the GPU time spent under the engine
    lock vs the per-text path for the SAME request. Sorting makes each chunk's
    lengths adjacent in the distribution (so the packer's waste guard almost
    never has to fragment a real batch), while the packer's
    EMBED_CHUNK_WASTE_FACTOR cap HARD-bounds amplification for any residual
    mix. Each row's vector depends only on its own tokens (causal mask + masked
    pool), so grouping order cannot change any result. The sort is stable ->
    deterministic plan."""
    order = sorted(range(len(lengths)), key=lambda i: lengths[i])
    ranges = _pack_embed_chunks(
        [lengths[i] for i in order],
        row_cap=row_cap,
        token_budget=token_budget,
        waste_factor=waste_factor,
    )
    return [order[a:b] for a, b in ranges]


def _pool_normalize(mx, hidden, lengths):
    """PURE masked-mean-pool + L2-normalize. `hidden` is a [B, T, H] tensor of
    per-token last hidden states; `lengths[i]` is row i's real-token count (the
    rest are right-padding). Mean-pool over each row's real tokens only, then
    L2-normalize so cosine similarity is a dot product, mapping any non-finite
    component to 0.0 (a degenerate-but-finite vector keeps the JSON response
    strict-valid instead of failing the whole batch). Returns a [B, H] tensor.

    No model / no tokenizer / no I/O — unit-testable with synthetic hidden states
    and a stub decoder. `mx` is injected so this stays import-light."""
    _b, t, _h = hidden.shape
    # [B, T] boolean: column j is real iff j < lengths[i]. Built on-device from
    # `lengths` so there is no per-token Python loop.
    valid = mx.arange(t)[None, :] < mx.array(lengths)[:, None]
    maskf = valid.astype(hidden.dtype)[:, :, None]  # [B, T, 1]
    summed = mx.sum(hidden * maskf, axis=1)  # [B, H] — pads contribute 0
    counts = mx.maximum(mx.sum(maskf[:, :, 0], axis=1), 1.0)[:, None]  # [B, 1]
    pooled = summed / counts
    norm = mx.sqrt(mx.sum(pooled * pooled, axis=1, keepdims=True))
    normed = pooled / mx.maximum(norm, 1e-12)
    return mx.where(mx.isnan(normed) | mx.isinf(normed), 0.0, normed)

# op=describe_image (on-device VLM): structured marker the op returns when the
# vision-language model cannot run (mlx-vlm not installed, or the checkpoint not
# downloaded). The daemon keys off ok:false + reason="vlm_unavailable" to fall
# back honestly (OCR/classification, or an honest "the vision model isn't
# downloaded") — it NEVER substitutes a fabricated description. This op is the
# ONLY place pixels are read, and they are read LOCALLY (never sent anywhere).
DESCRIBE_IMAGE_UNAVAILABLE_REASON = "vlm_unavailable"
# Decode budget for one description/answer. Generous enough for a paragraph of
# scene description or a VQA answer; bounds a pathological generation from
# holding the GPU lock. The daemon may not override it past this hard cap.
DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS = 256
DESCRIBE_IMAGE_MAX_TOKENS_CAP = 1024
# Default instruction when the caller asks for a plain description (no question).
DESCRIBE_IMAGE_DEFAULT_PROMPT = (
    "Describe this image in detail. Focus on the visual scene — objects, "
    "people, layout, colors, and what is happening — not just any text in it."
)
# Cap on the question length we forward to the VLM (a guard, not the normal
# case; VQA questions are short).
DESCRIBE_IMAGE_MAX_QUESTION_CHARS = 2000

# op=generate_image (on-device text->image): structured marker the op returns
# when the diffusion model cannot run (the diffusion package is not installed,
# or the checkpoint is not downloaded / fails to load). The daemon keys off
# ok:false + reason="image_model_unavailable" to surface an honest "the
# on-device image model isn't set up" message — it NEVER substitutes a
# fabricated image and NEVER falls back to a cloud image API. This op is the
# ONLY place the prompt is handed to a generator, and that generator is the
# on-device MLX diffusion model (the prompt + the pixels never leave the device).
GENERATE_IMAGE_UNAVAILABLE_REASON = "image_model_unavailable"
# Default sampling steps for one generation. Diffusion quality/speed trade off
# against step count; this is a sane middle default the caller may override
# (bounded by the cap below). The image QUALITY/speed are device/runtime-gated
# and are NOT claimed measured here.
GENERATE_IMAGE_DEFAULT_STEPS = 4
GENERATE_IMAGE_MAX_STEPS_CAP = 50
# Default square output resolution (pixels). Overridable per request, bounded by
# the cap so one request cannot ask for a pathological allocation.
GENERATE_IMAGE_DEFAULT_SIZE = 512
GENERATE_IMAGE_MIN_SIZE = 64
GENERATE_IMAGE_MAX_SIZE = 1536
# Cap on the prompt length we forward to the diffusion model (a guard; a wildly
# long prompt is truncated, not rejected).
GENERATE_IMAGE_MAX_PROMPT_CHARS = 2000
# On-device output directory for generated images (under state/, never the
# network). Saved here and the absolute path is returned to the daemon; the
# pixels stay on the machine.
IMAGES_DIR = PROJECT_ROOT / "state" / "images"

# op=consolidate: greedy decode budget, hard caps on what one reflection pass
# may touch (upserts + deletes combined) and on the transcript window the
# prompt is built from (the daemon sends the last 40 exchanges).
CONSOLIDATE_MAX_TOKENS = 400
CONSOLIDATE_MAX_CHANGES = 12
CONSOLIDATE_MAX_TRANSCRIPTS = 40

# op=converse synthesizes at most this many leading sentences; the rest of
# the reply is returned as text only (the persona keeps replies short, so
# this is a guard, not the normal case).
CONVERSE_MAX_SPOKEN_SENTENCES = 5

# Per-agent persona KV prompt caches kept resident at once (an LRU). The base
# persona has its own dedicated cache (self._gen_cache); this bounds the EXTRA
# agent-prefix caches so a long session that touches many agents does not grow
# GPU memory without limit. A handful covers the agents a user actually rotates
# through in a session; the least-recently-used cache is evicted past this.
AGENT_PERSONA_CACHE_MAX = 4


def _lru_insert(od, key, value, max_size):
    """Insert `key`->`value` into the OrderedDict `od` used as an LRU: the key
    becomes most-recently-used, and the oldest keys are evicted until at most
    `max_size` remain. Returns the list of evicted keys (newest-eviction last),
    so the caller can log/release them. PURE bookkeeping over the dict — no MLX,
    no model — so the eviction policy is unit-testable in isolation."""
    od[key] = value
    od.move_to_end(key)
    evicted = []
    while len(od) > max_size:
        ev, _ = od.popitem(last=False)
        evicted.append(ev)
    return evicted


# ---------------------------------------------------------------------------
# Multi-resident LOCAL model manager (task #17).
#
# HONESTY FIRST. This keeps MORE THAN ONE local MLX model warm at once SO THE
# LOCAL TIER CAN SWAP INSTANTLY between them (e.g. a small "local-fast" model
# and the capable 4B) WITHOUT a reload — but ONLY when there is RAM to spare.
# Two resident models cost ~2x the RAM of one; the DEFAULT is single-resident
# (warm-set = just the base LLM), which is exactly today's behavior and is the
# safe state on a low-RAM Mac (8GB M1). Multi-resident is OPT-IN via config and
# is RAM-BOUNDED: the manager NEVER loads past the budget, LRU-evicts the
# least-recently-used resident when a new load would exceed it, and if even a
# single configured model does not fit the budget it falls back to
# single-resident. The POLICY here is pure and unit-tested over SYNTHETIC
# sizes; the ACTUAL load + the swap speed benefit are runtime/device-gated and
# are NOT claimed measured anywhere.
#
# This changes nothing about routing SAFETY: it does not touch the consequential
# gate or which TIER is chosen (that is the existing model-swap). It only lets
# the already-chosen LOCAL tier pick among models that are already warm.

# Conservative approximate footprint (GiB of unified memory) used by the policy
# ONLY when a model's size is unknown (not in [models].local_sizes and no
# heuristic match). The 4B-4bit class is ~2.3-2.6GB resident on MLX; this is a
# deliberately generous default so an unknown model is assumed COSTLY (it must
# EARN a warm slot against the budget, never sneak in under-counted).
DEFAULT_LOCAL_MODEL_GIB = 3.0

# Hard floor on the warm-set: the base/primary LLM is ALWAYS a member (it is the
# single-resident fallback and the model the persona KV cache + embeddings are
# built on). The policy guarantees at least this one model stays warm.
LOCAL_WARM_MIN = 1


def estimate_local_model_gib(model_id, sizes=None):
    """Best-effort APPROX resident footprint (GiB) for a local MLX model id,
    used ONLY by the budgeting POLICY (never an allocation). Resolution order:
      1. an explicit override in `sizes` ([models].local_sizes, id -> GiB),
      2. a coarse heuristic on the id (param count x a per-bit-width factor),
      3. DEFAULT_LOCAL_MODEL_GIB when nothing matches.
    This is an ESTIMATE for keep-warm bookkeeping, NOT a measurement and NOT a
    guarantee; the real resident size is device/quant/runtime dependent. Pure
    (no MLX, no load), so the policy is unit-testable over synthetic ids."""
    if sizes and model_id in sizes:
        try:
            g = float(sizes[model_id])
            if g > 0:
                return g
        except (TypeError, ValueError):
            pass  # bad override -> fall through to the heuristic (honest default)
    name = (model_id or "").lower()
    # Param count from a "<n>b" token in the id (e.g. "qwen3-4b" -> 4.0).
    params_b = None
    m = re.search(r"(\d+(?:\.\d+)?)\s*b(?:[-_./]|$|it)", name)
    if m:
        try:
            params_b = float(m.group(1))
        except ValueError:
            params_b = None
    if params_b is None:
        return DEFAULT_LOCAL_MODEL_GIB
    # Bytes per parameter by quantization: 4bit ~0.55 GiB/B, 8bit ~1.1, else
    # (bf16/fp16) ~2.2. The factors include MLX overhead + a small KV margin;
    # they are intentionally a touch high so the budget is respected, not blown.
    if "4bit" in name or "4-bit" in name or "q4" in name:
        per_b = 0.6
    elif "8bit" in name or "8-bit" in name or "q8" in name:
        per_b = 1.15
    else:
        per_b = 2.2
    return round(params_b * per_b, 3)


def plan_warm_set(base_id, configured, budget_gib, sizes=None):
    """PURE keep-warm POLICY. Decide which local models may be kept resident at
    once under a RAM `budget_gib`, given the always-resident `base_id` and the
    operator's `configured` extra warm ids ([models].local_warm). Returns the
    ORDERED list of model ids the manager is ALLOWED to keep warm (base first,
    then the configured extras that fit, in config order).

    Rules (RAM-bounded, single-resident-safe):
      * `base_id` is ALWAYS in the result — it is the single-resident fallback.
      * Each subsequent configured id is admitted ONLY if adding its estimated
        footprint keeps the running total <= budget_gib; otherwise it is
        SKIPPED (it never gets a warm slot). Dedup preserves first position.
      * If `base_id` alone already exceeds the budget, the budget is treated as
        a soft floor for the base ONLY (the base MUST stay warm — it is the
        fallback), and NO extras are admitted: SINGLE-RESIDENT.
    Pure arithmetic over `sizes`/heuristic estimates — no MLX, no load — so the
    budget/admit/single-fallback decisions are unit-testable with synthetic
    sizes. This returns the ALLOWED SET, not what is loaded; actual loading is
    lazy and LRU-bounded by the manager against this same set + budget."""
    plan = [base_id]
    try:
        budget = float(budget_gib)
    except (TypeError, ValueError):
        budget = 0.0
    used = estimate_local_model_gib(base_id, sizes)
    seen = {base_id}
    # base over budget OR a non-positive budget => single-resident (base only).
    if budget <= 0 or used > budget:
        return plan
    for mid in configured or []:
        if not mid or mid in seen:
            continue
        cost = estimate_local_model_gib(mid, sizes)
        if used + cost <= budget:
            plan.append(mid)
            seen.add(mid)
            used += cost
        # else: does not fit -> skipped (NOT warm). Later, smaller models in the
        # list may still fit, so we keep scanning rather than breaking.
    return plan


# ---------------------------------------------------------------------------
# #37 SPECULATIVE DECODING + #39 SELECTABLE QUANTIZATION — PURE helpers.
#
# Both ship OFF/neutral: speculative defaults false (+ no draft model => normal
# generation, today's runtime), quant defaults "auto" (load the model exactly as
# configured, today's behavior). The PURE decisions below are unit-tested over
# synthetic inputs by --selftest; the LIVE seams (the draft-model load, the
# speculative generate, the quant model load) are device/runtime-gated and are
# NEVER exercised here — import-guarded like the VLM/diffusion optional seams, so
# a missing/absent draft or quant variant honestly falls back + reports the path
# that ACTUALLY ran (never claims speculative when normal gen ran, never claims
# int4 when fp16 loaded). The real speedup/RAM/quality effect is device-gated.
# ---------------------------------------------------------------------------

# The quantization values the contract allows. MUST match the daemon's
# config.rs `InferenceConfig::ALLOWED_QUANT`. "auto" is the neutral default:
# load the model as configured (today's behavior, no quant override).
ALLOWED_QUANT = ("auto", "fp16", "int8", "int4")


def validate_quant(quant):
    """Validate a requested quantization string against the allowed set (#39).
    PURE — returns the value unchanged if allowed, raises ValueError with a
    precise message otherwise. Mirrors the daemon's `quant_is_valid` so the two
    sides reject the same values. The caller (config-load seam) turns an invalid
    value into a fall-back-to-"auto" + an honest log; this is the strict gate so
    a bogus quant never reaches the model-load path."""
    if quant in ALLOWED_QUANT:
        return quant
    raise ValueError(
        f"unknown quantization {quant!r}; allowed: {', '.join(ALLOWED_QUANT)}"
    )


def select_quant(requested, available):
    """PURE quant SELECTION + HONEST fallback (#39). Decide which quantization to
    actually load given the `requested` value (validated) and the set/list of
    quants `available` for the configured model on disk. Returns
    `(chosen, requested_honored)`:

      * requested == "auto"            -> ("auto", True): today's behavior — let
        the loader pick the model as configured. No claim about a specific quant.
      * requested in `available`       -> (requested, True): the requested quant
        variant is present; load it.
      * requested NOT in `available`   -> (<first available> or "auto", False):
        the requested variant is NOT present — fall back to an available one (or
        "auto" when nothing is enumerable) and report `requested_honored=False`
        so the caller NEVER claims the requested quant loaded when it did not.

    `available` may be empty/None (the common case: we did not enumerate on-disk
    variants), in which case a non-auto request falls back to "auto" honestly
    rather than fabricating a variant. PURE — no load, no MLX; the real load +
    the RAM/speed tradeoff are device-gated. Unit-tested over synthetic inputs."""
    validate_quant(requested)
    if requested == "auto":
        return ("auto", True)
    avail = list(available or [])
    if requested in avail:
        return (requested, True)
    # Requested variant absent: fall back HONESTLY. Prefer a concrete available
    # quant (deterministic: first in the allowed order that is available); else
    # "auto" (let the loader pick what is actually on disk).
    for q in ALLOWED_QUANT:
        if q != "auto" and q in avail:
            return (q, False)
    return ("auto", False)


def quant_variant_id(base_id, quant):
    """PURE: derive the model id to TRY for a requested non-"auto" quant (#39).
    MLX community checkpoints encode the quant in the id suffix (…-4bit / …-8bit
    / …-bf16). Given the configured `base_id` and a validated `quant`, return the
    candidate id whose quant suffix is rewritten to the request:

      * quant == "auto"  -> base_id unchanged (today's behavior: load as-is).
      * quant int4/int8  -> swap a trailing -4bit/-8bit suffix (e.g.
        "…-4bit" with quant=int8 -> "…-8bit"); if the base has no recognized
        quant suffix, APPEND one ("…-8bit").
      * quant fp16       -> swap to/append "-bf16" (the MLX half-precision id).

    This is a best-effort id derivation, NOT a guarantee the variant exists — the
    LOAD seam tries this id and falls back to the base id (recording the quant
    that ACTUALLY loaded) if it is absent. PURE string work; no MLX, no load."""
    if quant == "auto":
        return base_id
    target = {"int4": "4bit", "int8": "8bit", "fp16": "bf16"}.get(quant)
    if not target:
        return base_id
    # Strip a recognized trailing quant suffix, then append the target.
    stripped = re.sub(r"[-_](?:4bit|8bit|4-bit|8-bit|bf16|fp16|q4|q8)$", "", base_id, flags=re.IGNORECASE)
    return f"{stripped}-{target}"


def should_use_speculative(speculative_on, draft_model, draft_available):
    """PURE decision for the generate path (#37): use speculative/draft decoding
    THIS turn iff ALL of:
      * `speculative_on` ([inference].speculative is true — the master gate), AND
      * `draft_model` is a non-empty configured id, AND
      * `draft_available` is True (the draft checkpoint + mlx_lm's speculative
        support actually loaded — the import/availability guard's verdict).

    Returns True only when speculative decoding will REALLY run; otherwise False
    => NORMAL generation (today's runtime). The caller reports the path that
    ACTUALLY ran from this same decision, so a turned-on-but-unavailable draft
    honestly reports speculative=False (NEVER fakes speculative). PURE — no MLX,
    no load; the real speedup is device/model-dependent and only on-device."""
    return bool(speculative_on) and bool(draft_model) and bool(draft_available)


def metrics_from_response(resp, path=None):
    """Extract the HONEST decode telemetry mlx_lm attaches to the LAST
    GenerationResponse of a stream_generate run (accel/benchmark-foundation).

    mlx_lm's stream_generate yields a GenerationResponse per token whose final
    value carries measured, real fields the server used to DISCARD (it read only
    `.text`): prefill/decode tok/s and running peak GPU memory. This returns
    those fields as a plain dict, or None when `resp` is falsy (no token was ever
    produced — e.g. an immediately-cancelled decode). Only fields mlx_lm actually
    reports are surfaced — nothing here is estimated, extrapolated, or fabricated.

      * prompt_tps          — prefill throughput (tokens/sec) mlx_lm measured
      * generation_tps      — DECODE throughput (tokens/sec) mlx_lm measured
      * peak_memory_gb      — running peak GPU memory (GB) per mx.get_peak_memory
      * prompt_tokens       — prompt tokens actually prefilled this call
      * generation_tokens   — tokens actually decoded this call
      * finish_reason       — mlx_lm's own stop reason ("stop"/"length"/None)
      * path (optional)     — which server decode path produced this (label only)

    PURE: reads attributes off the response object, no MLX call, no I/O."""
    if resp is None:
        return None
    m = {
        "prompt_tps": getattr(resp, "prompt_tps", None),
        "generation_tps": getattr(resp, "generation_tps", None),
        "peak_memory_gb": getattr(resp, "peak_memory", None),
        "prompt_tokens": getattr(resp, "prompt_tokens", None),
        "generation_tokens": getattr(resp, "generation_tokens", None),
        "finish_reason": getattr(resp, "finish_reason", None),
    }
    if path is not None:
        m["path"] = path
    return m


class LocalWarmManager:
    """Keeps up to N local MLX models WARM under a RAM budget so the Local tier
    can swap between them instantly. The class is PURE bookkeeping over the
    policy (plan_warm_set + an LRU of resident ids); it NEVER imports or loads
    MLX itself — `select()` takes a `loader(model_id)` callback that the engine
    supplies (the real MLX `load`), so every keep-warm / evict / select /
    single-fallback decision is unit-testable with a synthetic loader and
    synthetic sizes.

    Contract used by the daemon's Local tier (via the engine's generate/converse
    op): pass a requested local `model_id`; the manager returns the resident
    (model, tokenizer) for it — loading + keeping it warm if it is in the
    allowed warm-set and the budget permits, else transparently falling back to
    the base/single-resident model. An ABSENT model (loader raises) or a model
    OUTSIDE the warm-set returns the base — NEVER a crash.

    `base_id` is the always-resident primary LLM; `select(None)` or
    `select(base_id)` returns it. Caller holds the engine GPU lock; this class
    does no locking of its own (it is driven entirely under that lock)."""

    def __init__(self, base_id, configured=None, budget_gib=0.0, sizes=None):
        self.base_id = base_id
        self.configured = list(configured or [])
        self.budget_gib = budget_gib
        self.sizes = dict(sizes or {})
        # The ALLOWED warm-set under the budget (base first). Anything not in
        # here is never kept warm; a request for it falls back to the base.
        self.plan = plan_warm_set(base_id, self.configured, budget_gib, self.sizes)
        self.allowed = set(self.plan)
        # How many distinct models may be RESIDENT at once. The plan already
        # encodes the budget; capacity is just its length (>= LOCAL_WARM_MIN).
        self.capacity = max(len(self.plan), LOCAL_WARM_MIN)
        # id -> (model, tokenizer), used as an LRU (move_to_end on touch). The
        # base, once loaded, is PINNED: it is never evicted (it is the fallback
        # + the persona-cache/embedding model). Non-base residents LRU-evict.
        from collections import OrderedDict as _OrderedDict

        self.resident = _OrderedDict()

    def multi_resident(self):
        """True iff the policy admitted >1 model (i.e. instant local swap is
        possible). False => single-resident (the safe low-RAM default)."""
        return len(self.plan) > 1

    def warm_ids(self):
        """The model ids currently RESIDENT (loaded), base first. Honest view
        for the HUD indicator: what is actually warm right now, not the plan."""
        ids = []
        if self.base_id in self.resident:
            ids.append(self.base_id)
        ids += [k for k in self.resident if k != self.base_id]
        return ids

    def resolve_id(self, model_id):
        """Map a requested local model id to the id that will actually answer:
        the request itself if it is in the allowed warm-set, else the base
        (single-resident fallback). PURE — no load. None/"" -> base."""
        if model_id and model_id in self.allowed:
            return model_id
        return self.base_id

    def select(self, model_id, loader):
        """Return (resolved_id, model, tokenizer) for the requested local model,
        keeping it warm under the budget. `loader(id) -> (model, tokenizer)` is
        the engine's real lazy MLX load (or a synthetic stub in tests).

        Flow: resolve the id against the allowed warm-set (out-of-set -> base);
        return it if already resident; otherwise load it, LRU-EVICT non-base
        residents until at most `capacity` remain, and pin/insert it. If loading
        a NON-base model RAISES (absent checkpoint / OOM), fall back to the base
        (loading the base if needed) — never propagate the failure to the op."""
        resolved = self.resolve_id(model_id)
        hit = self.resident.get(resolved)
        if hit is not None:
            self.resident.move_to_end(resolved)
            return (resolved, hit[0], hit[1])
        try:
            model, tokenizer = loader(resolved)
        except Exception:
            if resolved == self.base_id:
                raise  # base load failing is fatal to the op anyway; surface it.
            log.exception(
                "local model %s could not be loaded; falling back to base %s",
                resolved,
                self.base_id,
            )
            return self.select(self.base_id, loader)
        self._insert(resolved, model, tokenizer)
        return (resolved, model, tokenizer)

    def _insert(self, model_id, model, tokenizer):
        """Insert a freshly loaded resident and LRU-evict non-base residents
        until at most `capacity` remain. The base is PINNED (never evicted).
        Returns the list of evicted ids (so the engine can drop their caches).

        RAM NOTE — bounded +1 transient: the caller (`select`) loads the new model
        object BEFORE calling this, so during a swap from a full warm-set RAM briefly
        holds capacity+1 models until the eviction loop below drops the LRU victim.
        This load-then-evict ordering is inherent to any keep-warm LRU (you must
        materialize the replacement before discarding the old one) and it all runs
        synchronously under the GPU lock in `_ensure_local_llm` before generation, so
        it is a momentary in-flight peak, NOT a sustained leak: the STEADY-STATE
        warm-set always respects the budget. The "never more than the budget" bound
        therefore holds in steady state, not during the atomic in-flight swap. For a
        RAM-tight Mac, set the budget with one-model headroom (or stay single-resident,
        the default) so even the transient stays inside physical RAM."""
        self.resident[model_id] = (model, tokenizer)
        self.resident.move_to_end(model_id)
        evicted = []
        # Evict the oldest NON-base resident until within capacity.
        while len(self.resident) > self.capacity:
            victim = None
            for k in self.resident:  # oldest-first
                if k != self.base_id:
                    victim = k
                    break
            if victim is None:
                break  # only the base remains -> nothing evictable, stop.
            del self.resident[victim]
            evicted.append(victim)
        if evicted:
            log.info("local warm-set evicted %s (capacity %d)", evicted, self.capacity)
        return evicted


# Silence trim applied to every synthesized WAV (speak AND converse): samples
# with |amplitude| < TRIM_AMPLITUDE are trimmed from both ends, keeping
# TRIM_PAD_MS of padding each side. Kokoro bakes ~0.26s leading and ~0.46s
# trailing silence into every utterance; trimmed, the residual lead/tail are
# each <= 120ms (60ms pad + the sub-threshold fade).
TRIM_AMPLITUDE = 0.005
TRIM_PAD_MS = 60

# Linear edge fades applied AFTER the silence trim, on the float array just
# before int16 conversion: fade-in over the first FADE_IN_SAMPLES (5ms @24kHz)
# and fade-out over the last FADE_OUT_SAMPLES (10ms @24kHz). The trim's hard
# cut at the threshold used to leave a step at the clip edges that played as
# a click where the daemon joins clips; the fade ramps pin the first and last
# samples of every emitted WAV (speak, converse sentences, openers, audition
# samples — they all funnel through _write_wav) to exactly 0, guaranteeing
# |amp| < 0.002 at both edges.
FADE_IN_SAMPLES = 120
FADE_OUT_SAMPLES = 240

# Sampling for the persona-voiced generate op ONLY. Greedy decoding (mlx_lm's
# default, temp 0) is deterministic: identical requests produce byte-identical
# replies, which breaks the persona contract ("vary your phrasing naturally;
# never answer similar requests with identical wording"). classify and
# extract_facts stay greedy — they emit strict JSON, where determinism is a
# feature. See InferenceEngine._persona_sampler for why this needs an explicit
# PRNG key instead of mlx_lm's make_sampler.
#
# Temp bump 0.6 -> 0.85 (persona part B): at 0.6 short turns collapsed onto a
# couple of stock phrasings — five "Hi DARWIN" samples returned the same two
# task-acknowledgement sentences ("Right away, sir. System online. Standing
# by."), which reads as a console, not a person. 0.85 (with top-p 0.95 holding
# the tail in check) gives genuinely distinct greetings each call while the
# grounding rules in persona.txt keep specifics from drifting — measured: no
# invented weather/schedule/numbers across the greeting + status + data probes.
GENERATE_TEMP = 0.85
GENERATE_TOP_P = 0.95

# Appended to the final user turn of a converse request that carries
# "opener_spoken": the daemon already played that opener line from the bank,
# so the model must continue past it instead of acknowledging again. The
# persona's short-opener-first rule intentionally loses to this note.
OPENER_SPOKEN_NOTE = (
    "[You already began your reply aloud with: '{opener}'. Continue naturally "
    "from there - do not repeat any acknowledgement or greeting; go directly "
    "to the substance.]"
)

# JSONL line limit. The asyncio default (64 KiB) is too small for generate
# requests carrying long history exchanges (cloud replies can run ~16 KB of
# JSON-escaped text each) plus facts and a data blob.
STREAM_LIMIT = 8 * 1024 * 1024

# Sesame CSM speaker prompts. mlx-audio's built-in voice= path fetches
# prompts/<voice>.wav from the GATED upstream sesame/csm-1b repo (401 without
# HF auth — measured 2026-06-12), so _synth_csm fetches the identical prompt
# from the ungated mlx-community mirror and passes it as ref_audio/ref_text.
# Transcripts are copied verbatim from mlx_audio.tts.models.sesame's
# SPEAKER_PROMPTS (the mirror carries no .txt captions).
CSM_PROMPT_REPO = "mlx-community/csm-1b"
CSM_PROMPT_TEXTS = {
    "conversational_a": (
        "like revising for an exam I'd have to try and like keep up the momentum because I'd "
        "start really early I'd be like okay I'm gonna start revising now and then like "
        "you're revising for ages and then I just like start losing steam I didn't do that "
        "for the exam we had recently to be fair that was a more of a last minute scenario "
        "but like yeah I'm trying to like yeah I noticed this yesterday that like Mondays I "
        "sort of start the day with this not like a panic but like a"
    ),
    "conversational_b": (
        "like a super Mario level. Like it's very like high detail. And like, once you get "
        "into the park, it just like, everything looks like a computer game and they have all "
        "these, like, you know, if, if there's like a, you know, like in a Mario game, they "
        "will have like a question block. And if you like, you know, punch it, a coin will "
        "come out. So like everyone, when they come into the park, they get like this little "
        "bracelet and then you can go punching question blocks around."
    ),
}

# Marker spliced into the classifier template to find the static-prefix /
# per-utterance boundary after chat templating. Never appears in real text.
_UTTERANCE_SENTINEL = "\x00<utterance>\x00"

# Valid Kokoro language codes (first letter of the voice name).
_KOKORO_LANGS = frozenset("abefhijpz")

# Orpheus 3B ft named speakers (upstream canopylabs voice set; mlx-audio
# prepends the voice verbatim as a "<voice>: " prompt tag with NO validation,
# so an unknown voice silently produces an arbitrary/garbled speaker).
ORPHEUS_VOICES = frozenset({"tara", "leah", "jess", "leo", "dan", "mia", "zac", "zoe"})

# Returned when the classifier output cannot be parsed/validated; confidence
# 0.3 is below the router's cloud_confidence_threshold (0.6) and complexity
# "heavy" both force escalation to cloud.
CLASSIFY_FALLBACK = {"intent": "conversation", "confidence": 0.3, "complexity": "heavy", "args": {}}

log = logging.getLogger("darwin.inference")


def setup_logging():
    LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
    fmt = logging.Formatter("%(asctime)s %(levelname)s %(name)s %(message)s")
    file_handler = logging.FileHandler(LOG_PATH)
    file_handler.setFormatter(fmt)
    stderr_handler = logging.StreamHandler(sys.stderr)
    stderr_handler.setFormatter(fmt)
    log.setLevel(logging.INFO)
    log.addHandler(file_handler)
    log.addHandler(stderr_handler)


def load_config():
    """Read settings from config/darwin.toml with contract defaults.

    Each key is validated independently: one bad value (e.g. speed = "fast")
    keeps the rest of the file applied, and the log names exactly which keys
    fell back — no half-applied dict misreported as full fallback.
    """
    settings = {
        "llm": DEFAULT_LLM,
        "stt": DEFAULT_STT,
        "classifier": DEFAULT_CLASSIFIER,
        # OPTIONAL on-device VLM checkpoint (op=describe_image). Default id is
        # only the repo we'd load IF present; the op is gated on the model
        # actually being downloaded, so this never forces a download.
        "vlm": DEFAULT_VLM,
        # OPTIONAL on-device text->image diffusion checkpoint (op=generate_image).
        # Like the VLM this default id is only what we'd load IF present; the op is
        # gated on availability so it never forces a download, and an empty/unset
        # [image].model still resolves to DEFAULT_IMAGE_MODEL (coerced below).
        "image_model": DEFAULT_IMAGE_MODEL,
        "engine": DEFAULT_TTS_ENGINE,
        # "" = resolve from TTS_ENGINE_DEFAULTS for the active engine; the
        # engine knows its own default repo/voice, so the empty string keeps
        # an engine switch from dragging a stale kokoro voice along.
        "tts_model": "",
        "voice": "",
        "speed": DEFAULT_SPEED,
        "openers": list(DEFAULT_OPENERS),
        "preload": True,
        # Multi-resident LOCAL warm-set (task #17). DEFAULT is SINGLE-RESIDENT:
        # an EMPTY extra warm-set + a 0 budget == today's behavior (only the
        # base [models].llm is kept warm). Multi-resident is OPT-IN and
        # RAM-bounded — see [models].local_warm / local_budget_gib / local_sizes
        # in config/darwin.toml. Conservative on purpose: a low-RAM Mac left at
        # the defaults is unaffected.
        "local_warm": [],
        "local_budget_gib": 0.0,
        "local_sizes": {},
        # #37 SPECULATIVE DECODING (OFF/neutral default): speculative gated false
        # + no draft model => NORMAL generation, today's exact runtime. A draft
        # model is the small checkpoint mlx_lm uses to propose tokens; absent it
        # (or unloadable) the generate path honestly falls back to normal gen.
        "speculative": False,
        "draft_model": "",
        # #39 SELECTABLE QUANTIZATION (neutral default): "auto" == today's
        # behavior (load the model as configured). An explicit value selects a
        # matching variant at load; if absent the loader falls back + reports the
        # quant that ACTUALLY loaded. Validated below (unknown -> "auto").
        "quant": "auto",
        # op=embed BACKEND. DEFAULT = the Core ML bge embedder (faster AND higher
        # retrieval quality — the SOTA choice). HONEST FALLBACK to the mean-pool
        # path if it cannot be built/loaded. Validated below (unknown ->
        # DEFAULT_EMBEDDER + warn). The Python server is the sole VALUE validator;
        # the daemon only lists the key in config.rs KNOWN_KEYS (typo detection).
        "embedder": DEFAULT_EMBEDDER,
        # op=rerank second stage (SHIPS ON — the measured winner). Gates whether the
        # server WARMS the Core ML cross-encoder at preload; the daemon reads the
        # same [inference].reranker key to decide whether to CALL op=rerank. HONEST
        # FALLBACK to dense order if the model can't build/load (fell_back on the
        # wire). A non-boolean value keeps the default (parsed below like preload).
        "reranker": DEFAULT_RERANKER_ENABLED,
    }
    try:
        import tomllib  # stdlib on 3.11+

        with open(CONFIG_PATH, "rb") as f:
            cfg = tomllib.load(f)
    except FileNotFoundError:
        log.warning("config %s not found; using hardcoded contract defaults", CONFIG_PATH)
        return settings
    except Exception:
        log.exception("failed to parse %s; using hardcoded contract defaults", CONFIG_PATH)
        return settings

    models = cfg.get("models", {})
    speech = cfg.get("speech", {})
    inference = cfg.get("inference", {})
    image = cfg.get("image", {})
    sources = (
        ("llm", models, "llm", str),
        ("stt", models, "stt", str),
        ("classifier", models, "classifier", str),
        ("vlm", models, "vlm", str),
        ("engine", speech, "engine", str),
        ("tts_model", speech, "model", str),
        ("voice", speech, "voice", str),
        ("speed", speech, "speed", float),
        ("preload", inference, "preload", bool),
        # #37 SPECULATIVE DECODING (OFF/neutral): the master gate + the draft id.
        ("speculative", inference, "speculative", bool),
        ("draft_model", inference, "draft_model", str),
        # RERANK second stage (SHIPS ON): warm/gate the Core ML cross-encoder.
        ("reranker", inference, "reranker", bool),
        # [image].model — the operator's on-device diffusion id. Empty/unset is
        # coerced to DEFAULT_IMAGE_MODEL after the loop; the op still reports
        # image_model_unavailable when the package/weights are absent.
        ("image_model", image, "model", str),
    )
    for key, section, cfg_key, conv in sources:
        if not isinstance(section, dict) or cfg_key not in section:
            continue
        raw = section[cfg_key]
        if conv is bool:
            if isinstance(raw, bool):
                settings[key] = raw
            else:
                log.warning("config key %r has non-boolean value %r; keeping default %r", key, raw, settings[key])
            continue
        if isinstance(raw, bool):
            # A TOML boolean is never a valid str/float value, but float(True)
            # == 1.0 and str(True) == "True" would both convert silently
            # (audit fix: speed = true used to become 1.0 with no warning).
            log.warning(
                "config key %r has boolean value %r where %s is expected; keeping default %r",
                key,
                raw,
                conv.__name__,
                settings[key],
            )
            continue
        try:
            value = conv(raw)
        except (TypeError, ValueError):
            log.warning("config key %r has invalid value %r; keeping default %r", key, raw, settings[key])
            continue
        if key == "speed" and not (SPEED_MIN <= value <= SPEED_MAX):
            # Out-of-range speed reaches every kokoro synthesis (opener bank
            # included); see SPEED_MIN/SPEED_MAX (audit fix).
            log.warning(
                "config key 'speed' = %r is outside the supported range [%s, %s]; keeping default %r",
                value,
                SPEED_MIN,
                SPEED_MAX,
                settings[key],
            )
            continue
        settings[key] = value
    # [image].model: an explicit EMPTY value resolves to the canonical default
    # (image generation has a real default model, unlike draft_model which is
    # legitimately empty=off). The op still reports image_model_unavailable when
    # the diffusion package/weights are absent — this only picks the id.
    if not str(settings["image_model"]).strip():
        settings["image_model"] = DEFAULT_IMAGE_MODEL
    # [speech].openers: a non-empty list of non-empty strings, validated as a
    # whole (a half-applied opener bank would desync the daemon's filename
    # index -> opener text mapping). An INVALID list disables the bank
    # entirely — zero WAVs — instead of substituting DEFAULT_OPENERS (audit
    # fix): the daemon maps WAV filename indexes onto ITS OWN parsed openers
    # list, so a defaults substitution here while the daemon keeps the custom
    # list makes the model "continue" from an opener line the user never
    # heard. A missing bank degrades gracefully daemon-side (no opener fires,
    # no opener_spoken is sent); a mismapped one does not.
    if isinstance(speech, dict) and "openers" in speech:
        raw = speech["openers"]
        if (
            isinstance(raw, list)
            and raw
            and all(isinstance(s, str) and s.strip() for s in raw)
        ):
            settings["openers"] = [s.strip() for s in raw]
        else:
            log.warning(
                "config key 'openers' must be a non-empty list of non-empty strings; "
                "got %r; DISABLING the opener bank (a defaults substitution would "
                "desync the daemon's index -> text mapping)",
                raw,
            )
            settings["openers"] = []

    # [models].local_warm: OPTIONAL list of EXTRA local model ids to keep warm
    # alongside the base [models].llm for instant Local-tier swap. Validated as
    # a whole list of non-empty strings; any invalid shape DISABLES the extra
    # warm-set (single-resident) rather than half-applying it — the conservative,
    # honest default. The base llm is always warm regardless; this is purely the
    # EXTRA set, and it is still RAM-bounded by local_budget_gib below.
    if isinstance(models, dict) and "local_warm" in models:
        raw = models["local_warm"]
        if isinstance(raw, list) and all(isinstance(s, str) and s.strip() for s in raw):
            settings["local_warm"] = [s.strip() for s in raw]
        else:
            log.warning(
                "config key [models].local_warm must be a list of non-empty strings; "
                "got %r; keeping SINGLE-RESIDENT (no extra warm models)",
                raw,
            )
            settings["local_warm"] = []

    # [models].local_budget_gib: RAM budget (GiB) the warm-set may occupy. 0
    # (the default) or any non-positive/invalid value means SINGLE-RESIDENT —
    # the manager admits only the base. A positive budget lets the policy admit
    # extras up to that total (estimated footprint). Conservative by default so
    # an unconfigured low-RAM Mac never keeps two models warm.
    if isinstance(models, dict) and "local_budget_gib" in models:
        raw = models["local_budget_gib"]
        if isinstance(raw, bool) or not isinstance(raw, (int, float)):
            log.warning(
                "config key [models].local_budget_gib must be a number; got %r; "
                "keeping SINGLE-RESIDENT (budget 0)",
                raw,
            )
        elif raw < 0:
            log.warning(
                "config key [models].local_budget_gib must be >= 0; got %r; "
                "keeping SINGLE-RESIDENT (budget 0)",
                raw,
            )
        else:
            settings["local_budget_gib"] = float(raw)

    # [models].local_sizes: OPTIONAL id -> approx resident GiB overrides for the
    # budgeting policy (used when the heuristic would mis-estimate a model). Each
    # entry must map a non-empty string id to a positive number; bad entries are
    # dropped individually (the heuristic still covers them) so one typo never
    # disables the whole table.
    if isinstance(models, dict) and "local_sizes" in models:
        raw = models["local_sizes"]
        if isinstance(raw, dict):
            sizes = {}
            for k, v in raw.items():
                if (
                    isinstance(k, str)
                    and k.strip()
                    and not isinstance(v, bool)
                    and isinstance(v, (int, float))
                    and v > 0
                ):
                    sizes[k.strip()] = float(v)
                else:
                    log.warning(
                        "config [models].local_sizes entry %r=%r is invalid "
                        "(need non-empty id -> positive number); ignoring it",
                        k,
                        v,
                    )
            settings["local_sizes"] = sizes
        else:
            log.warning(
                "config key [models].local_sizes must be a table; got %r; ignoring it",
                raw,
            )

    # #39 SELECTABLE QUANTIZATION: [inference].quant must be one of ALLOWED_QUANT
    # ("auto" default == today's behavior). An unknown value is a misconfiguration
    # — keep the NEUTRAL "auto" default + warn rather than pass a bogus value to
    # the model-load path. Mirrors the daemon's config.rs quant validation so the
    # two sides reject the same values.
    if isinstance(inference, dict) and "quant" in inference:
        raw = inference["quant"]
        if isinstance(raw, str):
            try:
                settings["quant"] = validate_quant(raw.strip())
            except ValueError:
                log.warning(
                    "config key [inference].quant = %r is not one of %s; keeping \"auto\"",
                    raw,
                    ", ".join(ALLOWED_QUANT),
                )
        else:
            log.warning(
                "config key [inference].quant must be a string; got %r; keeping \"auto\"",
                raw,
            )

    # op=embed BACKEND: [inference].embedder must be one of ALLOWED_EMBEDDERS
    # (default = the Core ML bge embedder). An unknown value is a misconfiguration
    # — keep the DEFAULT + warn rather than pass a bogus id. The Python server is
    # the sole VALUE validator here; the daemon does not value-validate this key
    # (it only lists it in config.rs KNOWN_KEYS so a typo under [inference] is
    # diagnosed).
    if isinstance(inference, dict) and "embedder" in inference:
        raw = inference["embedder"]
        if isinstance(raw, str):
            try:
                settings["embedder"] = validate_embedder(raw.strip())
            except ValueError:
                log.warning(
                    "config key [inference].embedder = %r is not one of %s; keeping %r",
                    raw,
                    ", ".join(ALLOWED_EMBEDDERS),
                    DEFAULT_EMBEDDER,
                )
        else:
            log.warning(
                "config key [inference].embedder must be a string; got %r; keeping %r",
                raw,
                DEFAULT_EMBEDDER,
            )

    return settings


def load_classifier_template():
    try:
        return PROMPT_PATH.read_text(encoding="utf-8")
    except OSError:
        log.exception("could not read %s; using built-in minimal classifier prompt", PROMPT_PATH)
        return (
            "You are the DARWIN intent classifier. Given one user utterance, "
            'return ONLY a JSON object {"intent": "...", "confidence": 0.0, '
            '"complexity": "light"|"heavy", "args": {}}.\n\n'
            'Utterance: "{utterance}"\nJSON:'
        )


def load_persona():
    try:
        return PERSONA_PATH.read_text(encoding="utf-8").strip()
    except OSError:
        log.exception("could not read %s; generate will run without persona", PERSONA_PATH)
        return ""


# System instruction for op=extract_facts. Kept out of the persona: this is a
# structured-extraction task, not a voiced reply.
EXTRACT_FACTS_SYSTEM = (
    "You extract durable facts about the user from one voice-assistant exchange. "
    "Reply with ONLY a JSON array — no prose, no code fences — of at most 3 objects, "
    'each shaped {"key": "...", "value": "..."}. Keys are namespaced: user.name, '
    "user.preference.<topic>, user.project.<name>, context.<topic>.\n"
    "Grounding rules (non-negotiable):\n"
    "- Extract ONLY facts the USER explicitly stated in their own words.\n"
    "- The assistant's reply is context for disambiguation ONLY. It is NEVER a source "
    "of facts: anything that appears only in the assistant's reply — names, numbers, "
    "preferences, rooms, devices, temperatures — must be ignored entirely, even if it "
    "sounds factual.\n"
    "- Record only durable information worth remembering across conversations: the "
    "user's name, preferences, projects, hardware, recurring goals or habits.\n"
    "- Trivia, one-off commands, simple questions, greetings, and anything about the "
    "assistant itself do NOT qualify.\n"
    "- Corrections count: when the user contradicts or corrects an earlier fact "
    "(\"actually my name is...\", \"no, I prefer...\"), extract the CORRECTED fact — "
    "same key, new value. The correction is the durable fact, not the stale one.\n"
    "- Be conservative: when in doubt, extract nothing. If the user stated nothing "
    "durable, reply with exactly [].\n"
    "Example — user says \"I'm Alice and I prefer metric units\": reply "
    '[{"key": "user.name", "value": "Alice"}, '
    '{"key": "user.preference.units", "value": "metric"}]. '
    "Example — user says \"Actually my name is Dar, not Darwin\": reply "
    '[{"key": "user.name", "value": "Dar"}]. '
    "Example — user says \"how's it going?\" and the assistant's reply mentions "
    "temperatures or rooms: reply []."
)


# System instruction for op=consolidate (self-learning reflection). Greedy
# decode + strict JSON: determinism is a feature here, exactly like classify
# and extract_facts. The op is deliberately conservative — the parse layer
# (parse_consolidation) additionally enforces namespaced keys, the
# deletes-must-exist rule, the meta.-key exclusion and the total change cap.
CONSOLIDATE_SYSTEM = (
    "You are the memory-consolidation pass of a voice assistant. You receive the "
    "assistant's stored facts about the user and recent conversation transcripts "
    "(oldest first). Reply with ONLY a JSON object — no prose, no code fences — "
    'shaped {"upserts": [{"key": "...", "value": "..."}], "deletes": ["..."]}.\n'
    "Rules (non-negotiable):\n"
    "- Merge duplicate or overlapping facts into one: upsert the surviving fact if "
    "its value needs updating, and delete the redundant keys. Exactly ONE key must "
    "survive a merge — never delete every copy of a duplicated fact.\n"
    "- On conflicting values for the same thing, prefer the NEWEST phrasing: later "
    "transcript statements override earlier ones and override stored facts.\n"
    "- DELETE facts that a later user statement contradicts or corrects (unless the "
    "same key is being upserted with the corrected value — then just upsert).\n"
    "- Generalize only clearly recurring patterns (something the user did or asked "
    "for repeatedly across exchanges); never generalize from a single mention.\n"
    "- Keys are namespaced (user.name, user.preference.<topic>, user.project.<name>, "
    "user.habit.<slug>, context.<topic>). Deletes may ONLY name keys that appear in "
    "the stored facts list — never invent a key to delete.\n"
    "- Never invent facts: every upsert must be supported by the stored facts or by "
    "what the USER said in the transcripts.\n"
    "- Mine habits: when the user makes the SAME kind of request or statement at "
    "least THREE separate times across the provided transcripts, upsert ONE fact "
    "under user.habit.<slug> (lowercase snake_case slug) whose value is a short "
    "plain description of the recurring pattern, e.g. "
    'user.habit.morning_status_check = "asks for a system status most mornings". '
    "The three occurrences do not need identical wording — three differently "
    "phrased requests for the same thing (\"give me a system status\" / \"system "
    "status please\" / \"run a status check\") ARE the same kind of request and "
    "must be mined.\n"
    "- When the user's own lines make the habit's timing evident (\"every "
    "morning\", \"most mornings\", \"again this evening\"), include that "
    "time-of-day word — morning, afternoon or evening — in the habit value, "
    "e.g. \"asks for a system status most mornings\". Never state a time of "
    "day the user's lines do not show.\n"
    "- Habit evidence comes ONLY from lines that start with \"User:\". Lines that "
    "start with \"Assistant:\" are NEVER evidence — even when the assistant "
    "explicitly claims the user does something regularly, that claim alone "
    "produces NO habit fact. Fewer than three User: occurrences in these "
    "transcripts means NO habit fact, and a habit is never inferred from stored "
    "facts alone.\n"
    "- Touch as little as possible: do NOT upsert facts whose value is unchanged, and "
    "do NOT delete a key you are upserting.\n"
    "- Output shape is strict: every upsert is an object with exactly the two fields "
    '"key" and "value"; every delete is the bare key string only (never "key = value").\n'
    "- Be conservative: when unsure, change nothing. If the stored facts are already "
    'clean and consistent, reply with exactly {"upserts": [], "deletes": []}.\n'
    "Example — stored facts user.name = Alice, user.preference.coffee = espresso, "
    "user.preference.coffee_drink = espresso; in a later transcript the user says "
    '"actually, it\'s Alicia": reply {"upserts": [{"key": "user.name", "value": '
    '"Alicia"}], "deletes": ["user.preference.coffee_drink"]} — the name takes the '
    "newest value, the duplicated preference collapses to one key, and the unchanged "
    "espresso fact is not touched. "
    "Example — the User: lines include \"play some jazz\", \"put on jazz please\" "
    "and \"jazz again, thanks\" (three separate user requests for the same thing): "
    'reply {"upserts": [{"key": "user.habit.jazz_listening", "value": '
    '"often asks for jazz"}], "deletes": []}. '
    "Example — only the Assistant: lines mention that the user usually asks for a "
    "morning status check, while the User: lines never actually ask for one: reply "
    '{"upserts": [], "deletes": []} — assistant claims are never habit evidence.'
)


def _is_ascii_digit(ch):
    return "0" <= ch <= "9"


def _speakable(text):
    """Make written forms speakable for TTS before synthesis. Today: spoken
    URLs/domains — a '.' between a word char and a 2+-letter segment ("apple.com",
    "github.io", "npr.org") becomes " dot " so Kokoro says "apple dot com", not
    "apple, com" (it otherwise reads the period as a clause pause). Conservative —
    left untouched: a digit after the dot (a decimal 3.14 / v2.0), a space after
    it (a sentence end, "Mr. Smith"), and single-letter segments ("e.g.").
    Applied at the single synth point so EVERY path (speak, converse sentences,
    openers) gets it."""
    out = []
    n = len(text)
    for i, c in enumerate(text):
        if c == "." and i > 0 and text[i - 1].isalnum():
            j = i + 1
            letters = 0
            while j < n and text[j].isascii() and text[j].isalpha():
                j += 1
                letters += 1
            if letters >= 2:
                out.append(" dot ")
                continue
        out.append(c)
    return "".join(out)


def _split_complete_sentences(buffer, final=False):
    """Incremental, decimal-aware sentence splitter matching the daemon's
    split_sentences semantics exactly: boundaries at . ! ? and newline,
    except a '.' between two ASCII digits (a decimal point like "6.5").

    Returns (sentences, remainder). Streaming twist: a '.' that is the very
    last character of the buffer with an ASCII digit before it is held back
    (remainder) unless final=True — the next decoded token may begin with a
    digit, which would make it a decimal point. With final=True the
    remainder is always flushed as a last sentence (matching the daemon's
    tail handling) and returned remainder is ''."""
    sentences = []
    start = 0
    n = len(buffer)
    for i, c in enumerate(buffer):
        if c not in ".!?\n":
            continue
        if c == "." and i > 0 and _is_ascii_digit(buffer[i - 1]):
            if i + 1 < n and _is_ascii_digit(buffer[i + 1]):
                continue  # decimal point, not a boundary
            if i + 1 == n and not final:
                continue  # possible decimal split across tokens; hold back
        # Domain dot ("apple.com"): a '.' between a word char and an ASCII LETTER
        # is part of a URL, not a sentence end — keep "apple.com" in one sentence
        # so it isn't synthesized as "apple." + "com" (which reads as "apple, com").
        # `_speakable` then turns it into "apple dot com" at synth time.
        if (
            c == "."
            and i > 0
            and buffer[i - 1].isalnum()
            and i + 1 < n
            and buffer[i + 1].isascii()
            and buffer[i + 1].isalpha()
        ):
            continue
        sentence = buffer[start : i + 1].strip()
        if sentence:
            sentences.append(sentence)
        start = i + 1
    remainder = buffer[start:]
    if final:
        tail = remainder.strip()
        if tail:
            sentences.append(tail)
        remainder = ""
    return sentences, remainder


class InferenceEngine:
    """Lazy-loading, resident MLX models. All methods are blocking; callers
    run them in a worker thread. A lock serializes GPU work."""

    def __init__(self, settings, classifier_template, persona):
        self.llm_id = settings["llm"]
        self.stt_id = settings["stt"]
        self.classifier_id = settings.get("classifier", DEFAULT_CLASSIFIER)
        # OPTIONAL on-device vision-language model id (op=describe_image). Empty
        # string disables the op entirely (honest "unavailable"); a non-empty id
        # is the checkpoint mlx-vlm will lazy-load on first use. Ships OFF: the
        # daemon's [vision] gate + the multi-GB download are what turn it on.
        self.vlm_id = settings.get("vlm", DEFAULT_VLM)
        # OPTIONAL on-device text->image model id (op=generate_image). Empty
        # string disables the op entirely (honest "unavailable"); a non-empty id
        # is the checkpoint the MLX diffusion engine will lazy-load on first use.
        # Ships OFF: the daemon's [image] gate + the multi-GB download are what
        # turn it on. The prompt + the pixels stay on-device (NO cloud).
        self.image_model_id = settings.get("image_model", DEFAULT_IMAGE_MODEL)
        # #37 SPECULATIVE DECODING (OFF/neutral default). The master gate + the
        # small DRAFT model id mlx_lm uses to propose tokens. Off => normal
        # generation, today's runtime. A draft model is lazy-loaded on first use
        # behind the import/availability guard; if it cannot load, generate
        # honestly falls back to normal gen and reports speculative=False.
        self.speculative = bool(settings.get("speculative", False))
        self.draft_model_id = (settings.get("draft_model") or "").strip()
        # Lazy-loaded draft (model, tokenizer) for speculative decoding. Stays
        # None until the first generate AND speculative is on AND the draft
        # checkpoint + mlx_lm's speculative support are present; absent => normal
        # gen (the honest unavailable-fallback, NEVER fabricated speculative).
        self._draft_model = None
        self._draft_tokenizer = None
        self._draft_unavailable = False  # set once a load attempt fails (no retry storm)
        # #39 SELECTABLE QUANTIZATION (neutral default "auto" == today's
        # behavior). The requested quant is validated at config-load; the
        # model-load seam selects a matching variant if present, else falls back
        # + records which quant ACTUALLY loaded (NEVER claims a quant that did
        # not load). "auto" loads the model exactly as configured (today's path).
        self.quant = settings.get("quant", "auto")
        # The quant that ACTUALLY loaded (honest report). None until the LLM is
        # loaded; "auto" means the loader picked the on-disk variant as today.
        self._quant_loaded = None
        # op=embed BACKEND (config-selectable). DEFAULT = the Core ML bge
        # embedder (faster + higher retrieval quality). The Core ML embedder is
        # lazy-built/loaded on first embed (convert-on-first-use, cached under the
        # HF cache root); if it cannot be built/loaded, op=embed falls back to the
        # 4B mean-pool path and REPORTS the fallback (never silent). Its lifecycle
        # lock is separate from the MLX GPU lock, so embedding runs independently
        # of LLM generation.
        self.embedder_choice = settings.get("embedder", DEFAULT_EMBEDDER)
        self._coreml_embedder = None
        # Set once a build/load attempt hard-fails, so we don't retry the
        # (expensive) conversion on every embed call — we serve the 4B fallback.
        self._coreml_embed_unavailable = False
        self._embed_lifecycle_lock = threading.Lock()
        # op=rerank second stage (SHIPS ON — the measured winner). Warms the Core ML
        # cross-encoder at preload when enabled; op=rerank lazy-builds it otherwise
        # (convert-on-first-use, cached under the HF cache root, same discipline as
        # the embedder). If it cannot be built/loaded, op=rerank reports fell_back
        # and the daemon keeps the dense order — never silent. Its lifecycle lock is
        # separate from the MLX GPU lock, so reranking runs independently of
        # generation.
        self.reranker_enabled = bool(settings.get("reranker", DEFAULT_RERANKER_ENABLED))
        self._coreml_reranker = None
        # Set once a build/load attempt hard-fails, so we don't retry the
        # (expensive) conversion on every rerank call — we report fell_back.
        self._coreml_rerank_unavailable = False
        self._rerank_lifecycle_lock = threading.Lock()
        engine = (settings.get("engine") or DEFAULT_TTS_ENGINE).strip().lower()
        if engine not in TTS_ENGINE_DEFAULTS:
            log.warning(
                "unknown [speech].engine %r (supported: %s); falling back to %s",
                engine,
                ", ".join(sorted(TTS_ENGINE_DEFAULTS)),
                DEFAULT_TTS_ENGINE,
            )
            engine = DEFAULT_TTS_ENGINE
        self.engine_name = engine
        engine_defaults = TTS_ENGINE_DEFAULTS[engine]
        self.tts_id = settings.get("tts_model") or engine_defaults["model"]
        # Validate the resolved voice against the ACTIVE engine: an engine
        # switch that drags a stale voice along ("bm_george" on csm/orpheus)
        # would otherwise fail on every synthesis (csm: unsupported-voice
        # ValueError — opener bank 0/N, speak/converse dead) or silently
        # garble the speaker (orpheus prepends the tag unvalidated). Config
        # errors are recoverable here like everywhere else in load_config:
        # fall back to the engine's own default voice and say so.
        voice = settings.get("voice") or engine_defaults["voice"]
        if not self._voice_valid_for_engine(engine, voice):
            log.warning(
                "[speech].voice %r is not a valid voice for engine %r; falling back to %r",
                voice,
                engine,
                engine_defaults["voice"],
            )
            voice = engine_defaults["voice"]
        self.voice = voice
        self.speed = settings["speed"]
        # An explicit empty list means load_config DISABLED the bank (invalid
        # [speech].openers); only a missing key falls back to the defaults —
        # `or DEFAULT_OPENERS` would silently resurrect the desync the
        # disable exists to prevent.
        openers = settings.get("openers")
        self.openers = list(openers) if openers is not None else list(DEFAULT_OPENERS)
        self.classifier_template = classifier_template
        self.persona = persona
        self._model = None
        self._tokenizer = None
        # SELF-DISTILLATION (distill.rs) — the LIVE personal LoRA adapter, if the
        # daemon has PROMOTED one (state/lora/promoted/). The resident LLM loads
        # WITH the adapter when its manifest base_model matches self.llm_id; else
        # base. HONEST: `_active_adapter` is the loaded adapter's stamp (or None =
        # base), `_adapter_note` records why an adapter present on disk was NOT
        # loaded (base mismatch / load failure) so op=generate never claims an
        # adapter it didn't apply. Both are set in _ensure_llm at load time.
        self._active_adapter = None
        self._adapter_note = None
        # Multi-resident LOCAL model manager (task #17). The base [models].llm is
        # ALWAYS warm + the single-resident fallback; the manager additionally
        # keeps the configured extra warm-set resident WHEN the RAM budget allows
        # (default: empty warm-set + 0 budget == single-resident, today's
        # behavior). The manager is pure bookkeeping; it loads models via the
        # `loader` callback the engine passes to select() (the real MLX load),
        # so it adds NO MLX dependency of its own and is fully unit-testable.
        self.local_manager = LocalWarmManager(
            base_id=self.llm_id,
            configured=settings.get("local_warm", []),
            budget_gib=settings.get("local_budget_gib", 0.0),
            sizes=settings.get("local_sizes", {}),
        )
        # Lazy-loaded on-device VLM (op=describe_image). Stays None until the
        # first describe_image call AND mlx-vlm + the checkpoint are present;
        # absent => the op returns the honest unavailable structure.
        self._vlm_model = None
        self._vlm_processor = None
        self._vlm_config = None
        # Lazy-loaded on-device diffusion model (op=generate_image). Stays None
        # until the first generate_image call AND the diffusion package + the
        # checkpoint are present; absent => the op returns the honest
        # unavailable structure (NEVER a fabricated image, NEVER a cloud call).
        self._image_model = None
        self._tts = None
        self._tts_counter = itertools.count(int(time.time()))
        self._lock = threading.Lock()
        # Dedicated classify model (separate resident model when
        # [models].classifier is non-empty; aliases the main LLM otherwise).
        self._cls_model = None
        self._cls_tokenizer = None
        # Classifier KV prompt cache state (built on the classify model).
        self._cls_cache = None
        self._cls_cache_len = 0
        self._cls_prefix_tokens = []
        self._cls_prefix_str = ""
        self._cls_suffix_str = ""
        # Persona (generate) KV prompt cache state — same pattern: the static
        # persona-only prefix is prefilled once; facts/history/utterance are
        # prefilled per request and trimmed back afterwards.
        self._gen_cache = None
        self._gen_cache_len = 0
        self._gen_prefix_tokens = []
        self._gen_prefix_str = ""
        # Per-agent persona KV prompt caches (op=converse with a persona
        # override). Maps agent name -> a dict with the same fields as the base
        # cache above {cache, cache_len, prefix_tokens, prefix_str}. An ordered
        # dict used as an LRU: the most-recently-used agent is moved to the end,
        # and the oldest is evicted past AGENT_PERSONA_CACHE_MAX so resident GPU
        # memory stays bounded across a session that rotates through agents. All
        # access is under self._lock (the same GPU lock the base cache uses), so
        # no separate synchronization is needed.
        from collections import OrderedDict as _OrderedDict

        self._agent_gen_caches = _OrderedDict()
        # In-process LAST-METRICS store (accel/benchmark-foundation). The decode
        # loops used to throw away the tok/s + peak-memory mlx_lm measures on the
        # final GenerationResponse; we now record the most-recent stream_generate
        # run's honest telemetry here so the op response can surface it AND an
        # in-process reader (last_metrics()) can poll the latest decode's numbers.
        # A single reference, replaced whole (CPython dict assignment is atomic);
        # the GPU lock already serializes the decodes that write it.
        self._last_metrics = None

    def last_metrics(self):
        """Return the most-recent decode's honest telemetry dict (tok/s + peak
        memory from the last stream_generate run), or None if nothing has been
        decoded yet. Reference read; no lock needed (see _last_metrics above)."""
        return self._last_metrics

    def _record_metrics(self, resp, path):
        """Capture the honest decode telemetry off `resp` (the LAST
        GenerationResponse of a stream_generate loop) into the in-process store
        and return it. Called under self._lock at the end of a decode. A None/
        empty resp (no token produced) leaves the previous value untouched and
        returns None, so a cancelled-before-first-token decode never clobbers the
        store with blanks."""
        m = metrics_from_response(resp, path)
        if m is not None:
            self._last_metrics = m
        return m

    @staticmethod
    def _voice_valid_for_engine(engine, voice):
        """Whether `voice` is usable on `engine`. csm: must be a known
        speaker prompt (synthesis raises otherwise). orpheus: must be a
        named upstream speaker (mlx-audio applies the tag unvalidated).
        kokoro: the language is the first letter of the voice name, so that
        letter must be a valid Kokoro language code (mirrors _lang_code)."""
        if not voice:
            return False
        if engine == "csm":
            return voice in CSM_PROMPT_TEXTS
        if engine == "orpheus":
            return voice in ORPHEUS_VOICES
        return voice[0] in _KOKORO_LANGS

    def _resolve_request_voice(self, voice):
        """Per-request voice override, validated against the ACTIVE engine
        (audit fix): the daemon echoes [speech].voice on EVERY speak and
        converse request, so the engine-aware fallback in __init__ — written
        exactly for the "engine switch drags a stale voice along" config
        state — was dead on the daemon's real path: a csm/orpheus engine
        with voice="bm_george" failed (or garbled) every synthesis except
        the opener bank. Mirror the init recovery policy here: warn and
        substitute self.voice (already engine-validated at init)."""
        if not voice:
            return self.voice
        if not self._voice_valid_for_engine(self.engine_name, voice):
            log.warning(
                "request voice %r is not a valid voice for engine %r; using %r",
                voice,
                self.engine_name,
                self.voice,
            )
            return self.voice
        return voice

    # -- model loading -------------------------------------------------
    # _ensure_* methods must be called with self._lock held.

    def _ensure_llm(self):
        if self._model is None:
            from mlx_lm import load

            # #39 QUANT SELECTION + HONEST fallback: with quant == "auto" (the
            # neutral default) load the configured id EXACTLY as today. With an
            # explicit quant, TRY the derived quant-variant id first; if it is not
            # present (load raises) fall back to the configured base id and record
            # the quant that ACTUALLY loaded — NEVER claim the requested quant
            # when the base loaded instead. The real RAM/speed tradeoff is
            # device-gated; this seam only governs WHICH id is requested.
            load_id, loaded_quant = self._resolve_quant_load(self.llm_id, self.quant)
            # SELF-DISTILLATION: a PROMOTED personal adapter, loaded ONLY when its
            # base_model matches the id we're loading (an adapter is invalid for a
            # different base). A present-but-mismatched or unloadable adapter is
            # recorded in _adapter_note and generation serves BASE — never a
            # silently-wrong or falsely-claimed adapter.
            adapter_path, adapter_stamp, self._adapter_note = self._promoted_adapter(load_id)
            log.info(
                "loading LLM %s (resident after first use)%s...",
                load_id,
                f" with adapter {adapter_stamp}" if adapter_path else "",
            )
            t0 = time.perf_counter()
            self._active_adapter = None
            try:
                if adapter_path:
                    try:
                        self._model, self._tokenizer = load(load_id, adapter_path=adapter_path)
                        self._active_adapter = adapter_stamp
                    except Exception as e:
                        # Adapter failed to fuse — fall back to base HONESTLY.
                        log.warning("promoted adapter %s failed to load (%s); serving base", adapter_stamp, e)
                        self._adapter_note = f"adapter load failed: {e}"
                        self._model, self._tokenizer = load(load_id)
                else:
                    self._model, self._tokenizer = load(load_id)
            except Exception:
                if load_id != self.llm_id:
                    # The requested quant variant is absent/unloadable: fall back
                    # to the configured base id and report the truth (auto = the
                    # on-disk variant the loader picked). Retry the adapter on base.
                    log.warning(
                        "quant variant %r unavailable; falling back to %s and reporting the loaded quant honestly",
                        load_id,
                        self.llm_id,
                    )
                    a2, s2, self._adapter_note = self._promoted_adapter(self.llm_id)
                    if a2:
                        try:
                            self._model, self._tokenizer = load(self.llm_id, adapter_path=a2)
                            self._active_adapter = s2
                        except Exception as e:
                            self._adapter_note = f"adapter load failed: {e}"
                            self._model, self._tokenizer = load(self.llm_id)
                    else:
                        self._model, self._tokenizer = load(self.llm_id)
                    loaded_quant = "auto"
                else:
                    raise
            self._quant_loaded = loaded_quant
            log.info("LLM loaded in %.1fs (quant=%s)", time.perf_counter() - t0, loaded_quant)
            try:
                self._build_generate_cache()
            except Exception:
                log.exception("persona prompt cache unavailable; generate will run uncached")
                self._gen_cache = None
        return self._model, self._tokenizer

    def _resolve_quant_load(self, base_id, quant):
        """PURE: the (load_id, reported_quant) pair for a quant request (#39).
        "auto" => (base_id, "auto") — today's behavior. An explicit quant =>
        (the derived quant-variant id, the requested quant) which the caller
        TRIES, falling back to base_id + "auto" if the variant is absent. No MLX,
        no load — the load + fallback happen in _ensure_llm."""
        if quant == "auto":
            return (base_id, "auto")
        return (quant_variant_id(base_id, quant), quant)

    def _promoted_adapter(self, base_id):
        """Return (adapter_path, stamp, note) for the daemon-PROMOTED personal
        LoRA adapter, or (None, None, note). Loads ONLY when state/lora/promoted/
        holds an adapters.safetensors AND a manifest.json whose base_model equals
        `base_id` — an adapter is invalid for a different base, and applying it to
        a mismatched model would silently corrupt output. HONEST: a mismatch, an
        unreadable manifest, or absence returns (None, None, <human note>); it
        never fabricates an adapter. No MLX/load here — _ensure_llm does the load
        + reports self._active_adapter from the returned stamp."""
        promoted = PROJECT_ROOT / "state" / "lora" / "promoted"
        weights = promoted / "adapters.safetensors"
        if not weights.is_file():
            return (None, None, None)
        try:
            with open(promoted / "manifest.json") as f:
                m = json.load(f)
        except Exception:
            return (None, None, "a promoted adapter is staged but its manifest is unreadable; serving base")
        base_model = m.get("base_model")
        if base_model != base_id:
            return (
                None,
                None,
                f"the promoted adapter is for {base_model!r}, not the resident {base_id!r}; serving base",
            )
        stamp = "lora:" + str(m.get("created") or "promoted")
        return (str(promoted), stamp, None)

    def reload_lora(self):
        """Drop the resident LLM so the NEXT generate reloads with the CURRENT
        state/lora/promoted/ pointer — fusing a freshly-promoted personal adapter,
        or serving base after a rollback. Held under the GPU lock; in-flight
        work is safe either way — ops that hold the lock serialize with this,
        and the deliberately lock-SLICED consolidate pass captures its own
        model reference at op start, so it completes coherently on the weights
        it began with (the reload applies from the next op). The reload itself
        is lazy (next _ensure_llm), so this call is cheap. The daemon calls it
        right after a promote/rollback.

        EVERY holder of the old model object is evicted, not just self._model —
        otherwise converse / the uncached + speculative generate paths keep
        serving the PRE-reload weights all session (they resolve the model via
        LocalWarmManager.resident, which returns a HIT without reloading) and
        the classifier (which aliases the main LLM when no dedicated classifier
        is configured) pins a second full copy in RAM. Evicting here means the
        next _ensure_local_llm/_ensure_classifier re-registers the freshly
        (re)loaded model, so the whole engine swaps atomically-from-the-callers'
        view and the spoken "it's live now / no longer live" is true for every
        generation surface."""
        with self._lock:
            self._model = None
            self._tokenizer = None
            self._active_adapter = None
            self._adapter_note = None
            self._gen_cache = None
            self._gen_cache_len = 0
            # The warm manager's pinned BASE entry holds the old model object;
            # pop it so _ensure_local_llm re-seeds from the reloaded self._model
            # (warm EXTRAS are untouched — the adapter fuses into base only).
            self.local_manager.resident.pop(self.llm_id, None)
            self.local_manager.resident.pop(self.local_manager.base_id, None)
            # AGENT-PERSONA converse KV caches were prefilled by the OLD
            # weights — decoding the reloaded model against them would condition
            # every agent-persona reply on pre-reload activations (the tokenizer
            # is unchanged, so the seam-trim can't catch it). Drop them all;
            # they rebuild lazily on each agent's next turn.
            self._agent_gen_caches.clear()
            # The classifier aliases the main LLM when no dedicated classifier
            # model is configured — drop the alias (and its prompt cache) so it
            # rebinds to the reloaded model instead of pinning the old copy.
            if not self.classifier_id:
                self._cls_model = None
                self._cls_tokenizer = None
                self._cls_cache = None
                self._cls_cache_len = 0

    def _ensure_draft(self):
        """#37: lazy-load the DRAFT model for speculative decoding, behind the
        import/availability guard. Returns (draft_model, draft_tokenizer) on
        success or None when speculative cannot run:
          * self.speculative is False        -> None (the OFF default).
          * self.draft_model_id is empty      -> None (no draft configured).
          * mlx_lm lacks speculative support  -> None.
          * the draft checkpoint fails to load -> None (recorded; no retry storm).
        A None return is the SHIPS-OFF / unavailable state, NOT an error: the
        generate path falls back to NORMAL generation and reports
        speculative=False. Caller holds self._lock."""
        if not self.speculative or not self.draft_model_id:
            return None
        if self._draft_unavailable:
            return None
        if not _mlx_speculative_available():
            self._draft_unavailable = True
            return None
        if self._draft_model is None:
            try:
                from mlx_lm import load

                log.info("loading DRAFT model %s for speculative decoding...", self.draft_model_id)
                t0 = time.perf_counter()
                self._draft_model, self._draft_tokenizer = load(self.draft_model_id)
                log.info("draft model loaded in %.1fs", time.perf_counter() - t0)
            except Exception:
                # Absent/corrupt draft checkpoint or any load failure: speculative
                # is UNAVAILABLE this run (normal gen), recorded so we do not retry
                # on every turn. NEVER fabricate speculative.
                log.exception(
                    "draft model %s could not be loaded; speculative decoding disabled (normal generation)",
                    self.draft_model_id,
                )
                self._draft_model = None
                self._draft_tokenizer = None
                self._draft_unavailable = True
                return None
        return (self._draft_model, self._draft_tokenizer)

    def speculative_status(self):
        """HONEST read-only snapshot of the speculative-decoding config for the
        HUD / a status op (#37). PURE — touches no model, takes no lock. Reports
        whether the gate is on + whether a draft is configured. It does NOT claim
        speculative is ACTIVE (only a generate call, after _ensure_draft, knows
        the draft actually loaded) and does NOT claim any speedup (device-gated)."""
        return {
            "enabled": self.speculative,
            "draft_model": self.draft_model_id,
            # Has a draft load already failed this run? (honest unavailable signal)
            "draft_unavailable": self._draft_unavailable,
        }

    def _local_loader(self, model_id):
        """Loader callback handed to the LocalWarmManager: returns
        (model, tokenizer) for `model_id`. The BASE id reuses the persona-cached
        resident from _ensure_llm (so its KV cache + embeddings are intact);
        any OTHER warm-set id is loaded fresh via mlx_lm. Caller (the manager,
        under select) is itself called with self._lock held. A non-base load
        that raises is caught by the manager and degraded to the base."""
        if model_id == self.llm_id:
            self._ensure_llm()
            return self._model, self._tokenizer
        from mlx_lm import load

        log.info("loading warm local model %s (resident after first use)...", model_id)
        t0 = time.perf_counter()
        model, tokenizer = load(model_id)
        log.info("warm local model %s loaded in %.1fs", model_id, time.perf_counter() - t0)
        return model, tokenizer

    def _ensure_local_llm(self, local_model=None):
        """Resolve the LOCAL model that answers this turn, keeping it warm via
        the manager. Returns (resolved_id, model, tokenizer). `local_model` is
        the Local-tier sub-choice the daemon threads through (a warm-set id);
        None / "" / an out-of-warm-set id transparently resolves to the base
        single-resident LLM — never a crash. Caller holds self._lock.

        The base path goes through _ensure_llm first so its persona KV cache is
        always built; the persona cache + sampler stay bound to the BASE model.
        A non-base warm model answers UNCACHED on its own resident weights (the
        instant-swap benefit is the loaded weights, not a per-model persona
        cache — keeping a cache per model would multiply RAM further; honest)."""
        # Always make sure the base is warm + persona-cached (it is the fallback
        # and the manager pins it). _ensure_llm registers it as the loader's
        # base entry; the manager then selects/keeps-warm the requested id.
        self._ensure_llm()
        if self.llm_id not in self.local_manager.resident:
            # First call: register the base as resident so warm_ids/eviction see
            # it and it is pinned. (select(base) below would do this too, but we
            # seed it explicitly so warm_ids reflects the base immediately.)
            self.local_manager.resident[self.llm_id] = (self._model, self._tokenizer)
        return self.local_manager.select(local_model, self._local_loader)

    def local_warm_status(self):
        """HONEST read-only snapshot of the local warm-set for the HUD / a
        status op (item 3 wires it through). PURE — touches no model, takes no
        lock (it reads the manager's bookkeeping). Reports the planned warm-set,
        which models are ACTUALLY resident right now, the active/base id and
        whether multi-resident is in effect (single-resident is the safe
        low-RAM default). The speed benefit of keeping >1 warm is device/RAM
        dependent and is NOT asserted here."""
        mgr = self.local_manager
        return {
            "base": mgr.base_id,
            "planned": list(mgr.plan),
            "resident": mgr.warm_ids(),
            "multi_resident": mgr.multi_resident(),
            "budget_gib": mgr.budget_gib,
        }

    def _ensure_classifier(self):
        """Resolve the (model, tokenizer) pair classify runs on and build its
        KV prompt cache once. With [models].classifier set, classification
        runs on a dedicated small resident model; an empty classifier id — or
        a load failure — reuses the main LLM. Any candidate configured here
        must first pass the 7-utterance accuracy gate (see DEFAULT_CLASSIFIER
        comment: all small candidates tried so far failed it)."""
        if self._cls_model is None:
            if self.classifier_id:
                try:
                    from mlx_lm import load

                    log.info(
                        "loading classifier %s (resident after first use)...", self.classifier_id
                    )
                    t0 = time.perf_counter()
                    self._cls_model, self._cls_tokenizer = load(self.classifier_id)
                    log.info("classifier loaded in %.1fs", time.perf_counter() - t0)
                except Exception:
                    log.exception(
                        "classifier %s failed to load; classify falls back to the main LLM",
                        self.classifier_id,
                    )
                    self.classifier_id = ""
                    self._cls_model = None
            if self._cls_model is None:
                self._ensure_llm()
                self._cls_model, self._cls_tokenizer = self._model, self._tokenizer
            try:
                self._build_classifier_cache()
            except Exception:
                log.exception("classifier prompt cache unavailable; classify will run uncached")
                self._cls_cache = None
        return self._cls_model, self._cls_tokenizer

    @staticmethod
    def _patch_kokoro_sinegen():
        """mlx-audio 0.4.4 upstream bug: SineGen._f02sine's downsample/upsample
        interpolation can come back one 300-sample frame longer or shorter than
        the uv path computed directly from f0, crashing the vocoder with a
        broadcast error for certain utterance lengths. Re-align sine_waves to
        the canonical f0/uv length (trim or zero-pad <=12.5ms at the tail —
        inaudible). Re-check on every mlx-audio upgrade."""
        import mlx.core as mx
        from mlx_audio.tts.models.kokoro import istftnet

        if getattr(istftnet.SineGen, "_darwin_len_patch", False):
            return

        def aligned_call(self, f0):
            fn = f0 * mx.arange(1, self.harmonic_num + 2)[None, None, :]
            sine_waves = self._f02sine(fn) * self.sine_amp
            uv = self._f02uv(f0)
            length = uv.shape[1]
            if sine_waves.shape[1] > length:
                sine_waves = sine_waves[:, :length, :]
            elif sine_waves.shape[1] < length:
                pad = mx.zeros(
                    (sine_waves.shape[0], length - sine_waves.shape[1], sine_waves.shape[2])
                )
                sine_waves = mx.concatenate([sine_waves, pad], axis=1)
            noise_amp = uv * self.noise_std + (1 - uv) * self.sine_amp / 3
            noise = noise_amp * mx.random.normal(sine_waves.shape)
            return sine_waves * uv + noise, uv, noise

        istftnet.SineGen.__call__ = aligned_call
        istftnet.SineGen._darwin_len_patch = True
        log.info("applied SineGen length-alignment patch (mlx-audio 0.4.4 vocoder bug)")

    def _ensure_tts(self):
        if self._tts is None:
            from mlx_audio.tts.utils import load_model as load_tts_model

            if self.engine_name == "kokoro":
                self._patch_kokoro_sinegen()
            log.info(
                "loading TTS %s (engine=%s, resident after first use)...",
                self.tts_id,
                self.engine_name,
            )
            t0 = time.perf_counter()
            self._tts = load_tts_model(self.tts_id)
            log.info("TTS loaded in %.1fs", time.perf_counter() - t0)
        return self._tts

    def preload(self):
        """Warm STT, LLM (+classifier and persona prompt caches) and TTS so
        the first real utterance pays no load or Metal-kernel-compile cost.
        Each stage takes the GPU lock independently, so early requests
        interleave via the normal lazy path."""
        import numpy as np

        try:
            import mlx_whisper

            t0 = time.perf_counter()
            with self._lock:
                mlx_whisper.transcribe(np.zeros(8000, dtype=np.float32), path_or_hf_repo=self.stt_id)
            log.info("preload: STT %s ready (%.1fs)", self.stt_id, time.perf_counter() - t0)
        except Exception:
            log.exception("preload: STT failed; transcribe will load lazily")
        try:
            t0 = time.perf_counter()
            with self._lock:
                self._ensure_llm()
            log.info("preload: LLM %s ready (%.1fs)", self.llm_id, time.perf_counter() - t0)
        except Exception:
            log.exception("preload: LLM failed; generate/converse will load lazily")
        try:
            t0 = time.perf_counter()
            with self._lock:
                self._ensure_classifier()
            log.info(
                "preload: classifier %s ready (%.1fs)",
                self.classifier_id or f"main LLM {self.llm_id}",
                time.perf_counter() - t0,
            )
        except Exception:
            log.exception("preload: classifier failed; classify will load lazily")
        # op=embed Core ML backend: convert-on-first-use is HEAVY (a one-time
        # torch->Core ML conversion) and the daemon's op=embed request has a hard
        # timeout — if the FIRST embed paid conversion latency it would blow the
        # timeout and a reindex in that window would build a vectorless (BM25-only)
        # index. So warm/convert it EAGERLY here (only when it is the configured
        # backend). Its lifecycle lock is independent of the MLX GPU lock, so this
        # runs OUTSIDE self._lock and does not serialize behind model warms. A
        # failure is the honest ships-unavailable state: log + mark unavailable so
        # op=embed falls back to the mean-pool path (never a broken hang).
        if self.embedder_choice == EMBEDDER_COREML and not self._coreml_embed_unavailable:
            try:
                from coreml_embed import CoreMLEmbedderUnavailable

                t0 = time.perf_counter()
                self._ensure_coreml_embedder()  # convert-on-first-use / validate-load
                log.info(
                    "preload: Core ML embedder %s ready (%.1fs)",
                    EMBEDDER_COREML, time.perf_counter() - t0,
                )
            except CoreMLEmbedderUnavailable as e:
                self._coreml_embed_unavailable = True
                log.warning(
                    "preload: Core ML embedder unavailable (%s); op=embed will "
                    "fall back to the mean-pool path", e,
                )
            except Exception:
                self._coreml_embed_unavailable = True
                log.exception(
                    "preload: Core ML embedder warm failed; op=embed will fall "
                    "back to the mean-pool path",
                )
        # op=rerank Core ML cross-encoder: convert-on-first-use is HEAVY (a one-time
        # torch->Core ML conversion), same as the embedder — so warm it EAGERLY here
        # when reranking is enabled, so the first real op=rerank never pays conversion
        # inside the daemon's request timeout (which would blow the timeout and make
        # the daemon fall back to dense order for that window). Its lifecycle lock is
        # independent of the MLX GPU lock, so this runs OUTSIDE self._lock. A failure
        # is the honest fallback state: log + mark unavailable so op=rerank reports
        # fell_back and the daemon keeps dense order (never a broken hang).
        if self.reranker_enabled and not self._coreml_rerank_unavailable:
            try:
                from coreml_rerank import CoreMLRerankerUnavailable

                t0 = time.perf_counter()
                self._ensure_coreml_reranker()  # convert-on-first-use / validate-load
                log.info(
                    "preload: Core ML reranker %s ready (%.1fs)",
                    RERANKER_COREML, time.perf_counter() - t0,
                )
            except CoreMLRerankerUnavailable as e:
                self._coreml_rerank_unavailable = True
                log.warning(
                    "preload: Core ML reranker unavailable (%s); op=rerank will "
                    "report fell_back and the daemon will keep dense order", e,
                )
            except Exception:
                self._coreml_rerank_unavailable = True
                log.exception(
                    "preload: Core ML reranker warm failed; op=rerank will report "
                    "fell_back and the daemon will keep dense order",
                )
        # LEARNED VAD native weights (inference/coreml_vad.py, task: replace the
        # RMS-gate VAD). The daemon runs the learned Silero VAD IN-PROCESS on its
        # realtime audio thread (daemon/src/silero.rs) — an RPC-per-frame design
        # was MEASURED unsafe: op=vad round-trips were 0.98ms idle but 131ms
        # median under a concurrent op=embed load on this server (GIL + backend
        # locks), 4x over the daemon's ~32ms frame budget — so the daemon loads
        # the raw weights and never depends on this server at capture time. This
        # preload step EXPORTS those weights (atomic, idempotent) to
        # state/models/, where the daemon picks them up (it retries while the
        # file is absent and carries capture on its RMS gate until then,
        # surfaced). Failure is non-fatal: the daemon simply stays on the RMS
        # gate and says so.
        try:
            from coreml_vad import NATIVE_WEIGHTS_NAME, export_native_weights

            t0 = time.perf_counter()
            dest = PROJECT_ROOT / "state" / "models" / NATIVE_WEIGHTS_NAME
            if export_native_weights(dest):
                log.info(
                    "preload: learned-VAD native weights exported to %s (%.1fs)",
                    dest, time.perf_counter() - t0,
                )
            else:
                log.info("preload: learned-VAD native weights already present at %s", dest)
        except Exception:
            log.exception(
                "preload: learned-VAD native weights export failed; the daemon "
                "stays on the RMS gate (surfaced daemon-side) until an export succeeds",
            )
        try:
            t0 = time.perf_counter()
            with self._lock:
                tts = self._ensure_tts()
                # Tiny throwaway synthesis through the engine dispatch:
                # compiles Metal kernels and fetches the configured voice.
                self._tts_synth_fn()(tts, "Ready.", self.voice)
            log.info("preload: TTS %s ready (%.1fs)", self.tts_id, time.perf_counter() - t0)
        except Exception:
            log.exception("preload: TTS failed; speak will load lazily")
        # Opener bank regenerates on every server start, AFTER the TTS warm,
        # so a voice/engine change Just Works. Each synthesis takes the GPU
        # lock independently like any other request.
        try:
            self.generate_openers()
        except Exception:
            log.exception("preload: opener bank generation failed; daemon will run without openers")

    # -- classifier prompt cache ----------------------------------------

    def _classifier_prompt_parts(self):
        """Split the fully chat-templated classifier prompt into the static
        prefix (everything before the utterance) and static suffix."""
        template = self.classifier_template
        if "{utterance}" in template:
            body = template.replace("{utterance}", _UTTERANCE_SENTINEL)
        else:
            body = f'{template}\n\nUtterance: "{_UTTERANCE_SENTINEL}"\nJSON:'
        rendered = self._render_chat(body, tokenizer=self._cls_tokenizer)
        if _UTTERANCE_SENTINEL not in rendered:
            raise ValueError("utterance slot lost during chat templating")
        prefix, suffix = rendered.split(_UTTERANCE_SENTINEL, 1)
        return prefix, suffix

    def _build_classifier_cache(self):
        """Prefill a KV cache with the static classifier prefix once (on the
        dedicated classify model when configured). Each classify request then
        only prefills utterance+suffix tokens and is trimmed back to the
        prefix afterwards (mlx_lm trim_prompt_cache)."""
        import mlx.core as mx
        from mlx_lm.models.cache import can_trim_prompt_cache, make_prompt_cache

        prefix_str, suffix_str = self._classifier_prompt_parts()
        prefix_tokens = self._cls_tokenizer.encode(prefix_str)
        cache = make_prompt_cache(self._cls_model)
        if not can_trim_prompt_cache(cache):
            raise ValueError(
                f"prompt cache for {self.classifier_id or self.llm_id} is not trimmable"
            )
        t0 = time.perf_counter()
        logits = self._cls_model(mx.array(prefix_tokens)[None], cache=cache)
        mx.eval(logits)
        self._cls_prefix_str = prefix_str
        self._cls_suffix_str = suffix_str
        self._cls_prefix_tokens = list(prefix_tokens)
        self._cls_cache = cache
        self._cls_cache_len = len(prefix_tokens)
        log.info(
            "classifier prompt cache prefilled: %d tokens in %dms",
            len(prefix_tokens),
            int((time.perf_counter() - t0) * 1000),
        )

    def _classify_cached(self, text):
        """Generate a classification using the prefilled prefix cache on the
        classify model. Caller holds self._lock."""
        from mlx_lm import stream_generate
        from mlx_lm.models.cache import trim_prompt_cache

        full_tokens = self._cls_tokenizer.encode(
            self._cls_prefix_str + text + self._cls_suffix_str
        )
        # Tokenization at the prefix/utterance seam can merge across the
        # boundary; trim the cache down to the actual common token prefix.
        common = 0
        for a, b in zip(self._cls_prefix_tokens[: self._cls_cache_len], full_tokens):
            if a != b:
                break
            common += 1
        if common < self._cls_cache_len:
            trim_prompt_cache(self._cls_cache, self._cls_cache_len - common)
            self._cls_cache_len = common
        suffix_tokens = full_tokens[self._cls_cache_len :]
        pieces = []
        generated = 0
        last_resp = None
        try:
            for resp in stream_generate(
                self._cls_model,
                self._cls_tokenizer,
                prompt=suffix_tokens,
                max_tokens=CLASSIFY_MAX_TOKENS,
                prompt_cache=self._cls_cache,
            ):
                pieces.append(resp.text)
                generated += 1
                last_resp = resp
        finally:
            # Trim everything past the static prefix so the cache is
            # reusable — on success AND on any mid-decode exception (audit
            # fix, mirroring converse's finally: a Metal OOM used to leave
            # the shared cache grown, and the whole-cache rebuild recovery
            # then prefilled a NEW cache while the grown one was still
            # referenced — a peak-memory spike at the worst moment).
            offset = getattr(self._cls_cache[0], "offset", None)
            added = (
                (offset - self._cls_cache_len)
                if isinstance(offset, int)
                else (len(suffix_tokens) + generated)
            )
            if added > 0:
                trim_prompt_cache(self._cls_cache, added)
        # Surface the decode telemetry the loop used to discard (additive).
        metrics = self._record_metrics(last_resp, "classify.cached")
        return "".join(pieces), metrics

    # -- persona (generate) prompt cache ---------------------------------

    def _persona_prefix_str(self, persona_text):
        """The chat-templated prompt up to the end of `persona_text` inside the
        system message. Facts (same system message), history turns and the final
        user turn all render after this point, so this prefix is static across
        every generate/converse request that shares that persona — exactly what
        the KV cache is keyed on. Works for the base persona AND any per-agent
        override (same templating, different text)."""
        if not persona_text:
            raise ValueError("no persona text; nothing to cache")
        rendered = self._render_chat_messages(
            [
                {"role": "system", "content": persona_text + _UTTERANCE_SENTINEL},
                {"role": "user", "content": "x"},
            ]
        )
        if _UTTERANCE_SENTINEL not in rendered:
            raise ValueError("persona slot lost during chat templating")
        return rendered.split(_UTTERANCE_SENTINEL, 1)[0]

    def _generate_prefix_str(self):
        """The base-persona prefix (back-compat wrapper over
        `_persona_prefix_str`)."""
        return self._persona_prefix_str(self.persona)

    def _prefill_persona_cache(self, persona_text):
        """Prefill a fresh KV cache with `persona_text`'s static prefix and
        return {cache, cache_len, prefix_tokens, prefix_str}. Caller holds
        self._lock and self._model is loaded. Same make_prompt_cache +
        trim-back-on-reuse pattern as the classifier cache; raises if the
        model's cache is not trimmable (so the caller can fall back to uncached
        decode rather than corrupt a shared cache)."""
        import mlx.core as mx
        from mlx_lm.models.cache import can_trim_prompt_cache, make_prompt_cache

        prefix_str = self._persona_prefix_str(persona_text)
        prefix_tokens = self._tokenizer.encode(prefix_str)
        cache = make_prompt_cache(self._model)
        if not can_trim_prompt_cache(cache):
            raise ValueError(f"prompt cache for {self.llm_id} is not trimmable")
        t0 = time.perf_counter()
        logits = self._model(mx.array(prefix_tokens)[None], cache=cache)
        mx.eval(logits)
        log.info(
            "persona prompt cache prefilled: %d tokens in %dms",
            len(prefix_tokens),
            int((time.perf_counter() - t0) * 1000),
        )
        return {
            "cache": cache,
            "cache_len": len(prefix_tokens),
            "prefix_tokens": list(prefix_tokens),
            "prefix_str": prefix_str,
        }

    def _build_generate_cache(self):
        """Prefill the BASE-persona KV cache once (called from _ensure_llm)."""
        state = self._prefill_persona_cache(self.persona)
        self._gen_cache = state["cache"]
        self._gen_cache_len = state["cache_len"]
        self._gen_prefix_tokens = state["prefix_tokens"]
        self._gen_prefix_str = state["prefix_str"]

    def _get_agent_gen_cache(self, name, persona_text):
        """Return this agent's resident persona-prefix KV cache state, building
        and prefilling it on first use, or None if prefill is not feasible (the
        caller then runs that turn uncached — never corrupting another cache).

        Keyed by agent `name`, LRU-bounded by AGENT_PERSONA_CACHE_MAX so a long
        session that rotates through many agents keeps GPU memory in check. On a
        hit the cache is moved to the MRU end and returned, so the next converse
        for that agent reuses its prefilled persona-prefix KV instead of
        recomputing it. Caller holds self._lock and self._model is loaded."""
        cached = self._agent_gen_caches.get(name)
        if cached is not None:
            self._agent_gen_caches.move_to_end(name)
            return cached
        try:
            state = self._prefill_persona_cache(persona_text)
        except Exception:
            log.exception(
                "per-agent persona cache prefill failed for %r; running this turn uncached",
                name,
            )
            return None
        for evicted in _lru_insert(
            self._agent_gen_caches, name, state, AGENT_PERSONA_CACHE_MAX
        ):
            log.info("evicted per-agent persona cache for %r (LRU)", evicted)
        return state

    def _persona_sampler(self):
        """Temperature/top-p sampler with an explicit, per-request PRNG key.

        mlx_lm's make_sampler draws through MLX's implicit PRNG state, which
        is thread-local and gets bound by mx.compile in whichever thread first
        imports/traces the sampling kernels (our preload thread). Requests run
        on asyncio.to_thread workers, where that binding made "temperature"
        sampling byte-deterministic per request — identical requests returned
        identical replies even after reseeding the worker thread (verified
        live against this server). Threading an explicit key through
        mx.random.split sidesteps the implicit state entirely; the key is
        seeded from the wall clock so every request samples differently.
        Matches make_sampler's order of operations: top-p filter on the raw
        logprobs, then categorical at temperature."""
        import mlx.core as mx
        from mlx_lm.sample_utils import apply_top_p

        state = {"key": mx.random.key(time.time_ns() & 0x7FFFFFFFFFFFFFFF)}
        inv_temp = 1.0 / GENERATE_TEMP

        def sample(logprobs):
            keys = mx.random.split(state["key"])
            state["key"] = keys[0]
            filtered = apply_top_p(logprobs, GENERATE_TOP_P)
            return mx.random.categorical(filtered * inv_temp, key=keys[1])

        return sample

    def _generate_cached(self, prompt, max_tokens, cancelled=None):
        """Generate a reply using the prefilled persona-prefix cache.
        Caller holds self._lock. `prompt` is the fully rendered chat prompt,
        which starts with (a tokenization-seam-tolerant match of) the cached
        persona prefix.

        `cancelled` (optional, audit fix): thread-safe callable polled once per
        decoded chunk; when it reports the peer gone the decode loop ABORTS
        (partial text returned, nobody is listening) instead of burning GPU for
        the rest of max_tokens. The finally trim below already restores the
        cache on ANY early exit, so an abort leaves the same invariant as an
        exception. None (the default) never cancels."""
        from mlx_lm import stream_generate
        from mlx_lm.models.cache import trim_prompt_cache

        full_tokens = self._tokenizer.encode(prompt)
        # Tokenization at the prefix boundary can merge across the seam; trim
        # the cache down to the actual common token prefix.
        common = 0
        for a, b in zip(self._gen_prefix_tokens[: self._gen_cache_len], full_tokens):
            if a != b:
                break
            common += 1
        if common < self._gen_cache_len:
            trim_prompt_cache(self._gen_cache, self._gen_cache_len - common)
            self._gen_cache_len = common
        suffix_tokens = full_tokens[self._gen_cache_len :]
        pieces = []
        generated = 0
        last_resp = None
        try:
            for resp in stream_generate(
                self._model,
                self._tokenizer,
                prompt=suffix_tokens,
                max_tokens=max_tokens,
                prompt_cache=self._gen_cache,
                sampler=self._persona_sampler(),
            ):
                if cancelled is not None and cancelled():
                    log.info(
                        "generate: peer disconnected; aborting cached decode after %d chunks",
                        generated,
                    )
                    break
                pieces.append(resp.text)
                generated += 1
                last_resp = resp
        finally:
            # Trim everything past the static prefix so the cache is
            # reusable — on success AND on any mid-decode exception (audit
            # fix; see _classify_cached — same invariant converse already
            # enforces with its finally).
            offset = getattr(self._gen_cache[0], "offset", None)
            added = (
                (offset - self._gen_cache_len)
                if isinstance(offset, int)
                else (len(suffix_tokens) + generated)
            )
            if added > 0:
                trim_prompt_cache(self._gen_cache, added)
        # Surface the decode telemetry the loop used to discard (additive).
        metrics = self._record_metrics(last_resp, "generate.cached")
        return "".join(pieces), metrics

    # -- ops -----------------------------------------------------------

    @staticmethod
    def _load_wav(path):
        # mlx_whisper only accepts file paths when ffmpeg is installed; decode
        # the daemon's WAVs ourselves and hand it a 16kHz float32 array instead.
        import wave

        import numpy as np

        with wave.open(path, "rb") as wf:
            rate = wf.getframerate()
            channels = wf.getnchannels()
            width = wf.getsampwidth()
            frames = wf.readframes(wf.getnframes())
        if width == 2:
            audio = np.frombuffer(frames, dtype=np.int16).astype(np.float32) / 32768.0
        elif width == 4:
            audio = np.frombuffer(frames, dtype=np.int32).astype(np.float32) / 2147483648.0
        else:
            raise ValueError(f"unsupported WAV sample width: {width} bytes")
        if channels > 1:
            audio = audio.reshape(-1, channels).mean(axis=1)
        # Service-boundary hardening: an empty/near-empty WAV would crash
        # np.interp below (zero sample points) or, at 16kHz, get padded to a
        # 30s segment by whisper and hallucinate text from pure silence.
        if rate <= 0 or len(audio) / rate < 0.1:
            raise ValueError(
                f"audio too short to transcribe ({len(audio)} frames at {rate}Hz; need >=0.1s)"
            )
        if rate != 16000:
            target = int(len(audio) * 16000 / rate)
            audio = np.interp(
                np.linspace(0.0, len(audio), num=target, endpoint=False),
                np.arange(len(audio)),
                audio,
            ).astype(np.float32)
        return audio

    def _transcribe_whisper(self, path):
        """On-device STT: decode the WAV and run mlx_whisper. The private,
        offline default AND the fallback for the cloud-STT tier. Caller does not
        hold self._lock (this acquires it)."""
        import mlx_whisper

        audio = self._load_wav(path)
        with self._lock:
            result = mlx_whisper.transcribe(audio, path_or_hf_repo=self.stt_id)
        return result["text"]

    def transcribe(self, path, backend=None, el_key=None, model=None):
        """WAV path -> (text, words). `backend` selects the STT path the daemon
        already chose (its own gated cloud-STT tier+key+offline decision):
          - "elevenlabs_scribe": the OPTIONAL cloud-STT tier. POST the audio file
            to ElevenLabs Scribe (xi-api-key header, key from `el_key`) and return
            the transcript PLUS the per-word `words` stream (carrying speaker_ids
            when Scribe diarized) so the daemon's gated diarize path can CONSUME
            the real labels. On ANY error/timeout — or a missing key — FALL BACK to
            on-device mlx_whisper (never fail the turn). HONESTY: on this path the
            user's VOICE AUDIO leaves the device — MORE sensitive than TTS text.
          - anything else (None/"whisper"): on-device mlx_whisper, EXACTLY as
            before — on-device whisper has NO diarization model, so `words` is None
            (the daemon then renders the honest single stream, never fabricating
            speakers).
        mlx_whisper is the private/offline default + the fallback; the cloud path
        is reached only when the daemon asked for it. `words` is None on the whisper
        path (no diarization) and on a Scribe response that carried no word detail."""
        use_scribe = (
            isinstance(backend, str)
            and backend.strip().lower() == ELEVENLABS_SCRIBE_BACKEND
        )
        if use_scribe:
            try:
                text, words = _elevenlabs_scribe_transcribe(
                    path, el_key, model or ELEVENLABS_SCRIBE_MODEL
                )
                if text is not None:
                    return text, words
                log.warning(
                    "ElevenLabs Scribe returned no text; falling back to on-device whisper"
                )
            except Exception as exc:
                # ANY error (network, HTTP, auth, timeout, missing key, decode) ->
                # whisper. Log only the redacted exception CLASS — never exc_info
                # / the message, which could echo a key-bearing URL fragment.
                log.warning(
                    "ElevenLabs Scribe STT failed (%s); falling back to on-device whisper",
                    _redact_elevenlabs(type(exc).__name__),
                )
            # FALL BACK to the on-device whisper path below.
        # On-device whisper: no diarization model -> no `words` (single stream).
        return self._transcribe_whisper(path), None

    def _render_chat(self, user_text, system=None, tokenizer=None):
        """Render a single user turn (plus optional system) to a prompt
        string. Used by the strict-JSON ops (classify, extract_facts), so
        enable_thinking=False is requested: hybrid-thinking templates (Qwen3
        non-2507, incl. the dedicated classifier model) would otherwise spend
        the whole token budget inside <think>. Templates without the flag
        ignore the unused context variable."""
        tokenizer = tokenizer if tokenizer is not None else self._tokenizer
        if hasattr(tokenizer, "apply_chat_template") and getattr(tokenizer, "chat_template", None):
            messages = []
            if system:
                messages.append({"role": "system", "content": system})
            messages.append({"role": "user", "content": user_text})
            try:
                return tokenizer.apply_chat_template(
                    messages, tokenize=False, add_generation_prompt=True, enable_thinking=False
                )
            except TypeError:
                return tokenizer.apply_chat_template(
                    messages, tokenize=False, add_generation_prompt=True
                )
        if system:
            return f"{system}\n\n{user_text}"
        return user_text

    def _render_chat_messages(self, messages, tokenizer=None):
        """Render an arbitrary chat message list to a prompt string. `tokenizer`
        defaults to the base resident; the Local-tier sub-choice passes the
        SELECTED warm model's tokenizer so the prompt uses that model's own chat
        template (a different warm model may template differently)."""
        tokenizer = tokenizer if tokenizer is not None else self._tokenizer
        if hasattr(tokenizer, "apply_chat_template") and getattr(tokenizer, "chat_template", None):
            return tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
        # Plain-text fallback for templateless tokenizers.
        parts = []
        for m in messages:
            if m["role"] == "system":
                parts.append(m["content"])
            elif m["role"] == "user":
                parts.append(f"User: {m['content']}")
            else:
                parts.append(f"Assistant: {m['content']}")
        parts.append("Assistant:")
        return "\n\n".join(parts)

    def _run_llm(self, user_text, max_tokens, system=None):
        from mlx_lm import generate

        with self._lock:
            self._ensure_llm()
            prompt = self._render_chat(user_text, system=system)
            return generate(self._model, self._tokenizer, prompt=prompt, max_tokens=max_tokens)

    def _run_llm_interruptible(self, user_text, max_tokens, system=None, prefill_chunk=256, decode_chunk=24):
        """Greedy decode like _run_llm, but self._lock is acquired and
        released in slices — prefill_chunk prompt tokens or decode_chunk
        generated tokens at a time — instead of held for the whole pass.

        op=consolidate is the only caller: its uncached prefill (up to 40
        transcript exchanges + the facts table + the ~470-word system text)
        plus up to CONSOLIDATE_MAX_TOKENS of decode would otherwise hold the
        GPU lock for many seconds in one blocking call, queueing every
        interactive op (classify/converse/speak/transcribe) behind the 20h
        reflection pass — opener plays, then silence until it finishes.
        Slicing lets an arriving utterance interleave at the next slice
        boundary. It costs the background pass a little wall time and
        nothing else: the pass owns a private prompt cache so slices are
        independent, and greedy decode is deterministic regardless of how
        the work is sliced."""
        import mlx.core as mx
        from mlx_lm import stream_generate
        from mlx_lm.models.cache import make_prompt_cache

        with self._lock:
            self._ensure_llm()
            # Capture the model/tokenizer refs ONCE: the slices below release
            # the lock between chunks (fairness), so a concurrent reload_lora
            # may null self._model mid-pass. Holding our own reference means
            # this pass completes COHERENTLY on the weights it started with
            # (never a crash, never a mixed-weight cache); the reload applies
            # from the next op on.
            model = self._model
            tokenizer = self._tokenizer
            prompt_tokens = tokenizer.encode(self._render_chat(user_text, system=system))
            cache = make_prompt_cache(model)
        # Chunked prefill of all but the last prompt token; stream_generate
        # below receives that final token as its prompt, so its own internal
        # prefill is a single step.
        n = max(len(prompt_tokens) - 1, 0)
        pos = 0
        while pos < n:
            stop = min(pos + prefill_chunk, n)
            with self._lock:
                logits = model(mx.array(prompt_tokens[pos:stop])[None], cache=cache)
                mx.eval(logits)
            pos = stop
            # threading.Lock has no fairness: without this yield the loop
            # re-acquires before a queued interactive op ever wakes (measured
            # — a concurrent classify waited out the whole pass). ~2ms per
            # slice is noise against the slice's own GPU time.
            time.sleep(0.002)
        pieces = []
        gen = stream_generate(
            model,
            tokenizer,
            prompt=prompt_tokens[n:],
            max_tokens=max_tokens,
            prompt_cache=cache,
        )
        try:
            exhausted = False
            while not exhausted:
                with self._lock:
                    for _ in range(decode_chunk):
                        resp = next(gen, None)
                        if resp is None:
                            exhausted = True
                            break
                        pieces.append(resp.text)
                time.sleep(0.002)  # yield to queued interactive ops (no lock fairness)
        finally:
            gen.close()
        return "".join(pieces)

    def _build_generate_messages(
        self, text, history=None, facts=None, data=None, opener_spoken=None, persona_override=None
    ):
        """Assemble the chat message list per the shared generate contract.
        opener_spoken (converse only): the opener line the daemon already
        played aloud; appends the continuation note so the reply skips any
        further acknowledgement — overriding the persona's short-opener-first
        rule by design.
        persona_override (converse only): when set, this agent persona text
        replaces the base self.persona as the system prefix for this call."""
        system = persona_override if persona_override is not None else (self.persona or "")
        if facts:
            system += "\n\nWhat you know about the user (from prior conversations):\n" + "\n".join(
                f"- {fact}" for fact in facts
            )
        user_text = text
        if data:
            user_text += (
                "\n\n[Verified system data — convey this naturally in your own words; "
                "do not invent or alter numbers, names, or application names. If the "
                "data names an app or browser, use exactly that name; if it says "
                "'default browser', say exactly that — never guess which one it is: "
                f"{data}]"
            )
        if opener_spoken:
            user_text += "\n\n" + OPENER_SPOKEN_NOTE.format(opener=opener_spoken)
        messages = []
        if system:
            messages.append({"role": "system", "content": system})
        for turn in history or []:
            role = "user" if turn.get("speaker") == "user" else "assistant"
            messages.append({"role": role, "content": str(turn.get("text", ""))})
        messages.append({"role": "user", "content": user_text})
        return messages

    def generate(
        self,
        text,
        max_tokens=GENERATE_DEFAULT_MAX_TOKENS,
        history=None,
        facts=None,
        data=None,
        local_model=None,
    ):
        text_out, _meta = self.generate_with_meta(
            text, max_tokens, history=history, facts=facts, data=data, local_model=local_model
        )
        return text_out

    def generate_with_meta(
        self,
        text,
        max_tokens=GENERATE_DEFAULT_MAX_TOKENS,
        history=None,
        facts=None,
        data=None,
        local_model=None,
        cancelled=None,
    ):
        """Like `generate`, but ALSO returns an HONEST per-call meta dict reporting
        the path that ACTUALLY ran: `{"speculative": <bool actually used>,
        "quant": <quant that actually loaded>}`. #37 SPECULATIVE DECODING seams in
        HERE: if speculative is on AND a draft model is configured AND loadable
        (the import/availability guard), the uncached generate threads the draft
        through mlx_lm; otherwise NORMAL generation. The cached persona path does
        NOT use a draft (speculative + a prefilled prompt cache conflict), so a
        speculative turn answers UNCACHED — reported truthfully either way. The
        meta NEVER claims speculative when normal gen ran, and reports the quant
        from the load seam (#39). The real speedup/RAM effect is device-gated.

        `cancelled` (optional, audit fix): thread-safe peer-disconnect probe,
        honoured ONLY on the cached persona path (the common route), whose
        decode is already a chunked loop. The uncached and speculative paths
        are single blocking mlx_lm.generate() calls with no seam to poll, so
        they honestly keep run-to-completion behavior."""
        from mlx_lm import generate as mlx_generate

        with self._lock:
            # Resolve the LOCAL model for this turn (Local-tier sub-choice). The
            # base persona KV cache is built/used ONLY on the base model; a
            # non-base warm model answers uncached on its own resident weights.
            resolved_id, model, tokenizer = self._ensure_local_llm(local_model)
            on_base = resolved_id == self.llm_id
            quant_loaded = self._quant_loaded if on_base else "auto"
            # SELF-DISTILLATION honesty: the promoted personal adapter is fused
            # into the BASE resident only — a non-base warm model answers on its
            # own unadapted weights, so its meta reports adapter=None. The op
            # reply takes THIS per-turn stamp, never the engine-global one.
            turn_adapter = self._active_adapter if on_base else None
            messages = self._build_generate_messages(text, history=history, facts=facts, data=data)
            prompt = self._render_chat_messages(messages, tokenizer=tokenizer)
            # #37: decide whether speculative ACTUALLY runs this turn. A draft only
            # rides the BASE model (the draft proposes for the base it shadows);
            # a non-base warm model answers normally. Probe the draft load (the
            # import/availability guard) and use the PURE should_use_speculative
            # decision so the report matches the path taken.
            draft = self._ensure_draft() if on_base else None
            spec_active = should_use_speculative(self.speculative, self.draft_model_id, draft is not None)
            if on_base and self._gen_cache is not None and not spec_active:
                # Cached normal path (today's behavior). Speculative is incompatible
                # with the prefilled persona cache, so a speculative turn skips it.
                try:
                    out, metrics = self._generate_cached(prompt, max_tokens, cancelled=cancelled)
                    # `metrics` carries the honest decode telemetry (tok/s + peak
                    # mem) mlx_lm measured on this stream_generate run; None only
                    # if nothing was decoded (immediate cancel).
                    return (out, {"speculative": False, "quant": quant_loaded, "adapter": turn_adapter, "metrics": metrics})
                except Exception:
                    log.exception("cached generate failed; rebuilding cache and falling back")
                    try:
                        self._build_generate_cache()
                    except Exception:
                        self._gen_cache = None
            # Uncached path: cache build failed / persona missing, a non-base warm
            # local model is answering, OR speculative decoding is active this turn.
            if spec_active:
                try:
                    out = mlx_generate(
                        model,
                        tokenizer,
                        prompt=prompt,
                        max_tokens=max_tokens,
                        sampler=self._persona_sampler(),
                        draft_model=draft[0],
                    )
                    # The speculative/uncached path is a single blocking
                    # mlx_lm.generate() call that returns only text — no
                    # per-token GenerationResponse stream to read tok/s from, so
                    # metrics are honestly absent here (never fabricated). The
                    # benchmark harness measures the speculative delta directly.
                    return (out, {"speculative": True, "quant": quant_loaded, "adapter": turn_adapter, "metrics": None})
                except Exception:
                    # A runtime speculative failure (version mismatch, draft
                    # incompatibility) HONESTLY falls back to normal generation and
                    # reports speculative=False — never a fabricated speculative run.
                    log.exception(
                        "speculative generation failed; falling back to normal generation"
                    )
                    self._draft_unavailable = True
            out = mlx_generate(
                model,
                tokenizer,
                prompt=prompt,
                max_tokens=max_tokens,
                sampler=self._persona_sampler(),
            )
            # Uncached normal path: also a single blocking mlx_lm.generate() call
            # with no per-token stream, so metrics are honestly absent.
            return (out, {"speculative": False, "quant": quant_loaded, "adapter": turn_adapter, "metrics": None})

    def _embed_encode(self, text):
        """Encode one input to a capped list of token ids for embedding. An input
        that tokenizes to nothing (e.g. "") still needs a vector, so it falls back
        to a single space so the forward pass has at least one token. Capped at
        EMBED_MAX_TOKENS so a giant blob cannot hold the GPU lock / blow memory."""
        token_ids = self._tokenizer.encode(text if text else " ")
        if not token_ids:
            token_ids = self._tokenizer.encode(" ")
        return token_ids[:EMBED_MAX_TOKENS]

    def _embed_batch(self, mx, texts):
        """Compute one L2-normalized embedding VECTOR per input by mean-pooling the
        resident LLM's last hidden states ON-DEVICE, as ONE padded forward per
        chunk. Caller holds self._lock and the LLM is loaded. NO new model is
        downloaded: we reuse the already-resident generate model's hidden states
        (the alternative — a dedicated sentence embedding model — would need a
        separate weight download, which the contract forbids).

        Method: encode each text (capped at EMBED_MAX_TOKENS); split into chunks
        capped by BOTH EMBED_BATCH_CHUNK rows and EMBED_CHUNK_TOKEN_BUDGET padded
        tokens (see _pack_embed_chunks); for each chunk right-pad to the chunk's
        max length and run the transformer BODY once (model.model(...) — the decoder
        stack WITHOUT the LM head, which most mlx_lm architectures expose) to get
        per-token last hidden states [chunk, maxlen, hidden]; masked-mean-pool over
        each row's REAL tokens; L2-normalize. Right-padding + the default causal
        mask means a real token attends only to earlier real tokens, so its hidden
        state is identical to the unpadded run. Returns a list of Python
        list[float] in input order. Deterministic (a forward pass, no sampling)."""
        inner = getattr(self._model, "model", None)
        if inner is None or not callable(inner):
            raise ValueError(
                "this model does not expose an inner decoder stack for embeddings"
            )
        encoded = [self._embed_encode(t) for t in texts]
        vectors = [None] * len(encoded)
        # Plan chunks LENGTH-SORTED and bounded by BOTH caps (rows AND padded
        # tokens), then write each result back to its ORIGINAL slot — output
        # order is untouched. See _plan_embed_batches / _pack_embed_chunks.
        for indices in _plan_embed_batches([len(ids) for ids in encoded]):
            chunk = [encoded[i] for i in indices]
            lengths = [len(ids) for ids in chunk]
            maxlen = max(lengths)
            # Pad id is irrelevant to the result: pad positions are never attended
            # by real tokens (causal mask) and are excluded from the mean-pool.
            padded = [ids + [0] * (maxlen - len(ids)) for ids in chunk]
            hidden = inner(mx.array(padded))  # [chunk, maxlen, hidden]
            normed = _pool_normalize(mx, hidden, lengths)  # [chunk, hidden]
            mx.eval(normed)
            for i, row in zip(indices, normed.tolist()):
                vectors[i] = [float(x) for x in row]
        return vectors

    def _embed_one(self, mx, text):
        """One L2-normalized embedding VECTOR for `text`. Thin wrapper over
        [`_embed_batch`] (a batch of one) so the single- and multi-text paths share
        one implementation. Caller holds self._lock and the LLM is loaded."""
        return self._embed_batch(mx, [text])[0]

    def _ensure_coreml_embedder(self):
        """Lazy-construct + return the Core ML bge embedder (convert-on-first-use,
        cached under the HF cache root). Raises coreml_embed.CoreMLEmbedderUnavailable
        if the deps/model cannot be built or loaded — the caller catches it, marks
        the backend unavailable (no retry storm), and falls back to the 4B path.
        Guarded by _embed_lifecycle_lock so two threads never convert at once."""
        from coreml_embed import CoreMLEmbedder

        with self._embed_lifecycle_lock:
            if self._coreml_embedder is None:
                self._coreml_embedder = CoreMLEmbedder()
            emb = self._coreml_embedder
        emb.ensure_loaded()  # convert-on-first-use / load (own lock); may raise
        return emb


    def _embed_4b(self, texts):
        """The LEGACY op=embed backend: one L2-normalized VECTOR per input,
        computed ON-DEVICE by mean-pooling the resident LLM's last hidden states.
        Reuses the already-loaded generate model so NO new model is downloaded.
        One padded forward per chunk of EMBED_BATCH_CHUNK; holds the GPU lock for
        the batch like the other LLM ops. Deterministic."""
        import mlx.core as mx

        with self._lock:
            self._ensure_llm()
            return self._embed_batch(mx, texts)

    def _meanpool_space_id(self):
        """The MODEL-DERIVED vector-space id EMITTED for the mean-pool path. The
        space is defined by the RESIDENT model's weights, so the id embeds the
        model id + the quant that actually loaded (quant changes weights). A
        fixed string would keep matching after an [models].llm or quant swap
        while the vectors moved to a different space; deriving the id from the
        resident model means any swap changes the stamp, so a store from the old
        model is correctly treated as a different space (never silently cosine-
        compared). A FUSED PERSONAL ADAPTER changes the decoder weights too —
        the very stack the mean-pool forwards through — so the ACTIVE adapter
        stamp is part of the id: a promote (or rollback) moves the space and the
        stamp moves with it, exactly like a quant swap. `_quant_loaded` /
        `_active_adapter` are set once the LLM is loaded; before that they fall
        back to the requested quant / no adapter (only reachable on an empty
        batch, which indexes nothing — the returned id for a real batch is read
        AFTER the forward)."""
        quant = self._quant_loaded or self.quant or "auto"
        base = f"llm-meanpool:{self.llm_id}:{quant}"
        return f"{base}:{self._active_adapter}" if self._active_adapter else base

    def embed_with_meta(self, texts):
        """Return (vectors, embedder_id, dim, fell_back) for op=embed — the batch
        embedded by the ACTIVE backend, plus which vector SPACE those vectors live
        in. This is the retrieval-embedding op MNEMOSYNE's NeuralEmbeddingProvider
        calls; the daemon/docsearch store `embedder_id` + `dim` with the index and
        refuse cross-space comparison.

        Backend = [inference].embedder. DEFAULT is the Core ML bge embedder
        (EMBEDDER_COREML, 384-dim — faster AND higher retrieval quality). HONEST
        FALLBACK: if the Core ML embedder cannot be built/loaded, this serves the
        mean-pool path and reports fell_back=True + a log warn — never silently.
        When the mean-pool path is configured outright, fell_back is False.

        The emitted `embedder_id` is the FIXED Core ML space id on the Core ML
        path, or the MODEL-DERIVED `_meanpool_space_id()` on the mean-pool path
        (so an LLM/quant swap changes the space stamp). `texts` is a list of
        strings; the batch is capped at EMBED_MAX_BATCH and each input at the
        backend's length cap (coreml_embed.SEQ tokens for Core ML,
        EMBED_MAX_TOKENS for mean-pool). Returns vectors in input order; empty
        batch -> []. `dim` is the Core ML fixed dim (384) even for an empty
        batch; for the mean-pool path it is the produced vector length (None on
        an empty batch, which indexes nothing). Vectors are finite (NaN/Inf
        scrubbed to 0.0)."""
        if not isinstance(texts, list):
            raise ValueError("'texts' must be a list of strings for op=embed")
        if len(texts) > EMBED_MAX_BATCH:
            raise ValueError(
                f"op=embed batch of {len(texts)} exceeds the {EMBED_MAX_BATCH} cap"
            )
        for t in texts:
            if not isinstance(t, str):
                raise ValueError("'texts' entries must be strings for op=embed")

        # Try the configured Core ML backend; on unavailability, fall back
        # HONESTLY to the mean-pool path and report it.
        if (
            self.embedder_choice == EMBEDDER_COREML
            and not self._coreml_embed_unavailable
        ):
            from coreml_embed import CoreMLEmbedderUnavailable

            try:
                emb = self._ensure_coreml_embedder()
                vectors = emb.embed(texts)  # already scrubbed + L2-normalized
                return (vectors, EMBEDDER_COREML, COREML_EMBED_DIM, False)
            except CoreMLEmbedderUnavailable as e:
                self._coreml_embed_unavailable = True
                log.warning(
                    "Core ML embedder unavailable (%s); falling back to the "
                    "mean-pool path and reporting a model-derived embedder id",
                    e,
                )

        # Mean-pool path. The emitted space id is model-derived; for a non-empty
        # batch it is (re)read AFTER the forward so it reflects the quant that
        # actually loaded.
        _id, _dim, fell_back = plan_embedder(
            self.embedder_choice,
            coreml_available=False,
            meanpool_id=self._meanpool_space_id(),
        )
        if not texts:
            # No work; the mean-pool path's true dim is unknown without a forward.
            return ([], self._meanpool_space_id(), None, fell_back)
        vectors = self._embed_4b(texts)
        dim = len(vectors[0]) if vectors else None
        return (vectors, self._meanpool_space_id(), dim, fell_back)

    def embed(self, texts):
        """Back-compat wrapper: return just the list of embedding VECTORS for
        `texts` (the ACTIVE backend, per [inference].embedder). Callers that also
        need the vector-space identity use [`embed_with_meta`]; op=embed on the
        wire carries the embedder id + dim from there."""
        return self.embed_with_meta(texts)[0]

    def _ensure_coreml_reranker(self):
        """Lazy-construct + return the Core ML cross-encoder reranker (convert-on-
        first-use, cached under the HF cache root). Raises
        coreml_rerank.CoreMLRerankerUnavailable if the deps/model cannot be built or
        loaded — the caller catches it, marks the backend unavailable (no retry
        storm), and reports the honest fallback. Guarded by
        _rerank_lifecycle_lock so two threads never convert at once."""
        from coreml_rerank import CoreMLReranker

        with self._rerank_lifecycle_lock:
            if self._coreml_reranker is None:
                self._coreml_reranker = CoreMLReranker()
            rr = self._coreml_reranker
        rr.ensure_loaded()  # convert-on-first-use / load (own lock); may raise
        return rr

    def rerank_with_meta(self, query, passages):
        """STAGE TWO of the two-stage retrieval stack. Return
        (scores, reranker_id, fell_back) for op=rerank: one cross-encoder relevance
        logit per passage (higher = more relevant; the CALLER re-orders), scored
        against `query` with full query x passage attention by the Core ML
        cross-encoder (RERANKER_COREML). The daemon calls this on the dense top-K it
        just retrieved and re-orders that shortlist by the returned scores.

        HONEST FALLBACK: if the cross-encoder cannot be built/loaded, this reports
        fell_back=True with an EMPTY reranker id (no model produced an order) and
        scores that PRESERVE the input order (descending), so a caller that sorts
        gets the dense order back unchanged and the daemon keeps dense order — never
        silently. Once a build/load hard-fails it is not retried (no conversion storm).

        `query` is a string; `passages` a list of strings, capped at
        RERANK_MAX_PASSAGES and each pair length-capped at coreml_rerank.SEQ tokens
        (the passage is truncated first, surfaced via a throttled server-side warn).
        Empty passage list -> ([], RERANKER_COREML, False). Scores are finite
        (NaN/Inf scrubbed)."""
        if not isinstance(query, str):
            raise ValueError("'query' must be a string for op=rerank")
        if not isinstance(passages, list):
            raise ValueError("'passages' must be a list of strings for op=rerank")
        if len(passages) > RERANK_MAX_PASSAGES:
            raise ValueError(
                f"op=rerank batch of {len(passages)} passages exceeds the "
                f"{RERANK_MAX_PASSAGES} cap"
            )
        for p in passages:
            if not isinstance(p, str):
                raise ValueError("'passages' entries must be strings for op=rerank")
        if not passages:
            # Nothing to score; a real (non-fallback) empty result.
            return ([], RERANKER_COREML, False)

        if not self._coreml_rerank_unavailable:
            from coreml_rerank import CoreMLRerankerUnavailable

            try:
                rr = self._ensure_coreml_reranker()
                scores = rr.rerank(query, passages)  # already scrubbed finite
                return (scores, RERANKER_COREML, False)
            except CoreMLRerankerUnavailable as e:
                self._coreml_rerank_unavailable = True
                log.warning(
                    "Core ML reranker unavailable (%s); op=rerank falling back to "
                    "dense order (fell_back)", e,
                )

        # HONEST FALLBACK: order-preserving descending scores + empty reranker id.
        # The daemon keys on fell_back and keeps its dense order regardless; these
        # scores make a naive re-sort a no-op too.
        n = len(passages)
        scores = [float(n - i) for i in range(n)]
        return (scores, "", True)

    def rerank(self, query, passages):
        """Back-compat wrapper: return just the list of relevance SCORES for
        (`query`, `passages`). Callers that also need the reranker identity + the
        honest-fallback flag use [`rerank_with_meta`]; op=rerank on the wire carries
        the reranker id + fell_back from there."""
        return self.rerank_with_meta(query, passages)[0]

    # -- on-device vision-language model (op=describe_image) ------------
    # _ensure_vlm must be called with self._lock held.

    def _ensure_vlm(self):
        """Lazy-load the on-device VLM (mlx-vlm + the checkpoint), caching it as
        resident for subsequent calls. Returns a dict of {load, generate,
        apply_chat_template, load_config, model, processor, config} on success,
        or None when the model cannot run:
          - self.vlm_id is empty  -> the op is disabled  -> None
          - mlx-vlm not installed -> _load_mlx_vlm() is None -> None
          - the checkpoint is not downloaded / fails to load -> None
        A None return is the SHIPS-OFF / unavailable state, NOT an error: the
        caller turns it into an honest structured "unavailable" response and the
        daemon falls back (OCR/classification). The model load is the only place
        that could touch the disk cache; it never reaches the network here unless
        the operator has chosen to download the checkpoint."""
        if not self.vlm_id:
            return None
        vlm = _load_mlx_vlm()
        if vlm is None:
            # mlx-vlm is not installed: the normal ships-OFF state.
            return None
        if self._vlm_model is None:
            try:
                log.info("loading on-device VLM %s (resident after first use)...", self.vlm_id)
                t0 = time.perf_counter()
                model, processor = vlm["load"](self.vlm_id)
                config = vlm["load_config"](self.vlm_id)
                log.info("VLM loaded in %.1fs", time.perf_counter() - t0)
                self._vlm_model = model
                self._vlm_processor = processor
                self._vlm_config = config
            except Exception:
                # Checkpoint missing/corrupt or any load failure: report
                # UNAVAILABLE (the daemon falls back) rather than crashing the
                # op. Never partially cache.
                log.exception(
                    "VLM %s could not be loaded; op=describe_image will report unavailable",
                    self.vlm_id,
                )
                self._vlm_model = None
                self._vlm_processor = None
                self._vlm_config = None
                return None
        return {
            "load": vlm["load"],
            "generate": vlm["generate"],
            "apply_chat_template": vlm["apply_chat_template"],
            "load_config": vlm["load_config"],
            "model": self._vlm_model,
            "processor": self._vlm_processor,
            "config": self._vlm_config,
        }

    def describe_image(self, path, question=None, max_tokens=None):
        """Describe / answer a question ABOUT a local image with the on-device
        vision-language model (distinct from OCR: OCR reads text glyphs; this
        REASONS about the visual scene). Returns a structured dict:

          available:   {"ok": True, "text": <description/answer str>,
                        "model": <vlm_id>}
          unavailable: {"ok": False, "reason": DESCRIBE_IMAGE_UNAVAILABLE_REASON,
                        "error": <honest human message>}

        It NEVER fabricates a description: if mlx-vlm or the checkpoint is
        absent, it returns the unavailable structure so the daemon can fall back
        honestly. HONESTY: the image at `path` is read LOCALLY and handed only to
        the on-device MLX VLM — the pixels NEVER leave the device.

        `path`      absolute path to a local image file (the daemon path-confines
                    it before calling; we re-validate existence here).
        `question`  optional VQA question; absent => a general scene description.
        `max_tokens` optional decode budget (capped at
                    DESCRIBE_IMAGE_MAX_TOKENS_CAP)."""
        if not path or not isinstance(path, str):
            raise ValueError("'path' is required for op=describe_image")
        if question is not None and not isinstance(question, str):
            raise ValueError("'question' must be a string for op=describe_image")
        if max_tokens is None:
            max_tokens = DESCRIBE_IMAGE_DEFAULT_MAX_TOKENS
        if not isinstance(max_tokens, int) or max_tokens <= 0:
            raise ValueError("'max_tokens' must be a positive integer for op=describe_image")
        max_tokens = min(max_tokens, DESCRIBE_IMAGE_MAX_TOKENS_CAP)
        # The image must exist locally before we even consider loading a model:
        # a bad path is a caller error, not a VLM-unavailable condition.
        if not os.path.isfile(path):
            raise ValueError(f"image path does not exist or is not a file: {path}")
        prompt = (question or "").strip() or DESCRIBE_IMAGE_DEFAULT_PROMPT
        if len(prompt) > DESCRIBE_IMAGE_MAX_QUESTION_CHARS:
            prompt = prompt[:DESCRIBE_IMAGE_MAX_QUESTION_CHARS]

        with self._lock:
            vlm = self._ensure_vlm()
            if vlm is None:
                # SHIPS-OFF / unavailable: honest, never a fabricated answer.
                return {
                    "ok": False,
                    "reason": DESCRIBE_IMAGE_UNAVAILABLE_REASON,
                    "error": (
                        "vision-language model not available (on-device VLM is "
                        "off or its model is not downloaded)"
                    ),
                }
            try:
                # Build the chat prompt with one image, then run the VLM. The
                # image is passed BY PATH and read locally inside mlx-vlm; the
                # pixels stay on-device.
                formatted = vlm["apply_chat_template"](
                    vlm["processor"], vlm["config"], prompt, num_images=1
                )
                text = vlm["generate"](
                    vlm["model"],
                    vlm["processor"],
                    formatted,
                    [path],
                    max_tokens=max_tokens,
                    verbose=False,
                )
            except Exception as exc:
                # A runtime failure on a loaded model (decode error, unsupported
                # image, OOM): report unavailable so the daemon falls back
                # honestly rather than failing the turn or inventing text.
                log.exception("op=describe_image generation failed")
                return {
                    "ok": False,
                    "reason": DESCRIBE_IMAGE_UNAVAILABLE_REASON,
                    "error": f"vision-language model failed to produce a description: {type(exc).__name__}",
                }
        # mlx-vlm.generate may return a str or a (text, usage)-style object
        # depending on version; normalize to the text string.
        if isinstance(text, tuple):
            text = text[0] if text else ""
        if not isinstance(text, str):
            text = getattr(text, "text", str(text))
        text = text.strip()
        if not text:
            return {
                "ok": False,
                "reason": DESCRIBE_IMAGE_UNAVAILABLE_REASON,
                "error": "vision-language model returned an empty description",
            }
        return {"ok": True, "text": text, "model": self.vlm_id}

    # -- on-device text->image model (op=generate_image) ----------------
    # _ensure_image_model must be called with self._lock held.

    def _ensure_image_model(self):
        """Lazy-load the on-device diffusion model (the MLX diffusion package +
        the checkpoint), caching it as resident for subsequent calls. Returns a
        dict of {Flux1, Config, model} on success, or None when the model cannot
        run:
          - self.image_model_id is empty -> the op is disabled  -> None
          - the diffusion package not installed -> _load_mlx_diffusion() is None
            -> None
          - the checkpoint is not downloaded / fails to load -> None
        A None return is the SHIPS-OFF / unavailable state, NOT an error: the
        caller turns it into an honest structured "unavailable" response and the
        daemon surfaces an honest message (NO cloud fallback). The model load is
        the only place that could touch the disk cache; it never reaches the
        network here unless the operator has chosen to download the checkpoint."""
        if not self.image_model_id:
            return None
        diff = _load_mlx_diffusion()
        if diff is None:
            # the diffusion package is not installed: the normal ships-OFF state.
            return None
        if self._image_model is None:
            try:
                log.info(
                    "loading on-device image model %s (resident after first use)...",
                    self.image_model_id,
                )
                t0 = time.perf_counter()
                model = diff["Flux1"](model_name=self.image_model_id)
                log.info("image model loaded in %.1fs", time.perf_counter() - t0)
                self._image_model = model
            except Exception:
                # Checkpoint missing/corrupt or any load failure: report
                # UNAVAILABLE (the daemon surfaces an honest message) rather than
                # crashing the op. Never partially cache.
                log.exception(
                    "image model %s could not be loaded; op=generate_image will report unavailable",
                    self.image_model_id,
                )
                self._image_model = None
                return None
        return {
            "Flux1": diff["Flux1"],
            "Config": diff["Config"],
            "model": self._image_model,
        }

    def generate_image(self, prompt, size=None, steps=None, seed=None):
        """Generate an image FROM a text prompt with the on-device MLX diffusion
        model, save it under state/images/ on the device, and return its path.
        Returns a structured dict:

          available:   {"ok": True, "path": <abs path under state/images/>,
                        "model": <image_model_id>, "size": int, "steps": int,
                        "seed": int}
          unavailable: {"ok": False, "reason": GENERATE_IMAGE_UNAVAILABLE_REASON,
                        "error": <honest human message>}

        It NEVER fabricates an image and NEVER calls a cloud image API: if the
        diffusion package or the checkpoint is absent, it returns the unavailable
        structure so the daemon can surface an honest "the on-device image model
        isn't set up" message. HONESTY: the prompt and the generated pixels stay
        on the machine — image generation is 100% ON-DEVICE (MLX diffusion), and
        nothing is sent to the cloud.

        `prompt` the text prompt to render (required, non-empty after strip).
        `size`   optional square output resolution in pixels (bounded by
                 GENERATE_IMAGE_MIN_SIZE..GENERATE_IMAGE_MAX_SIZE).
        `steps`  optional sampling steps (capped at GENERATE_IMAGE_MAX_STEPS_CAP).
        `seed`   optional integer seed for reproducibility (None => a
                 time-derived seed)."""
        if not prompt or not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("'prompt' is required for op=generate_image")
        prompt = prompt.strip()
        if len(prompt) > GENERATE_IMAGE_MAX_PROMPT_CHARS:
            prompt = prompt[:GENERATE_IMAGE_MAX_PROMPT_CHARS]
        if size is None:
            size = GENERATE_IMAGE_DEFAULT_SIZE
        if not isinstance(size, int) or size <= 0:
            raise ValueError("'size' must be a positive integer for op=generate_image")
        size = max(GENERATE_IMAGE_MIN_SIZE, min(size, GENERATE_IMAGE_MAX_SIZE))
        if steps is None:
            steps = GENERATE_IMAGE_DEFAULT_STEPS
        if not isinstance(steps, int) or steps <= 0:
            raise ValueError("'steps' must be a positive integer for op=generate_image")
        steps = min(steps, GENERATE_IMAGE_MAX_STEPS_CAP)
        if seed is not None and not isinstance(seed, int):
            raise ValueError("'seed' must be an integer for op=generate_image")
        if seed is None:
            # A time-derived seed keeps runs distinct without requiring the
            # caller to supply one; bounded so it stays a sane 32-bit value.
            seed = int(time.time() * 1000) & 0x7FFFFFFF

        with self._lock:
            diff = self._ensure_image_model()
            if diff is None:
                # SHIPS-OFF / unavailable: honest, never a fabricated image and
                # never a cloud call.
                return {
                    "ok": False,
                    "reason": GENERATE_IMAGE_UNAVAILABLE_REASON,
                    "error": (
                        "image model not available (on-device image generation "
                        "is off or its model is not downloaded)"
                    ),
                }
            try:
                # Build the diffusion config and run generation entirely
                # on-device. The prompt is handed ONLY to the local MLX model;
                # nothing is sent to the cloud.
                config = diff["Config"](
                    num_inference_steps=steps,
                    height=size,
                    width=size,
                )
                generated = diff["model"].generate_image(
                    seed=seed,
                    prompt=prompt,
                    config=config,
                )
                # Save the image to an on-device path under state/images/. The
                # generator returns an object with a .save(path); we never send
                # the pixels anywhere.
                IMAGES_DIR.mkdir(parents=True, exist_ok=True)
                out_path = IMAGES_DIR / f"image-{next(self._tts_counter)}.png"
                generated.save(path=str(out_path))
            except Exception as exc:
                # A runtime failure on a loaded model (decode error, OOM):
                # report unavailable so the daemon surfaces an honest message
                # rather than failing the turn or inventing an image. NEVER a
                # cloud fallback.
                log.exception("op=generate_image generation failed")
                return {
                    "ok": False,
                    "reason": GENERATE_IMAGE_UNAVAILABLE_REASON,
                    "error": f"image model failed to produce an image: {type(exc).__name__}",
                }
        if not os.path.isfile(out_path):
            # The generator claimed success but no file landed on disk: treat as
            # unavailable rather than returning a path to nothing.
            return {
                "ok": False,
                "reason": GENERATE_IMAGE_UNAVAILABLE_REASON,
                "error": "image model did not write an output file",
            }
        return {
            "ok": True,
            "path": str(out_path),
            "model": self.image_model_id,
            "size": size,
            "steps": steps,
            "seed": seed,
        }

    def extract_facts(self, text, response=None):
        """Extract at most 3 durable namespaced facts about the user from one
        exchange. Facts come from the user's words ONLY; the assistant reply is
        disambiguation context, never a source (see EXTRACT_FACTS_SYSTEM).
        Empty list when nothing qualifies or output is unparseable."""
        prompt = f"User said: {text}"
        if response:
            prompt += (
                "\nAssistant replied (context for disambiguation ONLY — never a "
                f"source of facts): {response}"
            )
        prompt += "\n\nJSON array:"
        raw = self._run_llm(prompt, EXTRACT_FACTS_MAX_TOKENS, system=EXTRACT_FACTS_SYSTEM)
        return parse_facts(raw)

    def consolidate(self, transcripts, facts):
        """Self-learning reflection pass: merge/clean the facts table against
        recent transcripts, and mine clearly recurring user patterns into
        "user.habit.<slug>" facts (prompted, then enforced by the
        habit_supported backstop — same parse layer, same caps; >=3
        occurrences across the provided transcripts, counted from the user's
        lines only, never the assistant's; habit facts are deletable like any
        other). Returns {"upserts": [{"key","value"}],
        "deletes": [key]} — empty arrays whenever the model output is
        unparseable or there is nothing to do. Greedy decode (determinism is
        a feature for strict-JSON ops). "meta."-prefixed input facts are
        ignored entirely: they are bookkeeping (e.g. meta.last_reflection),
        never memory."""
        facts = [f for f in facts if not f["key"].startswith("meta.")]
        if not facts:
            # Nothing to merge or delete; without stored facts every valid
            # delete is impossible and upserts would be invention by
            # definition of this op (extract_facts owns new-fact learning).
            return {"upserts": [], "deletes": []}
        lines = ["Stored facts:"]
        lines.extend(f"- {f['key']} = {f['value']}" for f in facts)
        lines.append("")
        lines.append("Recent transcripts (oldest first):")
        if transcripts:
            for turn in transcripts:
                lines.append(f"User: {turn['user']}")
                lines.append(f"Assistant: {turn['darwin']}")
        else:
            lines.append("(none)")
        lines.append("")
        lines.append("JSON object:")
        # Interruptible variant: this is the one op long enough (multi-
        # thousand-token uncached prefill) that holding the GPU lock end-to-
        # end would stall interactive utterances behind a background pass.
        raw = self._run_llm_interruptible(
            "\n".join(lines), CONSOLIDATE_MAX_TOKENS, system=CONSOLIDATE_SYSTEM
        )
        result = parse_consolidation(raw, {f["key"]: f["value"] for f in facts})
        # Deterministic habit backstop on top of the parse guards (which are
        # unchanged): user.habit.* upserts must be evidenced by >= 3 separate
        # user lines in THESE transcripts, or they are dropped. See
        # habit_supported for why the prompt alone cannot be trusted here.
        result["upserts"] = [
            u
            for u in result["upserts"]
            if not u["key"].startswith("user.habit.")
            or habit_supported(u["key"], u["value"], transcripts)
        ]
        return result

    def classify(self, text):
        from mlx_lm import generate as mlx_generate

        with self._lock:
            model, tokenizer = self._ensure_classifier()
            if self._cls_cache is not None:
                try:
                    raw, metrics = self._classify_cached(text)
                    result = parse_classification(raw)
                    # Additive: carry the honest decode telemetry out under a
                    # private key the op handler surfaces; parse_classification
                    # never emits it, so nothing downstream is disturbed.
                    result["_metrics"] = metrics
                    return result
                except Exception:
                    log.exception("cached classify failed; rebuilding cache and falling back")
                    try:
                        self._build_classifier_cache()
                    except Exception:
                        self._cls_cache = None
            # Uncached fallback path (greedy, on the same classify model).
            template = self.classifier_template
            if "{utterance}" in template:
                body = template.replace("{utterance}", text)
            else:
                body = f'{template}\n\nUtterance: "{text}"\nJSON:'
            prompt = self._render_chat(body, tokenizer=tokenizer)
            raw = mlx_generate(
                model, tokenizer, prompt=prompt, max_tokens=CLASSIFY_MAX_TOKENS
            )
        return parse_classification(raw)

    @staticmethod
    def _lang_code(voice):
        # Kokoro voices encode the language as the first letter (am_michael ->
        # American English "a", bf_emma -> British English "b", ...).
        if voice and voice[0] in _KOKORO_LANGS:
            return voice[0]
        return "a"

    @staticmethod
    def _speech_chunks(text, max_chars=280):
        """Split text into synthesis chunks (sentences, comma-split when a
        sentence runs long). The SineGen length-alignment patch
        (_patch_kokoro_sinegen) fixes mlx-audio 0.4.4's vocoder broadcast bug
        directly, so tiny chunks are no longer needed; chunking now only keeps
        single Kokoro calls reasonably sized, with the retry-at-1.0 guard in
        _synthesize_chunk as the backstop."""
        import re

        # Sentence enders followed by whitespace — decimal points ("11.0")
        # have no trailing whitespace, so they survive.
        sentences = [s.strip() for s in re.split(r"(?<=[.!?])\s+", text) if s.strip()]
        chunks = []
        for s in sentences:
            while len(s) > max_chars:
                cut = s.rfind(",", 0, max_chars)
                if cut < max_chars // 2:
                    cut = s.rfind(" ", 0, max_chars)
                if cut <= 0:
                    cut = max_chars
                chunks.append(s[:cut].strip())
                s = s[cut:].lstrip(", ")
            if s:
                chunks.append(s)
        return chunks or [text]

    def _synthesize_chunk(self, tts, chunk, voice):
        """One short Kokoro generate; retry once at speed 1.0 (the vocoder bug
        is length-dependent and a speed change shifts segment lengths)."""
        import numpy as np

        for speed in (self.speed, 1.0):
            try:
                return [
                    np.asarray(r.audio, dtype=np.float32)
                    for r in tts.generate(
                        text=chunk, voice=voice, speed=speed, lang_code=self._lang_code(voice)
                    )
                ]
            except ValueError as e:
                if "broadcast_shapes" not in str(e) or speed == 1.0:
                    raise
                log.warning("kokoro length bug on chunk %r at speed %s; retrying at 1.0", chunk, speed)
        return []

    # -- per-engine synthesis (one synth function per engine; speak, converse
    # -- and the opener bank all dispatch through _tts_synth_fn) ----------

    def _synth_kokoro(self, tts, text, voice):
        """Kokoro-82M: chunked synthesis with the SineGen patch active and the
        retry-at-1.0 guard per chunk. Honors [speech].speed."""
        chunks = []
        for piece in self._speech_chunks(text):
            chunks.extend(self._synthesize_chunk(tts, piece, voice))
        return chunks

    def _synth_csm(self, tts, text, voice):
        """Sesame CSM: voice selects a speaker prompt (conversational_b =
        male), passed as ref_audio/ref_text from the ungated mirror — see
        CSM_PROMPT_REPO (mlx-audio's own voice= path needs HF auth).
        hf_hub_download is cache-first, so only the first call ever touches
        the network. The model has no speed control; [speech].speed is
        ignored on this engine."""
        import numpy as np
        from huggingface_hub import hf_hub_download

        ref_text = CSM_PROMPT_TEXTS.get(voice)
        if ref_text is None:
            raise ValueError(
                f"unsupported CSM voice {voice!r}; supported: {sorted(CSM_PROMPT_TEXTS)}"
            )
        ref_path = hf_hub_download(repo_id=CSM_PROMPT_REPO, filename=f"prompts/{voice}.wav")
        return [
            np.asarray(r.audio, dtype=np.float32)
            for r in tts.generate(text=text, ref_audio=ref_path, ref_text=ref_text)
        ]

    def _synth_orpheus(self, tts, text, voice):
        """Orpheus 3B ft: voice is a named speaker tag prepended to the
        prompt (leo/dan/zac are male). No speed control; [speech].speed is
        ignored on this engine."""
        import numpy as np

        return [
            np.asarray(r.audio, dtype=np.float32)
            for r in tts.generate(text=text, voice=voice)
        ]

    def _tts_synth_fn(self):
        """Resolve the synth function for the active engine. engine_name is
        validated against TTS_ENGINE_DEFAULTS in __init__, so this lookup
        cannot miss."""
        return {
            "kokoro": self._synth_kokoro,
            "csm": self._synth_csm,
            "orpheus": self._synth_orpheus,
        }[self.engine_name]

    @staticmethod
    def _tts_sample_rate(tts):
        """Model sample rate: sesame/kokoro expose .sample_rate, orpheus
        carries it on .config; 24000 is every supported engine's native
        rate, kept as the final fallback."""
        rate = getattr(tts, "sample_rate", None)
        if not rate:
            rate = getattr(getattr(tts, "config", None), "sample_rate", None)
        return int(rate or 24000)

    @staticmethod
    def _trim_silence(audio, sample_rate, threshold=TRIM_AMPLITUDE, pad_ms=TRIM_PAD_MS):
        """Trim leading/trailing samples with |amplitude| < threshold, keeping
        pad_ms of padding each side. Kokoro bakes ~0.26s of leading and ~0.46s
        of trailing silence into every WAV; trimmed, the residual lead/tail
        are each <= 120ms, which is what makes back-to-back sentence playback
        gapless. All-silent audio (no sample reaches the threshold) returns
        None: there is nothing speakable in it, and _synthesize_to_wav treats
        it exactly like the engine producing no audio at all."""
        import numpy as np

        loud = np.flatnonzero(np.abs(audio) >= threshold)
        if loud.size == 0:
            return None
        pad = int(sample_rate * pad_ms / 1000)
        start = max(0, int(loud[0]) - pad)
        end = min(len(audio), int(loud[-1]) + 1 + pad)
        return audio[start:end]

    @staticmethod
    def _apply_fades(audio, fade_in=FADE_IN_SAMPLES, fade_out=FADE_OUT_SAMPLES):
        """Linear fade-in/fade-out at the clip edges, applied AFTER the
        silence trim on the float array (before int16 conversion). The first
        and last samples come out exactly 0 — |amp| < 0.002 at both edges of
        every emitted WAV — so the daemon's clip joins are click-free.
        Operates on a copy. Clips shorter than a fade window get a
        proportionally shorter ramp; when the two windows overlap the ramps
        multiply, which only fades harder (never a discontinuity)."""
        import numpy as np

        n = len(audio)
        if n == 0:
            return audio
        audio = np.asarray(audio, dtype=np.float32).copy()
        fade_in = min(int(fade_in), n)
        if fade_in > 0:
            # endpoint=False: ramp 0 .. (k-1)/k, continuous with the unfaded
            # factor 1.0 at the sample after the window; first sample is 0.
            audio[:fade_in] *= np.linspace(0.0, 1.0, fade_in, endpoint=False, dtype=np.float32)
        fade_out = min(int(fade_out), n)
        if fade_out > 0:
            # Ramp 1 .. 0 inclusive: the window starts at factor 1.0
            # (continuous with the preceding samples), last sample is 0.
            audio[-fade_out:] *= np.linspace(1.0, 0.0, fade_out, dtype=np.float32)
        return audio

    def _write_wav(self, audio, sample_rate, out_path=None):
        """Write float32 mono audio to a 16-bit WAV: a fresh file under
        state/tmp/ by default, or exactly `out_path` (opener bank, audition
        samples) when given. Every WAV emitted by this server funnels through
        here, so the edge fades (_apply_fades) are applied here — after the
        caller's silence trim, before int16 conversion — guaranteeing
        near-zero first/last samples on speak clips, converse sentences,
        openers and audition samples alike.

        The write is atomic: bytes go to '<name>.tmp' and os.replace() lands
        the finished file on the final path. Opener-bank paths are read by
        the daemon at any instant across server restarts, and a directly
        written WAV is briefly a valid-looking header over truncated data;
        with the rename, a reader sees either no file or a complete one. On
        any failure the .tmp is best-effort unlinked, so a failed write
        leaves nothing behind on either path."""
        import wave

        import numpy as np

        pcm = (np.clip(self._apply_fades(audio), -1.0, 1.0) * 32767.0).astype(np.int16)
        if out_path is None:
            TMP_DIR.mkdir(parents=True, exist_ok=True)
            path = TMP_DIR / f"tts-{next(self._tts_counter)}.wav"
        else:
            path = Path(out_path)
            path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = path.with_name(path.name + ".tmp")
        try:
            with wave.open(str(tmp_path), "wb") as wf:
                wf.setnchannels(1)
                wf.setsampwidth(2)
                wf.setframerate(sample_rate)
                wf.writeframes(pcm.tobytes())
            os.replace(tmp_path, path)
        except BaseException:
            try:
                tmp_path.unlink()
            except OSError:
                pass
            raise
        return str(path)

    @staticmethod
    def _apply_rate(audio, sample_rate, rate):
        """COARSE speaking-rate change (#33 prosody) for the on-device path, honoured
        by time-stretching via nearest-sample resampling at the SAME output sample
        rate (faster rate -> fewer samples -> quicker speech). A None/1.0/out-of-range
        `rate` leaves the audio UNTOUCHED (byte-for-byte today's). PURE; no network.
        This is the honest coarse rate the backend honours — never a fabricated tag."""
        import numpy as np

        if not isinstance(rate, (int, float)) or isinstance(rate, bool):
            return audio
        r = float(rate)
        if not (0.5 <= r <= 2.0) or abs(r - 1.0) < 1e-3 or audio.size == 0:
            return audio
        n_out = max(1, int(round(audio.shape[0] / r)))
        idx = np.minimum((np.arange(n_out) * r).astype(np.int64), audio.shape[0] - 1)
        return audio[idx]

    def _synthesize_to_wav(self, tts, text, voice, out_path=None, rate=None, volume=None):
        """Synthesize `text` through the active engine's synth function,
        silence-trim the result, and write it to a WAV (under state/tmp/, or
        at `out_path` when given). Returns the absolute path, or None when
        the engine produced no audio (e.g. punctuation-only text) OR audio
        that is entirely below the trim threshold — dead air must not become
        a sentence event, a speak reply or a "ready" opener. Caller holds
        self._lock.

        #33/#34 COARSE delivery: `rate` time-stretches the speech and `volume`
        applies a soft-delivery gain — both honoured on this on-device path (the
        coarse hints the backend can do; rich audio-tags are EL-v3-only). With both
        unset (the OFF default) this is byte-for-byte today's synth."""
        import numpy as np

        sample_rate = self._tts_sample_rate(tts)
        # Speakable normalization at the single synth point: turns "apple.com"
        # into "apple dot com" for EVERY path (speak op, converse sentences,
        # openers) so domains aren't read as "apple, com".
        text = _speakable(text)
        # No inserted silence between chunks: the engine's own sentence
        # prosody carries the pauses; injected gaps read as stutter.
        chunks = self._tts_synth_fn()(tts, text, voice)
        if not chunks:
            return None
        audio = np.concatenate(chunks)
        if audio.size == 0:
            return None
        audio = self._trim_silence(audio, sample_rate)
        if audio is None:
            return None
        # Coarse rate then gain (order is irrelevant — gain is sample-wise). Both are
        # no-ops at the neutral default, keeping the default WAV untouched.
        audio = self._apply_rate(audio, sample_rate, rate)
        audio = self._apply_gain(audio, volume)
        return self._write_wav(audio, sample_rate, out_path)

    def generate_openers(self):
        """Regenerate the opener bank: clear state/openers/ and synthesize
        each [speech].openers line to opener-<idx>.wav with the current
        engine/voice/speed, silence-trimmed like every other WAV. Runs inside
        preload after the TTS warm. The daemon maps filename index back to
        the configured openers text, so indexes always match config order —
        a line that fails to synthesize leaves a gap, never a shift. Failure
        to prepare the directory — including any stale file that survives
        the clear below — skips synthesis entirely rather than risking
        previous-run WAVs being misindexed against the current config."""
        try:
            OPENERS_DIR.mkdir(parents=True, exist_ok=True)
            stale = [p for p in OPENERS_DIR.iterdir() if p.is_file()]
        except OSError:
            log.exception("openers: cannot prepare %s; opener bank unavailable", OPENERS_DIR)
            return 0
        # Best-effort clear: attempt EVERY stale file (aborting on the first
        # error would strand previous-run WAVs — old engine/voice/text —
        # whose filename indexes the daemon would map onto the CURRENT
        # config's openers list, the exact desync the whole-bank validation
        # exists to prevent). Only synthesize into a verifiably cleared bank.
        survivors = []
        for old in stale:
            try:
                old.unlink()
            except OSError:
                survivors.append(old)
        if survivors:
            log.error(
                "openers: could not remove stale files (%s); opener bank "
                "unavailable — stale WAVs would be misindexed against the "
                "current openers config",
                ", ".join(p.name for p in survivors),
            )
            return 0
        t0 = time.perf_counter()
        ready = 0
        for idx, line in enumerate(self.openers):
            try:
                with self._lock:
                    tts = self._ensure_tts()
                    path = self._synthesize_to_wav(
                        tts, line, self.voice, out_path=OPENERS_DIR / f"opener-{idx}.wav"
                    )
                if path is None:
                    log.warning("openers: no audio for opener %d %r; skipped", idx, line)
                else:
                    ready += 1
            except Exception:
                log.exception("openers: synthesis failed for opener %d %r; skipped", idx, line)
                # A failed line must leave a GAP, never a present-but-corrupt
                # file: best-effort unlink whatever may sit at this index.
                try:
                    (OPENERS_DIR / f"opener-{idx}.wav").unlink()
                except OSError:
                    pass
        log.info(
            "openers: bank ready — %d/%d WAVs in %s (engine=%s voice=%s speed=%s, %dms)",
            ready,
            len(self.openers),
            OPENERS_DIR,
            self.engine_name,
            self.voice,
            self.speed,
            int((time.perf_counter() - t0) * 1000),
        )
        return ready

    @staticmethod
    def _resolve_elevenlabs_model(model, lang):
        """Pick the ElevenLabs TTS model_id for this speak request. An explicit
        non-empty `model` from the daemon wins (the daemon's choice is honored
        verbatim). Otherwise, when `lang` names a NON-English language, use the
        multilingual model; an absent/English lang uses the English-centric
        default. PURE; unit-testable in isolation (no network, no state)."""
        if isinstance(model, str) and model.strip():
            return model
        if _is_non_english_lang(lang):
            return ELEVENLABS_MULTILINGUAL_MODEL
        return ELEVENLABS_DEFAULT_MODEL

    def clone_voice(self, path, name, el_key):
        """CONSENT-GATED voice cloning: upload the owner audio SAMPLE at `path`
        to ElevenLabs (/v1/voices/add, name=`name`, xi-api-key from `el_key`) and
        return the new voice_id string, or None on any failure (the daemon then
        keeps Kokoro / the user's existing voice — never fails the turn).

        The daemon only calls this after an EXPLICIT, authorization-bound consent
        confirm on a user-owned sample (no impersonation, never automatic). HONESTY:
        the audio sample LEAVES the device here. The voice_id is NON-secret (the
        daemon stores it like any EL voice id); the key is secret and is used only
        to call the seam — never logged here. Blocking; runs on a worker thread."""
        name = (name or "").strip()
        try:
            voice_id = _elevenlabs_clone_voice(name, path, el_key)
            if isinstance(voice_id, str) and voice_id:
                return voice_id
            log.warning("ElevenLabs clone produced no voice_id; user keeps their existing voice")
            return None
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/sample, decode)
            # -> no clone. Log only the redacted exception CLASS — never exc_info /
            # the message (could echo a key-bearing URL fragment or the sample path).
            log.warning(
                "ElevenLabs voice clone failed (%s); user keeps their existing voice",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    def design_voice(self, description, name, el_key):
        """PROMPT-TO-VOICE design: mint a brand-new voice from the TEXT
        `description` (display name `name`) via ElevenLabs text-to-voice and return
        the new voice_id string, or None on any failure (the daemon then keeps
        Kokoro / the user's existing voice — never fails the turn).

        Unlike clone_voice, NO audio sample leaves the device — the voice is
        synthesized purely from the text description. The voice_id is NON-secret
        (the daemon stores it like any EL voice id); the key is secret and is used
        only to call the seam — never logged here. Blocking; runs on a worker
        thread."""
        description = (description or "").strip()
        name = (name or "").strip()
        try:
            voice_id = _elevenlabs_design_voice(description, name, el_key)
            if isinstance(voice_id, str) and voice_id:
                return voice_id
            log.warning("ElevenLabs voice design produced no voice_id; user keeps their existing voice")
            return None
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/description,
            # decode) -> no voice. Log only the redacted exception CLASS — never
            # exc_info / the message (could echo a key-bearing URL fragment).
            log.warning(
                "ElevenLabs voice design failed (%s); user keeps their existing voice",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    def create_pronunciation(self, name, rules, el_key):
        """Mint a NON-secret pronunciation dictionary from `name` + `rules` via
        ElevenLabs add-from-rules and return the (dictionary_id, version_id) pair, or
        None on any failure (the daemon then simply has no dictionary to apply —
        never fails a turn, never fabricates an id).

        Unlike clone_voice, NO audio leaves the device — the dictionary is built
        purely from the text rules. Both ids are NON-secret (the daemon stores them
        like any EL id and later threads them into a speak as pronunciation locators);
        the key is secret and is used only to call the seam — never logged here.
        Blocking; runs on a worker thread."""
        try:
            result = _elevenlabs_create_pronunciation(name, rules, el_key)
            if (isinstance(result, tuple) and len(result) == 2
                    and all(isinstance(x, str) and x for x in result)):
                return result
            log.warning("ElevenLabs pronunciation dictionary produced no id; nothing to apply")
            return None
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/name/rules, decode)
            # -> no dictionary. Log only the redacted exception CLASS — never exc_info
            # / the message (could echo a key-bearing URL fragment).
            log.warning(
                "ElevenLabs pronunciation dictionary failed (%s); nothing to apply",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    def sound_effect(self, prompt, el_key, duration_s=None, prompt_influence=None):
        """Generate a NON-speech SOUND-EFFECT cue from `prompt` via ElevenLabs
        sound-generation and return a pipeline WAV path, or None on any failure.

        There is NO on-device SFX generator, so a failure (or no key) yields None
        and the caller treats it as 'unavailable' — never a fabricated/placeholder
        cue, never a crash. The PCM bytes flow through the IDENTICAL _write_wav path
        as TTS (fades, atomic write, pipeline WAV). The key rides only the seam
        header — never logged here. Blocking; runs on a worker thread."""
        try:
            pcm = _elevenlabs_sfx(prompt, el_key, duration_s, prompt_influence)
            if not pcm:
                log.warning("ElevenLabs sound generation returned no audio; no cue produced")
                return None
            audio = _pcm16_to_float32(pcm)
            return self._write_wav(audio, ELEVENLABS_SAMPLE_RATE)
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/prompt, decode)
            # -> no cue. Log only the redacted exception CLASS — never the message
            # (which could echo a key-bearing URL fragment).
            log.warning(
                "ElevenLabs sound generation failed (%s); no cue produced",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    def compose_music(self, prompt, el_key, length_ms=None):
        """Compose a NON-speech MUSIC track from `prompt` via ElevenLabs music
        (compose) and return a pipeline WAV path, or None on any failure.

        There is NO on-device music generator, so a failure (or no key) yields None
        and the caller treats it as 'unavailable' — never a fabricated/placeholder
        track, never a crash. `length_ms` (None -> EL's 30s default) is clamped to
        EL's [3000, 600000] ms window in the seam. The PCM bytes are MONO @24kHz
        (pcm_24000), so they flow through the IDENTICAL _pcm16_to_float32 ->
        _write_wav path as TTS/SFX (fades, atomic write, pipeline WAV) with no
        stereo-interleave hazard. The key rides only the seam header — never logged
        here. Blocking; runs on a worker thread."""
        try:
            length = ELEVENLABS_MUSIC_LENGTH_DEFAULT_MS if length_ms is None else length_ms
            pcm = _elevenlabs_music(prompt, el_key, length)
            if not pcm:
                log.warning("ElevenLabs music returned no audio; no track produced")
                return None
            audio = _pcm16_to_float32(pcm)
            return self._write_wav(audio, ELEVENLABS_SAMPLE_RATE)
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/prompt, decode)
            # -> no track. Log only the redacted exception CLASS — never the message
            # (which could echo a key-bearing URL fragment).
            log.warning(
                "ElevenLabs music failed (%s); no track produced",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    def isolate_audio(self, audio_path, el_key):
        """Strip background noise from the audio at `audio_path` via ElevenLabs
        audio isolation (voice isolator) and return a CLEANED pipeline WAV path, or
        None on any failure.

        HONESTY: the user's AUDIO leaves the device here (like Scribe STT) — MORE
        sensitive than TTS text. There is NO on-device isolator, so a failure (or no
        key) yields None and the caller treats it as 'unavailable' — never a faked or
        passed-through clip, never a crash. The cleaned PCM bytes flow through the
        IDENTICAL _pcm16_to_float32 -> _write_wav path as TTS/SFX (fades, atomic
        write, pipeline WAV). The key rides only the seam header — never logged here.
        Blocking; runs on a worker thread."""
        try:
            cleaned = _elevenlabs_isolate_audio(audio_path, el_key)
            if not cleaned:
                log.warning("ElevenLabs audio isolation returned no audio; nothing produced")
                return None
            audio = _pcm16_to_float32(cleaned)
            return self._write_wav(audio, ELEVENLABS_SAMPLE_RATE)
        except Exception as exc:
            # ANY error (network, HTTP, auth, timeout, missing key/file, decode) ->
            # no audio. Log only the redacted exception CLASS — never the message
            # (which could echo a key-bearing URL fragment).
            log.warning(
                "ElevenLabs audio isolation failed (%s); nothing produced",
                _redact_elevenlabs(type(exc).__name__),
            )
            return None

    @staticmethod
    def _apply_gain(audio, volume):
        """COARSE output gain (#34 whisper soft delivery), honoured on EVERY backend.
        Scale `audio` by `volume` in (0,1] when it is a real multiplier below 1.0;
        a None/1.0/out-of-range value leaves the audio UNTOUCHED (byte-for-byte
        today's). PURE; no network. The clip in _write_wav still bounds the result."""
        import numpy as np

        if not isinstance(volume, (int, float)) or isinstance(volume, bool):
            return audio
        v = float(volume)
        if not (0.0 < v < 1.0):
            return audio
        return (audio.astype(np.float32) * v)

    def _elevenlabs_synth_pcm_with_optional_stream(self, voice_id, model, api_key, text,
                                                   audio_tag=None, stability=None,
                                                   style=None, locators=None, stream=False):
        """Fetch ElevenLabs PCM, preferring the OPT-IN streaming seam when `stream`
        is on and FALLING BACK to the blocking seam on ANY streaming error.

        With `stream` False (the default) this is BYTE-FOR-BYTE the blocking call:
        it invokes `_elevenlabs_synth_pcm` directly and the streaming seam is never
        touched — so the existing speak path is unchanged.

        With `stream` True it tries `_elevenlabs_synth_pcm_stream` first; if that
        raises (network/HTTP/timeout/decode) we log ONLY the redacted exception
        CLASS and retry once via the blocking seam, so a streaming failure degrades
        to today's behaviour rather than failing. (If the blocking retry also
        raises, the exception propagates to `speak`, which falls back to Kokoro.)
        The key reaches only the seams (which put it in the xi-api-key header) —
        never logged here. Returns the raw PCM bytes (possibly empty)."""
        if stream:
            try:
                return _elevenlabs_synth_pcm_stream(
                    voice_id, model, api_key, text,
                    audio_tag=audio_tag, stability=stability, style=style, locators=locators,
                )
            except Exception as exc:
                # Streaming failed -> fall back to the blocking seam (same wire body).
                # Log ONLY the redacted exception class (never the message/URL/key).
                log.warning(
                    "ElevenLabs streaming synthesis failed (%s); falling back to blocking synth",
                    _redact_elevenlabs(type(exc).__name__),
                )
        return _elevenlabs_synth_pcm(
            voice_id, model, api_key, text,
            audio_tag=audio_tag, stability=stability, style=style, locators=locators,
        )

    def _elevenlabs_to_wav(self, text, voice_id, model, api_key, lang=None,
                           audio_tag=None, stability=None, style=None, volume=None,
                           locators=None, stream=False):
        """Synthesize `text` through the ElevenLabs cloud voice tier and write it
        to the SAME pipeline WAV the daemon expects (under state/tmp/). Returns the
        absolute path, or None when ElevenLabs produced no usable audio.

        `lang` is the OPTIONAL target language Babel threads through the speak
        spec. When it names a NON-English language, a MULTILINGUAL model is
        selected (so Babel's spoken target-language output gets EL's multilingual
        quality instead of the English-centric default); an absent/English lang
        leaves the requested/default model unchanged. An explicit `model` from the
        daemon is always honored as-is — the multilingual swap only fills in the
        DEFAULT model slot for a non-English turn.

        #33/#34 EXPRESSIVENESS: `audio_tag`/`stability`/`style` are the EL-v3 rich
        surface (EL-v3-gated inside `_elevenlabs_synth_pcm`); `volume` is the coarse
        whisper gain applied to the produced WAV on EVERY backend. With nothing set
        (the OFF default) the path is byte-for-byte today's.

        Raises on any network/HTTP error: the caller (`speak`) catches EVERYTHING
        and falls back to on-device Kokoro, so a cloud failure NEVER fails a turn.
        The key is used only to call the seam (which puts it in the xi-api-key
        header) — it is never logged here. Runs WITHOUT self._lock (audit fix):
        nothing on this path touches GPU/model state — the seam is a network
        call, decode/trim/gain are pure numpy, and _write_wav is a plain file
        write already used lock-free by sound_effect/compose_music/isolate_audio
        — so the cloud round-trip must not stall local inference behind the GPU
        lock."""
        model = self._resolve_elevenlabs_model(model, lang)
        # STREAMING TTS (opt-in): when `stream` is on we hit the low-latency /stream
        # seam and fall back to the blocking seam on any streaming error; with stream
        # False (the default) this is the blocking seam BYTE-FOR-BYTE.
        pcm = self._elevenlabs_synth_pcm_with_optional_stream(
            voice_id, model, api_key, _speakable(text),
            audio_tag=audio_tag, stability=stability, style=style, locators=locators,
            stream=stream,
        )
        if not pcm:
            return None
        audio = _pcm16_to_float32(pcm)
        if audio.size == 0:
            return None
        audio = self._trim_silence(audio, ELEVENLABS_SAMPLE_RATE)
        if audio is None:
            return None
        audio = self._apply_gain(audio, volume)
        return self._write_wav(audio, ELEVENLABS_SAMPLE_RATE)

    def speak(self, text, voice=None, backend=None, voice_id=None, model=None, el_key=None,
              lang=None, audio_tag=None, stability=None, style=None, rate=None, volume=None,
              locators=None, stream=None):
        """Synthesize `text` to a silence-trimmed 24kHz mono 16-bit WAV under
        state/tmp/ and return its absolute path. The daemon owns playback and
        deletion.

        `backend` selects the TTS path the daemon already chose:
          - "elevenlabs": the OPTIONAL cloud voice tier. We POST to ElevenLabs
            (xi-api-key header, key from `el_key`) for `voice_id`/`model`, wrap the
            PCM as the pipeline WAV, and return it. On ANY error/timeout — or a
            missing key/voice id — we FALL BACK to on-device Kokoro (never fail the
            turn). HONESTY: on this path the text leaves the device.
          - anything else (None/"kokoro"): on-device Kokoro, EXACTLY as before.
        Kokoro is the default + the fallback; the cloud path is reached only when the
        daemon asked for it (its own tier+key+offline gate already passed).

        `lang` is the OPTIONAL target language Babel threads through the speak spec.
        Under the ElevenLabs backend a NON-English `lang` selects a multilingual
        model (so Babel's spoken target-language output gets EL's multilingual
        quality); it is ignored on the Kokoro path (Kokoro's voice already encodes
        its language). When the tier is OFF the path is unchanged (Kokoro).

        #33/#34 EXPRESSIVENESS shaping (all OPTIONAL; None == today's neutral wire):
          - `audio_tag`/`stability`/`style`: the EL-v3 RICH surface — consumed ONLY
            on the EL-v3 model (EL-v3-gated in `_elevenlabs_synth_pcm`), so a tag
            never reaches a non-v3 model or the Kokoro path.
          - `rate`/`volume`: COARSE delivery hints honoured on every backend. `volume`
            is a gain applied to the produced WAV (the real "speak softly" on Kokoro
            too). `rate` is a coarse speaking-rate hint — applied to Kokoro by nudging
            the request speed within bounds; the EL leg keeps its model's pacing.
        PRONUNCIATION DICTIONARIES: `locators` is the OPTIONAL list of pronunciation-
        dictionary locators ([{pronunciation_dictionary_id[, version_id]}]) the daemon
        threads through; under the ElevenLabs backend they are folded ADDITIVELY into
        the TTS payload (ignored on the Kokoro path). With none set the wire is
        unchanged.

        With nothing set (the daemon's shipped-OFF default sends none of these), the
        path is BYTE-FOR-BYTE today's. No real network call is added on any path."""
        text = (text or "").strip()
        if not text:
            raise ValueError("'text' must be non-empty for op=speak")

        # STREAMING TTS is OPT-IN: an explicit per-call `stream` wins; otherwise the
        # module default `STREAM_TTS` (ships False). With both False the EL leg uses
        # the blocking seam BYTE-FOR-BYTE today's. Only consulted on the elevenlabs
        # backend (Kokoro never streams over the cloud).
        use_stream = STREAM_TTS if stream is None else bool(stream)
        use_elevenlabs = isinstance(backend, str) and backend.strip().lower() == "elevenlabs"
        if use_elevenlabs:
            try:
                # AUDIT FIX: the ElevenLabs leg runs WITHOUT self._lock. It is a
                # cloud round-trip (up to ELEVENLABS_TIMEOUT_S) followed by pure
                # numpy decode/trim/gain and an atomic file write — no GPU/model
                # state is touched anywhere on it, so holding the GPU lock here
                # stalled every local inference op (classify/converse/transcribe)
                # behind the network for up to 15s. The other cloud audio ops
                # (sound_effect/compose_music/isolate_audio, and the Scribe leg of
                # transcribe) already run lock-free for exactly this reason; only
                # the Kokoro fallback below touches the GPU and keeps the lock.
                # EL-v3 rich surface (audio_tag/stability/style) + coarse whisper
                # gain (volume) thread in here; the EL leg keeps its own pacing so
                # `rate` is not forwarded to the cloud model.
                path = self._elevenlabs_to_wav(
                    text, voice_id, model, el_key, lang,
                    audio_tag=audio_tag, stability=stability, style=style,
                    volume=volume, locators=locators, stream=use_stream,
                )
                if path is not None:
                    return path
                # No audio from the cloud tier -> fall through to Kokoro.
                log.warning("ElevenLabs returned no audio; falling back to on-device Kokoro")
            except Exception as exc:
                # ANY error (network, HTTP, auth, timeout, decode) -> Kokoro. We log
                # only the redacted exception CLASS — never exc_info / the message,
                # since a transport error string can echo the URL (voice id) and we
                # never want the key/voice id anywhere near a log line.
                log.warning(
                    "ElevenLabs synthesis failed (%s); falling back to on-device Kokoro",
                    _redact_elevenlabs(type(exc).__name__),
                )
            # FALL BACK: the rest of this method is the unchanged Kokoro path.

        # Kokoro / non-v3: the COARSE rate/gain the backend honours (no audio-tags —
        # rich prosody is EL-v3-gated and never faked here). The audio_tag/stability/
        # style are intentionally ignored on this path. With rate/volume unset this is
        # byte-for-byte today's Kokoro synth.
        voice = self._resolve_request_voice(voice)
        with self._lock:
            tts = self._ensure_tts()
            path = self._synthesize_to_wav(tts, text, voice, rate=rate, volume=volume)
        if path is None:
            raise RuntimeError("TTS produced no audio")
        return path

    def converse(
        self,
        text,
        max_tokens=GENERATE_DEFAULT_MAX_TOKENS,
        history=None,
        facts=None,
        data=None,
        voice=None,
        emit=None,
        opener_spoken=None,
        persona=None,
        local_model=None,
        cancelled=None,
    ):
        """Streamed generate+TTS in one GPU-locked pass: decode the persona
        reply (same prompt assembly, KV cache and sampler as generate, plus
        the opener_spoken continuation note when the daemon already played an
        opener for this reply) and,
        each time the decimal-aware sentence boundary completes a sentence,
        pause decoding (the stream_generate generator simply stays suspended),
        synthesize that sentence to a silence-trimmed WAV, call
        emit({"event": "sentence", "seq", "text", "path"}), then resume.

        At most CONVERSE_MAX_SPOKEN_SENTENCES sentences are synthesized;
        later text is only returned. Sentences Kokoro yields no audio for
        (e.g. punctuation-only) are skipped without consuming a seq. Blocking;
        the caller runs it on one worker thread, and `emit` must therefore be
        thread-safe (the server hands events to the asyncio loop via
        loop.call_soon_threadsafe).

        `cancelled` (optional, audit fix): a cheap thread-safe callable the
        server threads in; it returns True once the requesting peer has
        vanished (socket EOF / closing transport). It is polled once per
        decoded chunk, and when it fires the decode loop ABORTS — no further
        tokens are generated and no further sentences are synthesized or
        emitted — so a disconnected client cannot keep burning GPU under the
        lock. The abort path runs the same finally trim-back as an exception,
        so the KV-cache invariant holds; already-emitted sentence events stay
        valid. None (the default, and every non-server caller) never cancels.

        Returns {"text": full reply, "sentences": spoken count,
        "first_sentence_ms": int|None}. The persona KV cache is trimmed back
        to the static prefix even when decode or synthesis raises mid-stream —
        the cache must never be left grown."""
        from mlx_lm import stream_generate
        from mlx_lm.models.cache import trim_prompt_cache

        t0 = time.perf_counter()
        text = (text or "").strip()
        if not text:
            raise ValueError("'text' must be non-empty for op=converse")
        voice = self._resolve_request_voice(voice)
        if emit is None:
            emit = lambda event: None  # noqa: E731

        # Per-agent persona: when a persona NAME is supplied and resolves to a
        # personas/<name>.txt, that text becomes the system prefix for THIS call.
        # Each agent's prefix has its OWN KV cache (keyed by agent name, LRU-
        # bounded) so the agent's persona-prefix KV is prefilled once and reused
        # on later turns — never mixed into the base cache (keyed to
        # prompts/persona.txt). Absent / unresolvable persona -> base persona.
        persona_name = persona.strip().lower() if isinstance(persona, str) else None
        persona_override = load_agent_persona(persona) if persona else None

        with self._lock:
            # Resolve the LOCAL model for this turn (Local-tier sub-choice). The
            # persona KV caches (base + agent) live on the BASE model only; when
            # a non-base warm model answers it runs UNCACHED on its own resident
            # weights — that is the instant-swap benefit. _ensure_local_llm pins
            # the base + keeps the requested model warm under the RAM budget.
            resolved_id, lm_model, lm_tokenizer = self._ensure_local_llm(local_model)
            on_base = resolved_id == self.llm_id
            tts = self._ensure_tts()
            messages = self._build_generate_messages(
                text,
                history=history,
                facts=facts,
                data=data,
                opener_spoken=opener_spoken,
                persona_override=persona_override,
            )
            prompt = self._render_chat_messages(messages, tokenizer=lm_tokenizer)
            if opener_spoken and opener_spoken.strip():
                # Seed the assistant turn with the opener the daemon already
                # played: decoding continues mid-reply after it, so the model
                # cannot open with another acknowledgement (measured: with the
                # bracketed note alone, Qwen3-4B parrots the quoted opener
                # verbatim 3/3 runs). The seed is prompt, not generation —
                # sentence events and the returned text start at the
                # substance, which is exactly what the daemon must append
                # after the opener WAV it already played.
                prompt += opener_spoken.strip()
            # Select the KV cache state for this turn:
            #   - base persona  -> the resident base cache (self._gen_cache),
            #   - agent override -> that agent's resident prefix cache (built on
            #     first use, reused after; None if prefill wasn't feasible, in
            #     which case this turn runs uncached without touching any cache).
            # `cache_state` holds {cache, cache_len, prefix_tokens} so the trim
            # and the finally trim-back operate on the SAME cache uniformly; the
            # updated cache_len is written back to wherever it lives.
            # A non-base warm model has NO persona KV cache (the caches are built
            # on the base model's weights and cannot be reused across models):
            # it answers uncached. Only the base path consults/builds caches.
            if not on_base:
                cache_state = None
            elif persona_override is not None:
                cache_state = self._get_agent_gen_cache(persona_name, persona_override)
            else:
                cache_state = (
                    {
                        "cache": self._gen_cache,
                        "cache_len": self._gen_cache_len,
                        "prefix_tokens": self._gen_prefix_tokens,
                    }
                    if self._gen_cache is not None
                    else None
                )
            cache = cache_state["cache"] if cache_state is not None else None
            if cache is not None:
                cache_len = cache_state["cache_len"]
                prefix_tokens = cache_state["prefix_tokens"]
                full_tokens = lm_tokenizer.encode(prompt)
                # Tokenization at the prefix boundary can merge across the
                # seam; trim the cache down to the actual common token prefix.
                common = 0
                for a, b in zip(prefix_tokens[:cache_len], full_tokens):
                    if a != b:
                        break
                    common += 1
                if common < cache_len:
                    trim_prompt_cache(cache, cache_len - common)
                    cache_len = common
                # Persist the (possibly shrunk) prefix length back to its home so
                # the finally trim-back and the next reuse both see the truth.
                cache_state["cache_len"] = cache_len
                if persona_override is None:
                    self._gen_cache_len = cache_len
                prompt_tokens = full_tokens[cache_len:]
            else:
                prompt_tokens = lm_tokenizer.encode(prompt)

            pieces = []
            pending = ""
            spoken = 0
            first_sentence_ms = None
            generated = 0
            aborted = False
            last_resp = None

            def speak_sentence(sentence):
                """Synthesize one completed sentence and emit its event.
                Holds the decode paused (we are inside the stream_generate
                loop) and self._lock (whole-converse scope)."""
                nonlocal spoken, first_sentence_ms
                path = self._synthesize_to_wav(tts, sentence, voice)
                if path is None:
                    log.warning("converse: no audio for sentence %r; skipping", sentence)
                    return
                if first_sentence_ms is None:
                    first_sentence_ms = int((time.perf_counter() - t0) * 1000)
                emit({"event": "sentence", "seq": spoken, "text": sentence, "path": path})
                spoken += 1

            try:
                for resp in stream_generate(
                    lm_model,
                    lm_tokenizer,
                    prompt=prompt_tokens,
                    max_tokens=max_tokens,
                    prompt_cache=cache,
                    sampler=self._persona_sampler(),
                ):
                    # Mid-request cancellation (audit fix): once the peer is
                    # gone there is nobody to hear the reply — stop decoding
                    # (and synthesizing) instead of burning GPU under the lock
                    # for up to max_tokens more. Polled per chunk; the finally
                    # below trims the KV cache exactly as on an exception.
                    if cancelled is not None and cancelled():
                        aborted = True
                        log.info(
                            "converse: peer disconnected; aborting decode after "
                            "%d chunks / %d spoken sentences",
                            generated,
                            spoken,
                        )
                        break
                    pieces.append(resp.text)
                    generated += 1
                    last_resp = resp
                    if spoken < CONVERSE_MAX_SPOKEN_SENTENCES:
                        pending += resp.text
                        sentences, pending = _split_complete_sentences(pending)
                        for sentence in sentences:
                            if spoken < CONVERSE_MAX_SPOKEN_SENTENCES:
                                speak_sentence(sentence)
                # Flush the final partial sentence (no trailing terminator).
                # Skipped on abort: the peer is gone, so synthesizing the tail
                # would be pure wasted GPU.
                if not aborted and spoken < CONVERSE_MAX_SPOKEN_SENTENCES:
                    sentences, pending = _split_complete_sentences(pending, final=True)
                    for sentence in sentences:
                        if spoken < CONVERSE_MAX_SPOKEN_SENTENCES:
                            speak_sentence(sentence)
            finally:
                # Trim everything past the static prefix so the cache is
                # reusable — on success AND on any mid-stream exception. Uses the
                # selected cache_state's prefix length, so the base cache and a
                # per-agent cache are both restored to exactly their static
                # prefix (the base cache must never be left grown; an agent cache
                # likewise, or its next reuse would mis-trim).
                if cache is not None:
                    static_len = cache_state["cache_len"]
                    offset = getattr(cache[0], "offset", None)
                    added = (
                        (offset - static_len)
                        if isinstance(offset, int)
                        else (len(prompt_tokens) + generated)
                    )
                    if added > 0:
                        trim_prompt_cache(cache, added)

        # Surface the decode telemetry the loop used to discard (additive).
        metrics = self._record_metrics(last_resp, "converse")
        return {
            "text": "".join(pieces).strip(),
            "sentences": spoken,
            "first_sentence_ms": first_sentence_ms,
            "metrics": metrics,
        }


def parse_classification(raw):
    """Extract a strict-JSON classification from model output.

    Finds the first '{' ... last '}' span, json.loads it, and validates the
    intent/confidence/complexity keys. "args" is a pass-through of the
    model's extracted-params object: included verbatim when it is a JSON
    object, replaced with {} when absent or any other type — never
    fabricated server-side. Any failure returns the contract fallback
    (conversation / 0.3 / heavy / {}) so the router escalates to cloud.
    """
    fallback = dict(CLASSIFY_FALLBACK)
    fallback["args"] = {}  # fresh object per call; never alias the constant's
    if not isinstance(raw, str):
        return fallback
    try:
        start = raw.index("{")
        end = raw.rindex("}")
        obj = json.loads(raw[start : end + 1])
        intent = obj["intent"]
        confidence = float(obj["confidence"])
        complexity = obj["complexity"]
        if not isinstance(intent, str) or not intent:
            return fallback
        if complexity not in ("light", "heavy"):
            return fallback
        if not math.isfinite(confidence):
            return fallback
        confidence = max(0.0, min(1.0, confidence))
        args = obj.get("args")
        if not isinstance(args, dict):
            args = {}
        return {"intent": intent, "confidence": confidence, "complexity": complexity, "args": args}
    except (ValueError, KeyError, TypeError):
        log.warning("classifier returned unparseable output; using cloud-escalation fallback: %r", raw)
        return fallback


def parse_facts(raw):
    """Extract a strict-JSON facts list from model output.

    Accepts a bare JSON array (first '[' ... last ']') or, failing that, an
    object wrapper ({"facts": [...]} or a single {"key","value"} object).
    Validates each entry to non-empty string key/value, dedupes by key, and
    clips to 3. Keys must be dot-namespaced and must NOT start with "meta."
    (audit fix, mirroring parse_consolidation): extract_facts runs after
    EVERY spoken reply and its output is upserted verbatim daemon-side, so
    without this guard an utterance like "remember that meta.last_reflection
    is zero" could overwrite internal bookkeeping (reflection stamp, heal
    markers) and non-namespaced junk keys would pollute every persona
    prompt. Any parse failure returns [] — never an exception.
    """
    if not isinstance(raw, str):
        return []
    items = None
    try:
        start = raw.index("[")
        end = raw.rindex("]")
        items = json.loads(raw[start : end + 1])
    except (ValueError, json.JSONDecodeError):
        try:
            start = raw.index("{")
            end = raw.rindex("}")
            obj = json.loads(raw[start : end + 1])
            if isinstance(obj, dict):
                if isinstance(obj.get("facts"), list):
                    items = obj["facts"]
                elif "key" in obj and "value" in obj:
                    items = [obj]
        except (ValueError, json.JSONDecodeError):
            pass
    if not isinstance(items, list):
        if items is not None or raw.strip():
            log.warning("extract_facts returned unparseable output; using empty list: %r", raw)
        return []
    facts = []
    seen = set()
    for item in items:
        if not isinstance(item, dict):
            continue
        key = item.get("key")
        value = item.get("value")
        if key is None and value is None and len(item) == 1:
            # Lenient recovery for the model's other natural shape:
            # {"user.name": "Darwin"} instead of {"key": ..., "value": ...}.
            key, value = next(iter(item.items()))
        if not isinstance(key, str) or not isinstance(value, str):
            continue
        key = key.strip()
        value = value.strip()
        if not key or not value or key in seen:
            continue
        # Same key policy parse_consolidation enforces (lines mirror its
        # upsert guard): bookkeeping namespaces are never model-writable and
        # un-namespaced keys are never stored.
        if key.startswith("meta.") or not _is_namespaced_key(key):
            log.warning("extract_facts: dropping invalid/reserved key %r", key)
            continue
        seen.add(key)
        facts.append({"key": key, "value": value})
        if len(facts) == 3:
            break
    return facts


def _is_namespaced_key(key):
    """A valid fact key is dot-namespaced with non-empty segments
    (user.name, user.preference.units, context.location, ...)."""
    parts = key.split(".")
    return len(parts) >= 2 and all(p.strip() for p in parts)


# user.habit.* backstop (see habit_supported): words too generic to count as
# habit evidence on their own. Audit fix: the original list lacked common
# function words ("about", "what", "with", ...), so a fabricated habit value
# like "asks about the weather most mornings" was supported by any three
# user lines containing "about" ("tell me about X", "what about Y") — the
# exact assistant-claim laundering the backstop exists to stop.
_HABIT_FILLER_WORDS = frozenset(
    {
        "user",
        "habit",
        "asks",
        "asked",
        "asking",
        "most",
        "often",
        "usually",
        "every",
        "each",
        "always",
        "likes",
        "wants",
        "please",
        "darwin",
        "thing",
        "things",
        "request",
        "requests",
        # Common function/filler words (audit fix). Deliberately NOT here:
        # morning/afternoon/evening/night and real topic nouns — those are
        # exactly the content words habits should be evidenced by.
        "about",
        "what",
        "with",
        "when",
        "tell",
        "need",
        "want",
        "time",
        "good",
        "today",
        "again",
        "also",
        "been",
        "being",
        "cant",
        "could",
        "does",
        "doing",
        "dont",
        "gave",
        "give",
        "gonna",
        "going",
        "have",
        "here",
        "just",
        "know",
        "like",
        "made",
        "make",
        "many",
        "more",
        "much",
        "okay",
        "really",
        "should",
        "some",
        "still",
        "sure",
        "take",
        "thank",
        "thanks",
        "that",
        "them",
        "then",
        "there",
        "they",
        "think",
        "this",
        "very",
        "well",
        "will",
        "wont",
        "would",
        "yeah",
        "your",
        "yours",
    }
)
_HABIT_MIN_USER_LINES = 3
_HABIT_MIN_WORD_LEN = 4
# Prefix-tolerant matching only applies when the shared stem (the shorter
# word) is at least this long: morning/mornings match, with/without do not
# (audit fix: bidirectional prefixing at 4 chars let "with" support
# "without" and similar).
_HABIT_MIN_STEM_LEN = 5


def _content_words(text):
    """Lowercased alphabetic words of >= _HABIT_MIN_WORD_LEN chars, minus
    filler. No regex needed: split on every non-letter."""
    out = set()
    word = []
    for ch in text.lower() + " ":
        if "a" <= ch <= "z":
            word.append(ch)
            continue
        if len(word) >= _HABIT_MIN_WORD_LEN:
            w = "".join(word)
            if w not in _HABIT_FILLER_WORDS:
                out.add(w)
        word = []
    return out


def _habit_words_match(user_word, habit_word):
    """Whether one user-line content word evidences one habit content word:
    exact match, or prefix-tolerant stemming (morning/mornings) where the
    shared stem — the shorter word, which must prefix the longer — is at
    least _HABIT_MIN_STEM_LEN chars. 4-char words therefore only ever match
    exactly ("with" no longer supports "without")."""
    if user_word == habit_word:
        return True
    shorter, longer = sorted((user_word, habit_word), key=len)
    return len(shorter) >= _HABIT_MIN_STEM_LEN and longer.startswith(shorter)


def habit_supported(key, value, transcripts):
    """Deterministic backstop for prompt-level habit mining: a user.habit.*
    upsert survives only when at least _HABIT_MIN_USER_LINES SEPARATE user
    lines in the provided transcripts each share ENOUGH distinct content
    words with the habit's slug+value text — 2 distinct words when the habit
    text has 3+ content words, 1 when it has fewer (a short habit like
    "often asks for jazz" only carries its topic word, and three user lines
    naming that topic ARE the evidence; richer habit texts must co-match,
    so "asks about the weather most mornings" needs weather+mornings-class
    support per line, audit fix).

    The 4B greedy model parrots worked examples and occasionally converts the
    ASSISTANT's own claims about the user into a habit; this check makes the
    contract's rules — >= 3 occurrences, user lines only, never from stored
    facts alone — hold regardless of what the model emits. Strictly
    conservative: it only ever DROPS habit upserts, never adds or edits."""
    slug = key[len("user.habit.") :].replace("_", " ").replace(".", " ")
    habit_words = _content_words(f"{slug} {value}")
    if not habit_words:
        return False
    required = 2 if len(habit_words) >= 3 else 1
    hits = 0
    for turn in transcripts or []:
        user_words = _content_words(str(turn.get("user") or ""))
        matched = {
            hw
            for hw in habit_words
            if any(_habit_words_match(uw, hw) for uw in user_words)
        }
        if len(matched) >= required:
            hits += 1
            if hits >= _HABIT_MIN_USER_LINES:
                return True
    return False


def parse_consolidation(raw, existing):
    """Extract a strict-JSON consolidation result from model output.

    `existing` maps the input facts' key -> value ("meta." keys already
    filtered by the caller). Finds the first '{' ... last '}' span,
    json.loads it, and validates the {"upserts": [{"key","value"}],
    "deletes": [str]} shape. Enforcement on top of the parse (the model is
    NOT trusted):
      - keys must be non-empty namespaced strings; "meta."-prefixed keys are
        dropped wherever they appear (meta facts are bookkeeping, not memory)
      - upserts whose value matches the stored fact are dropped (no-ops) —
        but they still SHIELD their key from deletion: a fact the model just
        reaffirmed must never be deletable in the same pass
      - deletes may only name keys present in the input facts — never keys
        the model invented
      - a key both upserted and deleted is kept as an upsert only (or, for a
        no-op reaffirmation, kept unchanged)
      - upserts dedupe by key; total changes cap at CONSOLIDATE_MAX_CHANGES
        (upserts take precedence, deletes fill the remainder)
    Lenient recovery for the model's two natural shape slips (same spirit as
    parse_facts): an upsert written as a single-pair object
    {"user.name": "Dar"} and a delete written as "key = value".
    Any failure returns empty arrays — a reflection pass that does nothing
    is always safe; one that does the wrong thing is not.
    """
    empty = {"upserts": [], "deletes": []}
    if not isinstance(raw, str):
        return empty
    try:
        start = raw.index("{")
        end = raw.rindex("}")
        obj = json.loads(raw[start : end + 1])
    except (ValueError, json.JSONDecodeError):
        log.warning("consolidate returned unparseable output; changing nothing: %r", raw)
        return empty
    if not isinstance(obj, dict):
        log.warning("consolidate returned non-object JSON; changing nothing: %r", raw)
        return empty
    raw_upserts = obj.get("upserts")
    raw_deletes = obj.get("deletes")
    if not isinstance(raw_upserts, list) or not isinstance(raw_deletes, list):
        log.warning("consolidate output missing upserts/deletes lists; changing nothing: %r", raw)
        return empty
    upserts = []
    upsert_keys = set()
    # Every structurally valid upsert key, INCLUDING ones dropped as no-ops
    # below. The delete shield must use this superset: a no-op upsert means
    # the model just reaffirmed the stored fact, and a reaffirmed fact must
    # not be deletable by a delete naming the same key in the same pass.
    shielded_keys = set()
    for item in raw_upserts:
        if not isinstance(item, dict):
            continue
        key = item.get("key")
        value = item.get("value")
        if key is None and value is None and len(item) == 1:
            # Lenient recovery: {"user.name": "Dar"} single-pair shape.
            key, value = next(iter(item.items()))
        if not isinstance(key, str) or not isinstance(value, str):
            continue
        key = key.strip()
        value = value.strip()
        if not key or not value or key in upsert_keys:
            continue
        if key.startswith("meta.") or not _is_namespaced_key(key):
            continue
        shielded_keys.add(key)
        if existing.get(key) == value:
            continue  # no-op upsert: the stored fact already says this
        upsert_keys.add(key)
        upserts.append({"key": key, "value": value})
    deletes = []
    seen_deletes = set()
    for key in raw_deletes:
        if not isinstance(key, str):
            continue
        key = key.strip()
        if "=" in key and key not in existing:
            # Lenient recovery: "user.name = Darwin" — fact keys never
            # contain '=', so everything after the first '=' is the value.
            key = key.split("=", 1)[0].strip()
        if not key or key in seen_deletes or key in shielded_keys:
            continue
        if key.startswith("meta.") or key not in existing:
            continue
        seen_deletes.add(key)
        deletes.append(key)
    # Cap the blast radius of one reflection pass: upserts first (they carry
    # information), deletes fill whatever budget remains.
    upserts = upserts[:CONSOLIDATE_MAX_CHANGES]
    deletes = deletes[: CONSOLIDATE_MAX_CHANGES - len(upserts)]
    return {"upserts": upserts, "deletes": deletes}


class InferenceServer:
    def __init__(self, engine, preload=False):
        self.engine = engine
        self.preload = preload
        self._stop = asyncio.Event()

    # -- request dispatch ----------------------------------------------

    @staticmethod
    def _validate_generate_extras(req):
        """Shared history/facts/data validation for generate and converse."""
        history = req.get("history")
        if history is not None:
            if not isinstance(history, list):
                raise ValueError("'history' must be a list")
            for turn in history:
                if (
                    not isinstance(turn, dict)
                    or turn.get("speaker") not in ("user", "darwin")
                    or not isinstance(turn.get("text"), str)
                ):
                    raise ValueError(
                        "'history' entries must be {\"speaker\": \"user\"|\"darwin\", \"text\": str}"
                    )
        facts = req.get("facts")
        if facts is not None:
            if not isinstance(facts, list) or not all(isinstance(f, str) for f in facts):
                raise ValueError("'facts' must be a list of strings")
        data = req.get("data")
        if data is not None and not isinstance(data, str):
            raise ValueError("'data' must be a string")
        return history, facts, data

    async def dispatch(self, req, writer, reader=None):
        """Handle one request. Single-response ops return their response
        dict; op=converse additionally writes interim "sentence" event lines
        to `writer` before returning the terminal "done" event. Safe because
        handle_client processes exactly one request at a time per connection,
        so nothing else can interleave bytes into this writer mid-stream
        (each client gets its own connection/writer pair).

        `reader` (optional) is this connection's StreamReader; when supplied it
        powers the mid-request peer-disconnect probe below. None (unit tests)
        disables cancellation entirely."""
        rid = str(req.get("id", ""))
        op = req.get("op")
        t0 = time.perf_counter()

        def latency_ms():
            return int((time.perf_counter() - t0) * 1000)

        # Mid-request cancellation probe (audit fix): while an op runs on its
        # asyncio.to_thread worker, the event loop KEEPS servicing this
        # connection's read side — so a peer that died mid-request surfaces as
        # EOF on `reader` (its close lands as eof_received) or, after a reset,
        # as a closing transport, long before our next write would notice. The
        # probe is a couple of attribute reads (no await, no mutation), safe to
        # poll from the worker thread under the GIL, and it can only fire for a
        # genuinely gone peer: a live client waiting on its response holds the
        # socket open, so at_eof()/is_closing() stay False. Threaded ONLY into
        # the ops with a chunked decode loop (converse; generate's cached
        # path) — the single-blocking-call ops cannot be interrupted mid-call
        # and keep today's run-to-completion behavior.
        if reader is not None:
            transport = writer.transport

            def peer_gone():
                return reader.at_eof() or transport.is_closing()
        else:
            peer_gone = None

        try:
            if op == "transcribe":
                path = req.get("path")
                if not path:
                    raise ValueError("'path' is required for op=transcribe")
                # OPTIONAL gated cloud-STT tier — present only when the daemon's
                # cloud-STT tier+key+offline gate already passed. The key (el_key)
                # rides only the request body the daemon read from the Keychain; we
                # NEVER log it. Absent/"whisper" backend -> on-device mlx_whisper
                # exactly as before; on ANY cloud error the engine falls back to
                # whisper itself (never fails the turn). HONESTY: the cloud path
                # sends the user's VOICE AUDIO off the device.
                backend = req.get("backend")
                if backend is not None and not isinstance(backend, str):
                    raise ValueError("'backend' must be a string")
                model = req.get("model")
                if model is not None and not isinstance(model, str):
                    raise ValueError("'model' must be a string")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                text, words = await asyncio.to_thread(
                    self.engine.transcribe, path, backend, el_key, model
                )
                resp = {"id": rid, "ok": True, "text": text, "latency_ms": latency_ms()}
                # #31 MULTI-SPEAKER DIARIZATION: when the Scribe backend returned a
                # per-word stream with speaker labels, surface it so the daemon's
                # gated [voice].diarize path CONSUMES the real labels (its PURE
                # diarize::diarize over a ScribeResponse). Absent on the on-device
                # whisper path (no diarization model) and on a Scribe response with
                # no word detail -> the daemon then renders the honest single stream,
                # never a fabricated speaker. The `words` carry text + timings +
                # speaker ids only, NEVER audio.
                if isinstance(words, list) and words:
                    resp["words"] = words
                return resp

            if op == "clone_voice":
                # CONSENT-GATED voice cloning. The daemon only emits this after an
                # explicit, authorization-bound consent confirm on a user-owned
                # sample. path = owner sample WAV, text = display name, el_key = the
                # xi-api-key (request-body only, NEVER logged). On success the daemon
                # stores the returned voice_id (NON-secret) like any EL voice id; on
                # failure -> ok:false (clean no-clone, the user keeps their voice).
                # HONESTY: the audio SAMPLE leaves the device to ElevenLabs.
                path = req.get("path")
                if not path or not isinstance(path, str):
                    raise ValueError("'path' (owner voice sample) is required for op=clone_voice")
                name = req.get("text")
                if not name or not isinstance(name, str):
                    raise ValueError("'text' (voice display name) is required for op=clone_voice")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                voice_id = await asyncio.to_thread(
                    self.engine.clone_voice, path, name, el_key
                )
                if voice_id:
                    return {
                        "id": rid,
                        "ok": True,
                        "voice_id": voice_id,
                        "latency_ms": latency_ms(),
                    }
                # Clean no-clone: never throws, never leaks; the daemon keeps Kokoro.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "voice cloning unavailable (no voice produced)",
                    "latency_ms": latency_ms(),
                }

            if op == "design_voice":
                # CLOUD-ONLY prompt-to-voice design (ElevenLabs text-to-voice). The
                # daemon emits this only after its key gate. text = the voice
                # DESCRIPTION (EL needs 20-1000 chars); voice/name = the display
                # name; el_key = the xi-api-key (request-body only, NEVER logged).
                # Unlike clone_voice, NO audio sample leaves the device — the voice
                # is minted purely from the text. There is NO on-device voice
                # designer, so on any failure (or no key) -> ok:false (honest
                # 'unavailable', never a fabricated voice_id). On success the daemon
                # stores the returned voice_id (NON-secret) like any EL voice id.
                description = req.get("text")
                if not description or not isinstance(description, str):
                    raise ValueError("'text' (voice description) is required for op=design_voice")
                name = req.get("voice") or req.get("name")
                if not name or not isinstance(name, str):
                    raise ValueError("'voice' (voice display name) is required for op=design_voice")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                voice_id = await asyncio.to_thread(
                    self.engine.design_voice, description, name, el_key
                )
                if voice_id:
                    return {
                        "id": rid,
                        "ok": True,
                        "voice_id": voice_id,
                        "latency_ms": latency_ms(),
                    }
                # Honest unavailable: there is no on-device designer to substitute.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "voice design unavailable (no voice produced)",
                    "latency_ms": latency_ms(),
                }

            if op == "sound_effect":
                # CLOUD-ONLY sound-effect cue (ElevenLabs sound-generation). The
                # daemon emits this only after its [voice].cloud_sfx + key gate.
                # text = the SFX prompt; el_key = the xi-api-key (request-body only,
                # NEVER logged); optional duration_s + prompt_influence shape the cue.
                # There is NO on-device SFX generator, so on any failure (or no key)
                # -> ok:false (honest 'unavailable', never a fabricated/placeholder cue).
                prompt = req.get("text")
                if not prompt or not isinstance(prompt, str):
                    raise ValueError("'text' (sound-effect prompt) is required for op=sound_effect")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                duration_s = req.get("duration_s")
                if duration_s is not None and not isinstance(duration_s, (int, float)):
                    raise ValueError("'duration_s' must be a number")
                prompt_influence = req.get("prompt_influence")
                if prompt_influence is not None and not isinstance(prompt_influence, (int, float)):
                    raise ValueError("'prompt_influence' must be a number")
                path = await asyncio.to_thread(
                    self.engine.sound_effect, prompt, el_key, duration_s, prompt_influence
                )
                if path:
                    return {"id": rid, "ok": True, "path": path, "latency_ms": latency_ms()}
                # Honest unavailable: there is no on-device SFX fallback to substitute.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "sound effects unavailable (cloud SFX tier off or no key)",
                    "latency_ms": latency_ms(),
                }

            if op == "compose_music":
                # CLOUD-ONLY music track (ElevenLabs music/compose). The daemon emits
                # this only after its [voice].cloud_music + key gate. text = the music
                # prompt; el_key = the xi-api-key (request-body only, NEVER logged);
                # optional length_ms shapes the duration (clamped to EL's
                # [3000, 600000] ms window). The returned PCM is MONO (pcm_24000), so
                # it decodes through the same mono pipeline path as TTS/SFX — no stereo
                # interleave hazard. There is NO on-device music generator, so on any
                # failure (or no key) -> ok:false (honest 'unavailable', never a
                # fabricated/placeholder track).
                prompt = req.get("text")
                if not prompt or not isinstance(prompt, str):
                    raise ValueError("'text' (music prompt) is required for op=compose_music")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                length_ms = req.get("length_ms")
                if length_ms is not None and (
                    not isinstance(length_ms, int) or isinstance(length_ms, bool)
                ):
                    raise ValueError("'length_ms' must be an integer")
                path = await asyncio.to_thread(
                    self.engine.compose_music, prompt, el_key, length_ms
                )
                if path:
                    return {"id": rid, "ok": True, "path": path, "latency_ms": latency_ms()}
                # Honest unavailable: there is no on-device music fallback to substitute.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "music unavailable (cloud music tier off or no key)",
                    "latency_ms": latency_ms(),
                }

            if op == "isolate_audio":
                # CLOUD-ONLY audio isolation (ElevenLabs voice isolator). The daemon
                # emits this only after its key gate. path = the audio file to clean;
                # el_key = the xi-api-key (request-body only, NEVER logged). There is
                # NO on-device isolator, so on any failure (or no key) -> ok:false
                # (honest 'unavailable', never a faked/passed-through clip). HONESTY:
                # the user's AUDIO leaves the device to ElevenLabs (like Scribe STT).
                path = req.get("path")
                if not path or not isinstance(path, str):
                    raise ValueError("'path' (audio to isolate) is required for op=isolate_audio")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                cleaned_path = await asyncio.to_thread(
                    self.engine.isolate_audio, path, el_key
                )
                if cleaned_path:
                    return {"id": rid, "ok": True, "path": cleaned_path, "latency_ms": latency_ms()}
                # Honest unavailable: there is no on-device isolator to substitute.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "audio isolation unavailable (no key or isolation failed)",
                    "latency_ms": latency_ms(),
                }

            if op == "create_pronunciation":
                # CLOUD-ONLY pronunciation dictionary (ElevenLabs add-from-rules). The
                # daemon emits this only after its key gate. name = the dictionary
                # display name; rules = the non-empty list of replacement rules
                # ({string_to_replace, type:'alias'|'phoneme', alias/phoneme, ...});
                # el_key = the xi-api-key (request-body only, NEVER logged). NO audio
                # leaves the device — the dictionary is minted purely from the text
                # rules. There is NO on-device equivalent, so on any failure (or no
                # key) -> ok:false (honest 'unavailable', never a fabricated id). On
                # success the daemon stores the returned NON-secret (dictionary_id,
                # version_id) pair to later thread into a speak as pronunciation
                # locators.
                name = req.get("name")
                if not name or not isinstance(name, str):
                    raise ValueError("'name' (dictionary name) is required for op=create_pronunciation")
                rules = req.get("rules")
                if not isinstance(rules, list) or not rules:
                    raise ValueError("'rules' (non-empty list) is required for op=create_pronunciation")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                result = await asyncio.to_thread(
                    self.engine.create_pronunciation, name, rules, el_key
                )
                if result:
                    dictionary_id, version_id = result
                    return {
                        "id": rid,
                        "ok": True,
                        "dictionary_id": dictionary_id,
                        "version_id": version_id,
                        "latency_ms": latency_ms(),
                    }
                # Honest unavailable: there is no on-device dictionary to substitute.
                return {
                    "id": rid,
                    "ok": False,
                    "error": "pronunciation dictionary unavailable (no dictionary produced)",
                    "latency_ms": latency_ms(),
                }

            if op == "classify":
                text = req.get("text")
                if not text or not isinstance(text, str):
                    raise ValueError("'text' must be a non-empty string for op=classify")
                result = await asyncio.to_thread(self.engine.classify, text)
                return {
                    "id": rid,
                    "ok": True,
                    "intent": result["intent"],
                    "confidence": result["confidence"],
                    "complexity": result["complexity"],
                    "args": result.get("args", {}),
                    # Additive honest decode telemetry (tok/s + peak mem) from the
                    # cached path; None on the uncached fallback (no per-token
                    # stream to measure) — never fabricated.
                    "metrics": result.get("_metrics"),
                    "latency_ms": latency_ms(),
                }

            if op == "generate":
                text = req.get("text")
                if text is None:
                    raise ValueError("'text' is required for op=generate")
                max_tokens = req.get("max_tokens", GENERATE_DEFAULT_MAX_TOKENS)
                if not isinstance(max_tokens, int) or max_tokens <= 0:
                    raise ValueError("'max_tokens' must be a positive integer")
                history, facts, data = self._validate_generate_extras(req)
                # Optional Local-tier sub-choice: a warm local model id. Unknown
                # / absent ids degrade to the base single-resident model engine-
                # side, so we only type-check it here.
                local_model = req.get("local_model")
                if local_model is not None and not isinstance(local_model, str):
                    raise ValueError("'local_model' must be a string")
                # #37/#39: generate_with_meta returns the HONEST path that ran —
                # speculative on/off (never faked) + the quant that ACTUALLY
                # loaded. Surfaced in the response so the daemon/HUD report the
                # real path, not the requested one.
                out, gen_meta = await asyncio.to_thread(
                    self.engine.generate_with_meta, text, max_tokens, history, facts, data,
                    local_model, peer_gone,
                )
                return {
                    "id": rid,
                    "ok": True,
                    "text": out,
                    "speculative": gen_meta.get("speculative", False),
                    "quant": gen_meta.get("quant", "auto"),
                    # SELF-DISTILLATION: the personal adapter stamp for THE PATH
                    # THIS TURN RAN (or None = unadapted). HONEST — from the
                    # per-turn gen_meta, not the engine-global state: a turn on a
                    # non-base warm model reports None even while the base
                    # resident carries an adapter (the adapter fuses into base
                    # only), and mismatched/failed adapters serve base + None. The
                    # daemon/HUD never see a personalization that didn't run.
                    "adapter": gen_meta.get("adapter"),
                    # Additive honest decode telemetry (mlx_lm-measured tok/s +
                    # peak GPU memory) from the cached generate path — the SAME
                    # metrics dict op=converse and op=classify already forward.
                    # None on the speculative/uncached single-shot paths (no
                    # per-token stream to measure) and on an immediate cancel;
                    # never estimated or fabricated. Was measured then DROPPED.
                    "metrics": gen_meta.get("metrics"),
                    "latency_ms": latency_ms(),
                }

            if op == "reload_lora":
                # SELF-DISTILLATION: the daemon promoted or rolled back a personal
                # adapter — drop the resident LLM so the NEXT generate reloads with
                # the current state/lora/promoted/ pointer. Under the GPU lock (via
                # a thread); lock-holding ops serialize with it, and the
                # lock-SLICED consolidate pass runs on refs captured at op
                # start, so a mid-pass reload is safe (applies next op).
                await asyncio.to_thread(self.engine.reload_lora)
                return {"id": rid, "ok": True, "reloaded": True, "latency_ms": latency_ms()}

            if op == "lora_status":
                # Read-only: which personal adapter (if any) is LIVE, plus the
                # honest note when one staged on disk was NOT loaded (base mismatch
                # / load failure). Never claims an adapter that isn't running.
                return {
                    "id": rid,
                    "ok": True,
                    "adapter": self.engine._active_adapter,
                    "note": self.engine._adapter_note,
                    "latency_ms": latency_ms(),
                }

            if op == "converse":
                text = req.get("text")
                if not text or not isinstance(text, str):
                    raise ValueError("'text' must be a non-empty string for op=converse")
                max_tokens = req.get("max_tokens", GENERATE_DEFAULT_MAX_TOKENS)
                if not isinstance(max_tokens, int) or max_tokens <= 0:
                    raise ValueError("'max_tokens' must be a positive integer")
                history, facts, data = self._validate_generate_extras(req)
                voice = req.get("voice")
                if voice is not None and not isinstance(voice, str):
                    raise ValueError("'voice' must be a string")
                opener_spoken = req.get("opener_spoken")
                if opener_spoken is not None and not isinstance(opener_spoken, str):
                    raise ValueError("'opener_spoken' must be a string")
                persona = req.get("persona")
                if persona is not None and not isinstance(persona, str):
                    raise ValueError("'persona' must be a string")
                local_model = req.get("local_model")
                if local_model is not None and not isinstance(local_model, str):
                    raise ValueError("'local_model' must be a string")
                loop = asyncio.get_running_loop()

                def emit(event):
                    # Called from the GPU worker thread: hand the write to the
                    # event loop. call_soon_threadsafe callbacks run in FIFO
                    # order and asyncio.to_thread's own completion is queued
                    # after them, so every sentence line is written before the
                    # done line below. Sentence events are tiny (~200 bytes,
                    # <= 5 per request); no drain/backpressure needed.
                    line = (json.dumps({"id": rid, **event}) + "\n").encode("utf-8")
                    loop.call_soon_threadsafe(writer.write, line)

                result = await asyncio.to_thread(
                    self.engine.converse,
                    text,
                    max_tokens,
                    history,
                    facts,
                    data,
                    voice,
                    emit,
                    opener_spoken,
                    persona,
                    local_model,
                    peer_gone,
                )
                lat = latency_ms()
                return {
                    "id": rid,
                    "event": "done",
                    "ok": True,
                    "text": result["text"],
                    "sentences": result["sentences"],
                    "first_sentence_ms": (
                        result["first_sentence_ms"]
                        if result["first_sentence_ms"] is not None
                        else lat
                    ),
                    # Additive honest decode telemetry (tok/s + peak mem) from the
                    # converse stream_generate loop; None if nothing was decoded.
                    "metrics": result.get("metrics"),
                    "latency_ms": lat,
                }

            if op == "extract_facts":
                text = req.get("text")
                if not text or not isinstance(text, str):
                    raise ValueError("'text' must be a non-empty string for op=extract_facts")
                response = req.get("response")
                if response is not None and not isinstance(response, str):
                    raise ValueError("'response' must be a string")
                facts = await asyncio.to_thread(self.engine.extract_facts, text, response)
                return {"id": rid, "ok": True, "facts": facts, "latency_ms": latency_ms()}

            if op == "consolidate":
                transcripts = req.get("transcripts")
                if transcripts is None:
                    transcripts = []
                if not isinstance(transcripts, list):
                    raise ValueError("'transcripts' must be a list")
                for turn in transcripts:
                    if (
                        not isinstance(turn, dict)
                        or not isinstance(turn.get("user"), str)
                        or not isinstance(turn.get("darwin"), str)
                    ):
                        raise ValueError(
                            "'transcripts' entries must be {\"user\": str, \"darwin\": str}"
                        )
                # Keep the newest window if the caller over-sends (the
                # contract caps the request at 40; transcripts arrive oldest
                # first, so the tail is the recent material).
                transcripts = transcripts[-CONSOLIDATE_MAX_TRANSCRIPTS:]
                facts = req.get("facts")
                if facts is None:
                    facts = []
                if not isinstance(facts, list):
                    raise ValueError("'facts' must be a list")
                for fact in facts:
                    if (
                        not isinstance(fact, dict)
                        or not isinstance(fact.get("key"), str)
                        or not isinstance(fact.get("value"), str)
                    ):
                        raise ValueError(
                            "'facts' entries must be {\"key\": str, \"value\": str} for op=consolidate"
                        )
                result = await asyncio.to_thread(self.engine.consolidate, transcripts, facts)
                return {
                    "id": rid,
                    "ok": True,
                    "upserts": result["upserts"],
                    "deletes": result["deletes"],
                    "latency_ms": latency_ms(),
                }

            if op == "speak":
                text = req.get("text")
                if not text:
                    raise ValueError("'text' is required for op=speak")
                voice = req.get("voice")
                if voice is not None and not isinstance(voice, str):
                    raise ValueError("'voice' must be a string")
                # OPTIONAL ElevenLabs cloud voice tier — present only when the daemon
                # chose it (its tier+key+offline gate already passed). The key (el_key)
                # rides only the request body the daemon read from the Keychain; we
                # NEVER log it. Absent/"kokoro" backend -> on-device Kokoro exactly as
                # before. On any cloud error the engine falls back to Kokoro itself.
                backend = req.get("backend")
                if backend is not None and not isinstance(backend, str):
                    raise ValueError("'backend' must be a string")
                voice_id = req.get("voice_id")
                if voice_id is not None and not isinstance(voice_id, str):
                    raise ValueError("'voice_id' must be a string")
                model = req.get("model")
                if model is not None and not isinstance(model, str):
                    raise ValueError("'model' must be a string")
                el_key = req.get("el_key")
                if el_key is not None and not isinstance(el_key, str):
                    raise ValueError("'el_key' must be a string")
                # OPTIONAL target language (Babel interpret/translate output). Under
                # the ElevenLabs backend a non-English lang selects a multilingual
                # model; ignored on the Kokoro path and when the tier is OFF.
                lang = req.get("lang")
                if lang is not None and not isinstance(lang, str):
                    raise ValueError("'lang' must be a string")
                # OPTIONAL EXPRESSIVENESS shaping (#33 prosody + #34 whisper). Present
                # ONLY when the daemon's adaptive_prosody / whisper features are on and
                # the resolved shape is non-neutral; ABSENT (the shipped default) ->
                # byte-for-byte today's speak. The rich EL-v3 surface (audio_tag/
                # stability/style) is honoured ONLY on the EL-v3 model; rate/volume are
                # coarse hints honoured on every backend. Validated + clamped in
                # `_normalize_speak_shape` so a bad/out-of-range value can never reach
                # the synth path. NON-secret delivery hints (never the key/voice id).
                audio_tag, stability, style, rate, volume = _normalize_speak_shape(req)
                # OPTIONAL pronunciation-dictionary locators (NON-secret ids the daemon
                # minted via op=create_pronunciation). Folded ADDITIVELY into the EL TTS
                # payload ONLY when a non-empty list survives validation; ABSENT (the
                # shipped default) -> byte-for-byte today's speak. Validated/clamped in
                # `_normalize_pronunciation_locators` so a malformed value can never
                # reach the wire. NON-secret (never the key/voice id).
                pron_locators = req.get("pronunciation_locators")
                if pron_locators is not None and not isinstance(pron_locators, list):
                    raise ValueError("'pronunciation_locators' must be a list")
                locators = _normalize_pronunciation_locators(pron_locators)
                # OPTIONAL low-latency STREAMING TTS tier. Present only when the daemon
                # opts in per-call; ABSENT (None) -> the module STREAM_TTS default
                # (ships False), i.e. the existing blocking cloud synth. Only consulted
                # on the elevenlabs backend; any streaming error falls back to the
                # blocking seam (then Kokoro), so this never fails a turn. NON-secret.
                stream = req.get("stream")
                if stream is not None and not isinstance(stream, bool):
                    raise ValueError("'stream' must be a boolean")
                path = await asyncio.to_thread(
                    self.engine.speak, text, voice, backend, voice_id, model, el_key,
                    lang, audio_tag, stability, style, rate, volume, locators, stream,
                )
                return {"id": rid, "ok": True, "path": path, "latency_ms": latency_ms()}

            if op == "embed":
                texts = req.get("texts")
                if texts is None:
                    raise ValueError("'texts' is required for op=embed")
                if not isinstance(texts, list):
                    raise ValueError("'texts' must be a list of strings for op=embed")
                vectors, embedder_id, dim, fell_back = await asyncio.to_thread(
                    self.engine.embed_with_meta, texts
                )
                # ==== op=embed WIRE CONTRACT (consumed by daemon/docsearch) ====
                # Alongside `vectors` (unchanged shape: one L2-normalized vector
                # per input text, in input order), the response carries the
                # ACTIVE embedder's vector-space identity so the daemon/docsearch
                # know which SPACE these vectors live in and refuse cross-space
                # comparison. The id is OPAQUE downstream — compared by STRING
                # EQUALITY only; docsearch stamps its store with it and any
                # differing id is a different space that forces reindex.
                #   "embedder": the vector-space id —
                #       "coreml-bge-small-en-v1.5" (fixed) for the Core ML bge
                #       path (384-dim, one pinned model), or the MODEL-DERIVED
                #       "llm-meanpool:<llm_id>:<quant>[:<adapter>]" for the mean-pool path (so
                #       swapping [models].llm or the quant changes the stamp — a
                #       fixed string would falsely keep matching across a space
                #       change). docsearch stores this WITH its index.
                #   "dim": the integer vector dimension (384 for Core ML, the
                #       resident model's hidden size for mean-pool); may be null
                #       ONLY on an empty batch of the mean-pool path (nothing to
                #       index).
                #   "fell_back": advisory bool — true iff the Core ML embedder was
                #       CONFIGURED but unavailable, so this response is the honest
                #       mean-pool fallback. Not required to key the space (embedder
                #       already does), but lets the daemon/HUD surface the degraded
                #       state.
                # BACKWARD-SAFE: the daemon's inference Response is
                # #[serde(default)] with NO deny_unknown_fields, so ADDING these
                # fields is safe — an old daemon simply ignores them and reads
                # `vectors` as before.
                return {
                    "id": rid,
                    "ok": True,
                    "vectors": vectors,
                    "embedder": embedder_id,
                    "dim": dim,
                    "fell_back": fell_back,
                    "latency_ms": latency_ms(),
                }

            if op == "rerank":
                # STAGE TWO of the two-stage retrieval stack: re-score the daemon's
                # dense top-K candidates with the Core ML cross-encoder and return one
                # relevance logit per passage (the daemon re-orders by these). READ-
                # ONLY / benign — no actuation, no writes.
                query = req.get("query")
                if query is None:
                    raise ValueError("'query' is required for op=rerank")
                if not isinstance(query, str):
                    raise ValueError("'query' must be a string for op=rerank")
                passages = req.get("passages")
                if passages is None:
                    raise ValueError("'passages' is required for op=rerank")
                if not isinstance(passages, list):
                    raise ValueError("'passages' must be a list of strings for op=rerank")
                scores, reranker_id, fell_back = await asyncio.to_thread(
                    self.engine.rerank_with_meta, query, passages
                )
                # ==== op=rerank WIRE CONTRACT (consumed by daemon retrieval) ====
                #   "scores": one relevance score per passage, in INPUT order (higher
                #       = more relevant). The daemon re-orders its dense shortlist by
                #       these. On the honest fallback the scores are order-PRESERVING
                #       (descending), so a re-sort is a no-op.
                #   "reranker": the cross-encoder model id that produced the scores
                #       ("coreml-ms-marco-minilm-l6-v2"), or "" when fell_back (no
                #       model scored — the daemon keeps its dense order). Opaque
                #       downstream (carried verbatim for the HUD / which-order-ran
                #       telemetry).
                #   "fell_back": advisory bool — true iff the reranker was CONFIGURED
                #       but unavailable, so this response is the honest dense-order
                #       fallback. The daemon keeps its dense order and labels the
                #       ranking as un-reranked.
                # BACKWARD-SAFE: additive fields on an ok response; an old daemon that
                # never sends op=rerank never sees this path.
                return {
                    "id": rid,
                    "ok": True,
                    "scores": scores,
                    "reranker": reranker_id,
                    "fell_back": fell_back,
                    "latency_ms": latency_ms(),
                }

            if op == "describe_image":
                # On-device VLM: describe / answer a question about a LOCAL image.
                # The daemon path-confines `path` (canonicalize + allowed root)
                # BEFORE sending it here; we read the image locally and hand it
                # only to the on-device MLX VLM (pixels never leave the device).
                # Distinct from op=transcribe/OCR: this REASONS about the visual
                # scene. When the VLM is unavailable (mlx-vlm absent or the model
                # not downloaded) the engine returns an honest structured
                # ok:false/reason — NEVER a fabricated description — and the
                # daemon falls back (OCR/classification / honest message).
                path = req.get("path")
                if not path or not isinstance(path, str):
                    raise ValueError("'path' is required for op=describe_image")
                question = req.get("question")
                if question is not None and not isinstance(question, str):
                    raise ValueError("'question' must be a string for op=describe_image")
                max_tokens = req.get("max_tokens")
                if max_tokens is not None and (
                    not isinstance(max_tokens, int) or max_tokens <= 0
                ):
                    raise ValueError("'max_tokens' must be a positive integer for op=describe_image")
                result = await asyncio.to_thread(
                    self.engine.describe_image, path, question, max_tokens
                )
                if result.get("ok"):
                    return {
                        "id": rid,
                        "ok": True,
                        "text": result["text"],
                        "model": result.get("model"),
                        "latency_ms": latency_ms(),
                    }
                # Honest unavailable / failure path: ok:false + a machine-keyable
                # reason the daemon uses to choose its fallback.
                return {
                    "id": rid,
                    "ok": False,
                    "reason": result.get("reason", DESCRIBE_IMAGE_UNAVAILABLE_REASON),
                    "error": result.get("error", "vision-language model not available"),
                    "latency_ms": latency_ms(),
                }

            if op == "generate_image":
                # On-device text->image: render a prompt with the MLX diffusion
                # model and save the image LOCALLY under state/images/. The
                # prompt is handed ONLY to the on-device model and the pixels
                # stay on the machine — image generation is 100% on-device, NO
                # cloud image API anywhere in this path. When the model is
                # unavailable (the diffusion package absent or the checkpoint not
                # downloaded) the engine returns an honest structured
                # ok:false/reason — NEVER a fabricated image and NEVER a cloud
                # fallback — and the daemon surfaces an honest message.
                prompt = req.get("prompt")
                if not prompt or not isinstance(prompt, str):
                    raise ValueError("'prompt' is required for op=generate_image")
                size = req.get("size")
                if size is not None and (not isinstance(size, int) or size <= 0):
                    raise ValueError("'size' must be a positive integer for op=generate_image")
                steps = req.get("steps")
                if steps is not None and (not isinstance(steps, int) or steps <= 0):
                    raise ValueError("'steps' must be a positive integer for op=generate_image")
                seed = req.get("seed")
                if seed is not None and not isinstance(seed, int):
                    raise ValueError("'seed' must be an integer for op=generate_image")
                result = await asyncio.to_thread(
                    self.engine.generate_image, prompt, size, steps, seed
                )
                if result.get("ok"):
                    return {
                        "id": rid,
                        "ok": True,
                        "path": result["path"],
                        "model": result.get("model"),
                        "size": result.get("size"),
                        "steps": result.get("steps"),
                        "seed": result.get("seed"),
                        "latency_ms": latency_ms(),
                    }
                # Honest unavailable / failure path: ok:false + a machine-keyable
                # reason the daemon surfaces honestly (NO cloud fallback).
                return {
                    "id": rid,
                    "ok": False,
                    "reason": result.get("reason", GENERATE_IMAGE_UNAVAILABLE_REASON),
                    "error": result.get("error", "image model not available"),
                    "latency_ms": latency_ms(),
                }

            raise ValueError(f"unknown op: {op!r}")
        except Exception as exc:
            log.exception("request %s (op=%s) failed", rid, op)
            resp = {"id": rid, "ok": False, "error": str(exc), "latency_ms": latency_ms()}
            if op == "converse":
                # Terminal line of the converse event stream; any sentence
                # events already written remain valid for the client.
                resp["event"] = "done"
            return resp

    # -- connection handling -------------------------------------------

    async def handle_client(self, reader, writer):
        peer = "client"
        log.info("%s connected", peer)
        try:
            while not self._stop.is_set():
                try:
                    line = await reader.readline()
                except ValueError:
                    # Line exceeded the stream limit. The overrun buffer has
                    # been discarded, so resyncing mid-stream is unsafe:
                    # answer with a structured error, then close.
                    log.warning("request line exceeds %d-byte limit; closing connection", STREAM_LIMIT)
                    resp = {
                        "id": "",
                        "ok": False,
                        "error": f"request line exceeds {STREAM_LIMIT}-byte limit",
                        "latency_ms": 0,
                    }
                    writer.write((json.dumps(resp) + "\n").encode("utf-8"))
                    await writer.drain()
                    break
                if not line:
                    break
                line = line.strip()
                if not line:
                    continue
                t0 = time.perf_counter()
                try:
                    req = json.loads(line)
                    if not isinstance(req, dict):
                        raise ValueError("request must be a JSON object")
                except (json.JSONDecodeError, ValueError) as exc:
                    resp = {
                        "id": "",
                        "ok": False,
                        "error": f"invalid request JSON: {exc}",
                        "latency_ms": int((time.perf_counter() - t0) * 1000),
                    }
                else:
                    resp = await self.dispatch(req, writer, reader)
                writer.write((json.dumps(resp) + "\n").encode("utf-8"))
                await writer.drain()
        except (ConnectionResetError, BrokenPipeError):
            pass
        except Exception:
            log.exception("unexpected error on client connection")
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            log.info("%s disconnected", peer)

    # -- lifecycle -------------------------------------------------------

    def request_stop(self, signame):
        log.info("received %s; shutting down", signame)
        self._stop.set()

    async def _claim_socket_path(self):
        """Refuse to start when another live server owns the socket; remove
        the socket file only when it is provably stale (connect refused or
        the file vanished)."""
        if not SOCKET_PATH.exists():
            return
        try:
            _, probe_writer = await asyncio.open_unix_connection(str(SOCKET_PATH))
        except (ConnectionRefusedError, FileNotFoundError):
            try:
                SOCKET_PATH.unlink()
                log.info("removed stale socket %s", SOCKET_PATH)
            except FileNotFoundError:
                pass
            return
        except OSError as exc:
            raise SystemExit(
                f"cannot probe existing socket {SOCKET_PATH} ({exc}); refusing to start"
            )
        probe_writer.close()
        try:
            await probe_writer.wait_closed()
        except Exception:
            pass
        raise SystemExit(
            f"another inference server is already running on {SOCKET_PATH}; refusing to start"
        )

    async def run(self):
        SOCKET_PATH.parent.mkdir(parents=True, exist_ok=True)
        await self._claim_socket_path()

        server = await asyncio.start_unix_server(
            self.handle_client, path=str(SOCKET_PATH), limit=STREAM_LIMIT
        )
        # Identity of the socket file WE bound: at shutdown, unlink only if
        # the path still points at this inode (never delete a successor's
        # live socket).
        try:
            bound = os.stat(SOCKET_PATH)
            socket_identity = (bound.st_dev, bound.st_ino)
        except OSError:
            socket_identity = None

        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, self.request_stop, sig.name)

        log.info(
            "inference server listening on %s (llm=%s, stt=%s, classifier=%s, tts=%s engine=%s voice=%s)",
            SOCKET_PATH,
            self.engine.llm_id,
            self.engine.stt_id,
            self.engine.classifier_id or "(main llm)",
            self.engine.tts_id,
            self.engine.engine_name,
            self.engine.voice,
        )
        if self.preload:
            # Daemon thread: requests that arrive before a stage finishes
            # still work via the lazy path (they just queue on the GPU lock).
            threading.Thread(target=self.engine.preload, name="preload", daemon=True).start()
            log.info("preload enabled; warming models in the background")
        try:
            await self._stop.wait()
        finally:
            server.close()
            await server.wait_closed()
            try:
                current = os.stat(SOCKET_PATH)
                if socket_identity == (current.st_dev, current.st_ino):
                    SOCKET_PATH.unlink()
                else:
                    log.warning(
                        "socket %s was replaced by another process; leaving it in place",
                        SOCKET_PATH,
                    )
            except FileNotFoundError:
                pass
            log.info("inference server stopped cleanly")


def check_runtime_deps():
    """Refuse to bind the socket as a zombie-healthy server when the MLX
    stack is missing (e.g. launched with the bare Homebrew python instead of
    .venv/bin/python — module-level imports here are stdlib-only by design,
    so without this guard every op would fail per-request). Unconditional
    (audit fix): the mlx probe used to run only under preload=true, so a
    preload=false server with bare-interpreter numpy bound the socket and
    logged healthy while every op failed — exactly the zombie this guard's
    contract promises to prevent. preload only controls model WARMING, never
    the dependency check."""
    missing = []
    try:
        import numpy  # noqa: F401
    except ImportError:
        missing.append("numpy")
    try:
        import mlx.core  # noqa: F401
    except ImportError:
        missing.append("mlx")
    if missing:
        venv_python = PROJECT_ROOT / ".venv" / "bin" / "python"
        log.critical(
            "missing runtime dependencies %s under %s; run the server with the "
            "MLX venv interpreter instead: %s inference/server.py",
            ", ".join(missing),
            sys.executable,
            venv_python,
        )
        raise SystemExit(2)


def _selftest():
    """Hermetic checks for the pure parts of the per-agent persona KV cache —
    no MLX, no model, no network. Exercises the LRU eviction/promotion policy
    (`_lru_insert`) and the persona-name validation/traversal guard
    (`load_agent_persona`). Run with `python server.py --selftest`; exits
    non-zero on the first failure so it can gate CI."""
    from collections import OrderedDict

    # LRU: newest insert is MRU; oldest evicted once over the bound.
    od = OrderedDict()
    assert _lru_insert(od, "friday", 1, 2) == []
    assert _lru_insert(od, "aegis", 2, 2) == []
    assert _lru_insert(od, "vision", 3, 2) == ["friday"], "oldest must evict first"
    assert list(od.keys()) == ["aegis", "vision"], od

    # Touching an existing key promotes it to MRU, sparing it from the next
    # eviction (this is what makes an agent-switch reuse the right cache).
    od2 = OrderedDict()
    _lru_insert(od2, "a", 1, 2)
    _lru_insert(od2, "b", 2, 2)
    _lru_insert(od2, "a", 1, 2)  # re-touch a -> a is now MRU
    assert _lru_insert(od2, "c", 3, 2) == ["b"], "re-touched key must survive"
    assert list(od2.keys()) == ["a", "c"], od2

    # max_size 1 keeps only the latest; each insert evicts the prior.
    od3 = OrderedDict()
    _lru_insert(od3, "x", 1, 1)
    assert _lru_insert(od3, "y", 2, 1) == ["x"]
    assert list(od3.keys()) == ["y"]

    # Persona-name guard: a valid lowercase slug is accepted (file may or may
    # not exist -> str or None, never an exception); traversal / invalid names
    # are rejected as None WITHOUT touching the filesystem.
    assert load_agent_persona("../../etc/passwd") is None
    assert load_agent_persona("Friday") is None or isinstance(
        load_agent_persona("Friday"), str
    )
    assert load_agent_persona("bad/name") is None
    assert load_agent_persona("") is None
    assert load_agent_persona(None) is None
    assert load_agent_persona(123) is None

    _selftest_local_warm()
    _selftest_speak_shape()
    _selftest_diarize()
    _selftest_runtime_knobs()
    print("server selftest OK")


def _selftest_runtime_knobs():
    """Hermetic checks for the #37 SPECULATIVE DECODING + #39 SELECTABLE
    QUANTIZATION pure helpers — NO MLX, NO model, NO network, NO draft/quant
    load. Exercises the PURE decisions over SYNTHETIC inputs:
      * should_use_speculative: the AND-gate + the honest unavailable-fallback
        (turned on but no draft / draft unavailable -> normal gen, never faked);
      * validate_quant: accept the allowed set, reject an unknown value;
      * select_quant: honor a present requested quant, FALL BACK + report
        requested_honored=False when absent (NEVER claims a quant that did not
        load), "auto" stays today's behavior;
      * quant_variant_id: derive the candidate id for a requested quant.
    The LIVE seams (draft load, speculative generate, quant model load) are
    device/runtime-gated and NOT exercised — the import-guarded optional-feature
    precedent. The real speedup/RAM/quality effect is device-gated."""
    # --- #37 should_use_speculative: the AND-gate + honest fallback ---------
    # All three conditions true -> speculative WILL run.
    assert should_use_speculative(True, "draft-0.6b", True) is True
    # Master gate OFF (the default) -> normal gen, even with a draft available.
    assert should_use_speculative(False, "draft-0.6b", True) is False
    # On but NO draft model configured -> normal gen (inert).
    assert should_use_speculative(True, "", True) is False
    # On + draft configured but the draft is UNAVAILABLE (load/import guard said
    # no) -> normal gen, NEVER a faked speculative run.
    assert should_use_speculative(True, "draft-0.6b", False) is False

    # --- #39 validate_quant: accept allowed, reject unknown ----------------
    for q in ALLOWED_QUANT:
        assert validate_quant(q) == q, q
    for bad in ("int3", "8bit", "bf16", "INT4", "", "fp32"):
        try:
            validate_quant(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f"validate_quant({bad!r}) must raise")

    # --- #39 select_quant: present honored / absent honest fallback --------
    # "auto" -> ("auto", True): today's behavior, no claim about a variant.
    assert select_quant("auto", ["int4", "int8"]) == ("auto", True)
    assert select_quant("auto", []) == ("auto", True)
    # Requested variant PRESENT -> honored.
    assert select_quant("int4", ["fp16", "int4"]) == ("int4", True)
    # Requested variant ABSENT, others present -> fall back to a concrete
    # available quant + report requested_honored=False (NEVER claims int4).
    chosen, honored = select_quant("int4", ["fp16", "int8"])
    assert honored is False, "an absent requested quant must report it honestly"
    assert chosen in ("fp16", "int8") and chosen != "int4", chosen
    # Requested variant ABSENT and NOTHING enumerable -> "auto" + honored=False
    # (let the loader pick the on-disk variant; never fabricate a variant).
    assert select_quant("int4", []) == ("auto", False)
    assert select_quant("fp16", None) == ("auto", False)

    # --- #39 quant_variant_id: derive the candidate id --------------------
    # "auto" leaves the id untouched (today's behavior).
    assert quant_variant_id("mlx-community/Qwen3-4B-Instruct-4bit", "auto") == "mlx-community/Qwen3-4B-Instruct-4bit"
    # An explicit quant swaps a recognized quant suffix.
    assert quant_variant_id("mlx-community/Qwen3-4B-Instruct-4bit", "int8") == "mlx-community/Qwen3-4B-Instruct-8bit"
    assert quant_variant_id("mlx-community/Qwen3-4B-Instruct-4bit", "fp16") == "mlx-community/Qwen3-4B-Instruct-bf16"
    # No recognized suffix -> APPEND the target quant suffix.
    assert quant_variant_id("some/model", "int4") == "some/model-4bit"

    # --- defaults are OFF/neutral (off => today's runtime) -----------------
    # A config-less load_config keeps speculative OFF + no draft + quant "auto".
    # The Engine reads these straight from settings; prove the neutral defaults
    # the loader would produce.
    assert ALLOWED_QUANT[0] == "auto", "auto must be the neutral default quant"


def _selftest_diarize():
    """Hermetic checks for the #31 PURE Scribe-label diarizer (_diarize_scribe_words)
    — no network, no model. Proves: (a) two speakers map to contiguous per-speaker
    turns with carried timings; (b) a single speaker coalesces to one turn; (c) words
    with no speaker_id are honestly "unknown", NEVER fabricated as distinct speakers;
    (d) an empty/absent words stream falls back to ONE unknown turn from `text`;
    (e) spacing/audio_event tokens are skipped; (f) empty text -> no turns."""
    # (a) two speakers -> three contiguous turns (s0, s1, s0) with timings.
    resp = {
        "text": "hello there hi darwin all good",
        "words": [
            {"text": "hello", "type": "word", "speaker_id": "speaker_0", "start": 0.0, "end": 0.4},
            {"text": "there", "type": "word", "speaker_id": "speaker_0", "start": 0.5, "end": 0.9},
            {"text": "hi", "type": "word", "speaker_id": "speaker_1", "start": 1.2, "end": 1.4},
            {"text": "darwin", "type": "word", "speaker_id": "speaker_1", "start": 1.5, "end": 1.9},
            {"text": "all", "type": "word", "speaker_id": "speaker_0", "start": 2.2, "end": 2.4},
            {"text": "good", "type": "word", "speaker_id": "speaker_0", "start": 2.5, "end": 2.9},
        ],
    }
    turns = _diarize_scribe_words(resp)
    assert len(turns) == 3, turns
    assert turns[0] == {"speaker_id": "speaker_0", "text": "hello there", "start": 0.0, "end": 0.9}, turns[0]
    assert turns[1] == {"speaker_id": "speaker_1", "text": "hi darwin", "start": 1.2, "end": 1.9}, turns[1]
    assert turns[2] == {"speaker_id": "speaker_0", "text": "all good", "start": 2.2, "end": 2.9}, turns[2]

    # (b) single speaker -> one coalesced turn.
    one = _diarize_scribe_words({
        "text": "what is the time",
        "words": [{"text": w, "type": "word", "speaker_id": "speaker_0"} for w in ["what", "is", "the", "time"]],
    })
    assert len(one) == 1 and one[0]["speaker_id"] == "speaker_0" and one[0]["text"] == "what is the time", one

    # (c) no speaker_id -> honest "unknown", NEVER fabricated distinct speakers.
    unk = _diarize_scribe_words({
        "text": "hello world",
        "words": [{"text": "hello", "type": "word"}, {"text": "world", "type": "word"}],
    })
    assert len(unk) == 1 and unk[0]["speaker_id"] == DIARIZE_UNKNOWN_SPEAKER and unk[0]["text"] == "hello world", unk

    # (d) empty/absent words -> one unknown turn from `text`.
    fallback = _diarize_scribe_words({"text": "only text here", "words": []})
    assert fallback == [{"speaker_id": DIARIZE_UNKNOWN_SPEAKER, "text": "only text here", "start": None, "end": None}], fallback

    # (e) spacing/audio_event tokens are skipped.
    skipped = _diarize_scribe_words({
        "text": "hi there",
        "words": [
            {"text": "hi", "type": "word", "speaker_id": "speaker_0"},
            {"text": " ", "type": "spacing", "speaker_id": "speaker_0"},
            {"text": "(laughter)", "type": "audio_event"},
            {"text": "there", "type": "word", "speaker_id": "speaker_0"},
        ],
    })
    assert len(skipped) == 1 and skipped[0]["text"] == "hi there", skipped

    # (f) empty text + no words -> no turns; non-dict -> no turns.
    assert _diarize_scribe_words({"text": "   ", "words": []}) == []
    assert _diarize_scribe_words(None) == []


def _selftest_speak_shape():
    """Hermetic checks for the #33/#34 EXPRESSIVENESS consumer seam — no MLX, no
    model, NO NETWORK (the EL synth seam is never called here; we test only the PURE
    payload-shaping + validation + coarse gain/rate). Proves: (a) with adaptive
    prosody ON + the EL-v3 model, the request payload carries the inline audio-tag +
    stability/style voice_settings; (b) on a non-v3 model the tag/settings are DROPPED
    (EL-v3-gated, never spoken literally); (c) a neutral request (nothing set, the
    shipped default) shapes a byte-for-byte-today payload; (d) the validator clamps /
    rejects out-of-range + bad-type delivery hints; (e) the coarse gain/rate are
    no-ops at the neutral default and real below 1.0 / off 1.0."""
    import numpy as np

    # (a) EL-v3 RICH surface: audio-tag prefixed inline + stability/style settings.
    v3 = _build_elevenlabs_payload(
        "VID", ELEVENLABS_V3_MODEL, "hello", audio_tag="[urgently]", stability=0.35, style=0.6
    )
    assert v3["text"] == "[urgently] hello", v3
    assert v3["model_id"] == ELEVENLABS_V3_MODEL
    assert v3["voice_settings"] == {"stability": 0.35, "style": 0.6}, v3

    # (b) NON-v3 model: the rich surface is DROPPED — no tag in the text, no settings.
    flash = _build_elevenlabs_payload(
        "VID", "eleven_flash_v2_5", "hello", audio_tag="[urgently]", stability=0.35, style=0.6
    )
    assert flash["text"] == "hello", "no audio-tag may reach a non-v3 model"
    assert "voice_settings" not in flash, "no v3 settings on a non-v3 model"

    # (c) NEUTRAL request: nothing set -> byte-for-byte today's payload (text+model only).
    neutral = _build_elevenlabs_payload("VID", ELEVENLABS_V3_MODEL, "hello")
    assert neutral == {"text": "hello", "model_id": ELEVENLABS_V3_MODEL}, neutral

    # (d) validator: clamps in-range, rejects bad type / NaN / out of range -> None.
    req = {"audio_tag": "[calm]", "stability": 0.75, "style": 0.3, "rate": 0.95, "volume": 0.45}
    tag, stab, sty, rate, vol = _normalize_speak_shape(req)
    assert (tag, stab, sty, rate, vol) == ("[calm]", 0.75, 0.3, 0.95, 0.45), (tag, stab, sty, rate, vol)
    # Out of range clamps; bad type / blank -> None (== neutral default).
    over = _normalize_speak_shape({"stability": 5.0, "rate": 99.0, "volume": -1.0, "audio_tag": "  "})
    assert over == (None, 1.0, None, 2.0, SPEAK_VOLUME_MIN), over
    empty = _normalize_speak_shape({})
    assert empty == (None, None, None, None, None), empty
    bad = _normalize_speak_shape({"stability": "loud", "rate": True, "volume": float("nan")})
    assert bad == (None, None, None, None, None), bad

    # (e) coarse gain/rate: no-op at neutral, real below/off 1.0. PURE, no synth.
    a = np.ones(100, dtype=np.float32)
    assert float(InferenceEngine._apply_gain(a, None)[0]) == 1.0  # neutral -> untouched
    assert float(InferenceEngine._apply_gain(a, 1.0)[0]) == 1.0  # 1.0 untouched
    # soft delivery: the gain is really applied (float32 round-trips 0.45 to ~0.45,
    # so compare with a float32 tolerance — exact float64 equality would spuriously fail).
    assert abs(float(InferenceEngine._apply_gain(a, 0.45)[0]) - 0.45) < 1e-6  # soft delivery
    # rate: faster -> fewer samples; 1.0 / None untouched.
    assert InferenceEngine._apply_rate(a, ELEVENLABS_SAMPLE_RATE, 1.08).shape[0] < a.shape[0]
    assert InferenceEngine._apply_rate(a, ELEVENLABS_SAMPLE_RATE, 1.0).shape[0] == a.shape[0]
    assert InferenceEngine._apply_rate(a, ELEVENLABS_SAMPLE_RATE, None).shape[0] == a.shape[0]


def _selftest_local_warm():
    """Hermetic checks for the multi-resident LOCAL model manager (task #17) —
    no MLX, no model, no network. Exercises the PURE policy (footprint estimate,
    budget admit / single-resident fallback) and the LocalWarmManager keep-warm /
    LRU-evict / select / absent-model-fallback behavior over SYNTHETIC sizes
    with a SYNTHETIC loader. Proves the LOGIC the daemon Local tier relies on;
    the ACTUAL load + the swap speed benefit are runtime/device-gated and are
    NOT exercised or claimed here."""
    SIZES = {"base4b": 3.0, "fast1b": 1.0, "cap8b": 6.0, "tiny": 0.5}

    # --- footprint estimate ------------------------------------------------
    # Explicit override wins.
    assert estimate_local_model_gib("base4b", SIZES) == 3.0
    # Bad override (<=0 / non-numeric) -> heuristic/default, never the bad value.
    assert estimate_local_model_gib("x", {"x": 0}) == DEFAULT_LOCAL_MODEL_GIB
    assert estimate_local_model_gib("x", {"x": "big"}) == DEFAULT_LOCAL_MODEL_GIB
    # Heuristic: param count x quant factor (4bit smaller than bf16).
    assert estimate_local_model_gib("mlx-community/Qwen3-4B-Instruct-4bit") < estimate_local_model_gib(
        "mlx-community/Qwen3-4B-Instruct-bf16"
    )
    # Unknown id with no "<n>b" token -> conservative default.
    assert estimate_local_model_gib("some/unknown-model") == DEFAULT_LOCAL_MODEL_GIB

    # --- plan_warm_set: budget admit / single-resident fallback ------------
    # Budget 0 (the DEFAULT) -> single-resident: base only, extras ignored.
    assert plan_warm_set("base4b", ["fast1b", "cap8b"], 0, SIZES) == ["base4b"]
    # Generous budget admits the extras that FIT, in config order; base first.
    assert plan_warm_set("base4b", ["fast1b"], 5.0, SIZES) == ["base4b", "fast1b"]
    # 3.0 (base) + 1.0 (fast) = 4.0 <= 4.0 admits fast; cap8b (6.0) never fits.
    assert plan_warm_set("base4b", ["fast1b", "cap8b"], 4.0, SIZES) == ["base4b", "fast1b"]
    # Over-budget extra is SKIPPED but a later smaller one still admitted.
    assert plan_warm_set("base4b", ["cap8b", "tiny"], 3.6, SIZES) == ["base4b", "tiny"]
    # Base alone exceeds the budget -> single-resident (base MUST stay warm).
    assert plan_warm_set("cap8b", ["fast1b"], 4.0, SIZES) == ["cap8b"]
    # Dedup: a repeated id (incl. the base) is admitted at most once.
    assert plan_warm_set("base4b", ["fast1b", "fast1b", "base4b"], 9.0, SIZES) == [
        "base4b",
        "fast1b",
    ]

    # --- LocalWarmManager: select / keep-warm / evict / fallback -----------
    loaded = []  # records every loader invocation (proves no redundant loads).

    def loader(mid):
        loaded.append(mid)
        return (f"model::{mid}", f"tok::{mid}")

    def absent_loader(mid):
        loaded.append(mid)
        if mid != "base4b":
            raise RuntimeError(f"checkpoint {mid} not downloaded")
        return (f"model::{mid}", f"tok::{mid}")

    # Single-resident manager (default): any requested id resolves to the base,
    # and the base loads exactly once and stays pinned.
    loaded.clear()
    m1 = LocalWarmManager("base4b", configured=["fast1b"], budget_gib=0, sizes=SIZES)
    assert m1.multi_resident() is False
    rid, model, _ = m1.select("fast1b", loader)  # out-of-warm-set -> base
    assert rid == "base4b" and model == "model::base4b"
    assert m1.select(None, loader)[0] == "base4b"  # None -> base
    assert loaded == ["base4b"], "single-resident loads only the base"
    assert m1.warm_ids() == ["base4b"]

    # Multi-resident manager: base + one extra warm, both kept resident; the
    # base is pinned and never evicted; a re-select is a cache HIT (no reload).
    loaded.clear()
    m2 = LocalWarmManager("base4b", configured=["fast1b"], budget_gib=5.0, sizes=SIZES)
    assert m2.multi_resident() is True and m2.capacity == 2
    assert m2.select(None, loader)[0] == "base4b"  # base warm
    assert m2.select("fast1b", loader)[0] == "fast1b"  # extra warm too
    assert sorted(m2.warm_ids()) == ["base4b", "fast1b"]
    assert m2.select("fast1b", loader)[0] == "fast1b"  # HIT
    assert loaded == ["base4b", "fast1b"], "no redundant reload on a warm hit"

    # LRU eviction over capacity: capacity 2 (base + 1 extra slot). Warming a
    # SECOND extra evicts the least-recently-used NON-base resident; the base
    # survives (pinned).
    loaded.clear()
    m3 = LocalWarmManager(
        "base4b", configured=["fast1b", "tiny"], budget_gib=3.6, sizes=SIZES
    )
    # 3.0 + 1.0 = 4.0 > 3.6 so fast1b does NOT fit; 3.0 + 0.5 = 3.5 <= 3.6 so
    # tiny fits. Plan = [base4b, tiny]; capacity 2.
    assert m3.plan == ["base4b", "tiny"]
    assert m3.select(None, loader)[0] == "base4b"
    assert m3.select("tiny", loader)[0] == "tiny"
    # fast1b is NOT in the warm-set (didn't fit) -> resolves to base, no load.
    assert m3.select("fast1b", loader)[0] == "base4b"
    assert sorted(m3.warm_ids()) == ["base4b", "tiny"]

    # Eviction with two admitted extras under a bigger budget + capacity 2 cap.
    loaded.clear()
    m4 = LocalWarmManager(
        "base4b", configured=["fast1b", "tiny"], budget_gib=9.0, sizes=SIZES
    )
    assert m4.plan == ["base4b", "fast1b", "tiny"] and m4.capacity == 3
    m4.select(None, loader)
    m4.select("fast1b", loader)
    m4.select("tiny", loader)
    assert sorted(m4.warm_ids()) == ["base4b", "fast1b", "tiny"]
    # Now constrain capacity to 2 to force an eviction on the next distinct load.
    m4.capacity = 2
    # Re-touch tiny so fast1b is the LRU non-base resident, then load a NEW one.
    m4.select("tiny", loader)
    m4.resident["fast1b"]  # fast1b still present, but now LRU
    m4.select("base4b", loader)  # base re-touch (pinned) must not evict tiny
    # Insert a fresh resident directly to trigger _insert eviction logic.
    ev = m4._insert("newmodel", "m", "t")
    assert "base4b" not in ev, "base is pinned, never evicted"
    assert len(m4.warm_ids()) <= 2

    # Absent model (loader raises) -> transparent fallback to the base, never a
    # crash; the base is loaded as the substitute. The fallback path logs the
    # caught load error via log.exception; silence it here so the EXPECTED
    # traceback does not masquerade as a selftest failure in CI output.
    loaded.clear()
    m5 = LocalWarmManager("base4b", configured=["fast1b"], budget_gib=5.0, sizes=SIZES)
    _prev_level = log.level
    log.setLevel(logging.CRITICAL)
    try:
        rid, model, _ = m5.select("fast1b", absent_loader)
    finally:
        log.setLevel(_prev_level)
    assert rid == "base4b" and model == "model::base4b", "absent extra -> base"
    assert "fast1b" in loaded and "base4b" in loaded
    assert m5.warm_ids() == ["base4b"], "failed extra is NOT marked resident"

    # resolve_id is pure (no load): in-set -> itself, out-of-set/None -> base.
    m6 = LocalWarmManager("base4b", configured=["fast1b"], budget_gib=5.0, sizes=SIZES)
    assert m6.resolve_id("fast1b") == "fast1b"
    assert m6.resolve_id("cap8b") == "base4b"
    assert m6.resolve_id(None) == "base4b"
    assert m6.resolve_id("") == "base4b"


def main():
    import sys

    if "--selftest" in sys.argv[1:]:
        _selftest()
        return
    setup_logging()
    settings = load_config()
    check_runtime_deps()
    engine = InferenceEngine(settings, load_classifier_template(), load_persona())
    asyncio.run(InferenceServer(engine, preload=settings["preload"]).run())


if __name__ == "__main__":
    main()
