#!/usr/bin/env python3
"""Greeting-variation probe — prove DARWIN varies on repeated identical input.

This is a STANDALONE diagnostic, not the daemon. It faithfully mirrors the
cloud-conversation path the Rust daemon builds for a bare greeting:

  - system  = inference/prompts/persona.txt  (the real persona, trimmed)
  - messages = prior "Hi DARWIN" turns + this turn, as real chat turns
  - avoid    = DARWIN's last replies, freshest-first, capped at 4, folded into
               the system prompt as the anti-repeat note (anthropic.rs::avoid_instruction)
  - model    = claude-opus-4-8   max_tokens = 200
  - NO temperature / top_p / top_k (Opus 4.8 400s on them — the prompt is the
    only variation lever), NO thinking, NO tools.

Key resolution matches the daemon: ANTHROPIC_API_KEY env first, else the macOS
Keychain (service com.darwin.daemon, account anthropic_api_key). No key is ever
printed or logged.

Usage:
    .venv/bin/python scripts/greeting_probe.py                 # 5x "Hi DARWIN"
    .venv/bin/python scripts/greeting_probe.py "Hey Darwin"    # custom phrase
    .venv/bin/python scripts/greeting_probe.py "Hi" 8          # phrase + count
"""
import json
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PERSONA_PATH = ROOT / "inference" / "prompts" / "persona.txt"
MODEL = "claude-opus-4-8"
MAX_TOKENS = 200
AVOID_CAP = 4  # router.rs::AVOID_RECENT_REPLIES

KEYCHAIN_SERVICE = "com.darwin.daemon"
KEYCHAIN_ACCOUNT = "anthropic_api_key"


def resolve_key():
    import os
    env = os.environ.get("ANTHROPIC_API_KEY", "").strip()
    if env:
        return env
    try:
        out = subprocess.run(
            ["/usr/bin/security", "find-generic-password",
             "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT, "-w"],
            capture_output=True, text=True, timeout=5,
        )
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout.strip()
    except Exception:
        pass
    sys.exit(
        f"No API key. Set ANTHROPIC_API_KEY, or store one in the Keychain "
        f"(service {KEYCHAIN_SERVICE}, account {KEYCHAIN_ACCOUNT}) via the HUD "
        f"settings panel."
    )


def avoid_instruction(avoid):
    """Mirror of daemon anthropic.rs::avoid_instruction — None when empty."""
    recent = [r.strip() for r in avoid if r.strip()]
    if not recent:
        return None
    lines = ['Vary your phrasing — do NOT reuse the wording, opening, or shape '
             'of your recent replies:']
    lines += [f'- "{r}"' for r in recent]
    lines.append("Say something genuinely fresh, in your own voice.")
    return "\n".join(lines)


def call(key, persona, history, utterance, avoid):
    system = persona
    note = avoid_instruction(avoid)
    if note:
        system = (system + "\n\n" + note) if system else note
    messages = []
    for user, darwin in history:
        if user.strip() and darwin.strip():
            messages.append({"role": "user", "content": user})
            messages.append({"role": "assistant", "content": darwin})
    messages.append({"role": "user", "content": utterance})
    body = {"model": MODEL, "max_tokens": MAX_TOKENS, "messages": messages}
    if system:
        body["system"] = system
    req = urllib.request.Request(
        "https://api.anthropic.com/v1/messages",
        data=json.dumps(body).encode(),
        headers={"x-api-key": key,
                 "anthropic-version": "2023-06-01",
                 "content-type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            data = json.load(r)
    except urllib.error.HTTPError as e:
        sys.exit(f"HTTP {e.code}: {e.read().decode()[:300]}")
    return " ".join(b.get("text", "") for b in data.get("content", [])
                    if b.get("type") == "text").strip()


def main():
    phrase = sys.argv[1] if len(sys.argv) > 1 else "Hi DARWIN"
    rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    key = resolve_key()
    persona = PERSONA_PATH.read_text().strip() if PERSONA_PATH.exists() else ""
    if not persona:
        print("warning: persona.txt empty/missing — replies will be ungrounded\n")

    history = []          # (user, darwin) pairs, oldest-first (real chat turns)
    replies = []          # all replies, for the freshest-first avoid list
    print(f'Probing {rounds}x  "{phrase}"  through {MODEL} '
          f'(no temperature; avoid-list cap {AVOID_CAP})\n')
    for i in range(1, rounds + 1):
        avoid = list(reversed(replies))[:AVOID_CAP]   # freshest-first, capped
        reply = call(key, persona, history, phrase, avoid)
        print(f"{i:>2}. (avoid={len(avoid)})  {reply}")
        history.append((phrase, reply))
        replies.append(reply)

    distinct = len(set(replies))
    print(f"\n{distinct}/{rounds} distinct.", "VARIED." if distinct > 1
          else "COLLAPSED — still identical.")


if __name__ == "__main__":
    main()
