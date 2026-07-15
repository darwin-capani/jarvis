#!/bin/bash
# install.sh — the ONE command that installs ALL of D.A.R.W.I.N., built FRESH,
# into a per-user home with no sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/darwin-capani/darwin/main/install.sh | bash
#
# (./install.sh also works from a local clone — it copies *this* clone into the
#  install home, builds every artifact fresh there, and loads the LaunchAgents.)
#
# Install home (per-user, no sudo, relocatable — the daemon + boot wrappers are
# DARWIN_ROOT-relative, so the tree runs from wherever it lands):
#
#   ~/Library/Application Support/DARWIN
#
# What it does (staged):
#   1. PREFLIGHT  — macOS + arm64, Xcode CLT, Rust, Python 3.11, Node/npm.
#                   AUTO-PROVISIONS every installable build prereq so a fresh Mac
#                   just works: the Xcode Command Line Tools (triggers Apple's
#                   installer + WAITS for it), Rust via rustup, and Python 3.11 +
#                   Node via Homebrew (bootstrapping Homebrew itself if absent).
#                   Opt out with --no-provision (then a missing dep is fatal).
#                   macOS / arm64 remain HONEST hard checks (no software fixes a
#                   non-Apple-Silicon Mac). CLT has no unattended installer, so we
#                   open its GUI dialog and poll until it verifies (one click,
#                   like the Homebrew password prompt).
#   2. PLACE      — copy the project tree into the install home (excluding
#                   built/fetched dirs: target .venv node_modules .build state models)
#   3. PYTHON ENV — python3.11 -m venv, upgrade pip, install the FULL dep set
#   4. MODELS     — pre-download every model the OS uses into HF_HOME, and
#                   persist that HF_HOME into state/env.sh so the RUNTIME (the
#                   boot wrappers source it) reads the SAME cache
#   5. BUILD      — cargo build --release (daemon + apps), swift build, HUD/Tauri .app
#   6. AUTOSTART  — consent-gated (see --yes): render + load the 2 LaunchAgents
#                   via scripts/install_boot.sh --install (RunAtLoad STARTS DARWIN)
#   7. FINISH     — "DARWIN IS ONLINE" + honest next-steps (TCC grants, keys, wake word)
#
# Flags:
#   --check / --dry-run   print the full plan and run only READ-ONLY detection
#                         (prereq provisioning is PLANNED, never executed)
#   --yes / -y            assume "yes" to the consent prompts (the DARWIN-
#                         actuation confirm()s). Today that is ONE gate: stage 6
#                         AUTOSTART, loading the LaunchAgents that START the
#                         armed OS (declining leaves autostart MANUAL, enable
#                         later via scripts/install_boot.sh --install). Build-
#                         prereq provisioning is NOT gated on this; it runs by
#                         default, see --no-provision.
#   --no-provision        DO NOT auto-install build prereqs. Revert to detect +
#                         instruct + fatal-if-missing (Rust/Python3.11/Node).
#                         Provisioning is ON by default; use this to opt out.
#   --no-models           skip the model pre-download stage (build everything else)
#   --help / -h           this help
#
# Idempotent + resumable: re-running skips work already done (existing venv,
# already-downloaded models, up-to-date release binaries) and is safe to ctrl-C.
#
# HONESTY: this is a FULL-POWER default — consequential actions are ARMED (the
# master switch ships ON). What protects you is that every consequential action
# STILL requires a fresh per-action confirm + voice-id (if enrolled) + per-action
# policy + !lockdown, all enforced at the chokepoints (not defaults you can flip
# away). Lockdown/panic forces everything off. Many features ship ON but stay
# inert until you supply the dependency: cloud LLM / ElevenLabs voice+STT / self-
# heal+forge drafting need an API key; UI automation / mic / screen-context need a
# macOS TCC grant; docsearch/code need an allowlisted folder; MCP/webhooks need a
# server-or-mapping. No secret, key, state DB, venv, model, or build artifact is
# ever written into the source repo.

set -euo pipefail

# ----------------------------------------------------------------------------
# Self-bootstrap (curl | bash) — fetch the whole source tree, then re-run.
#
# The installer needs the ENTIRE repo (it copies the tree into the install home
# and builds every artifact fresh, sourcing scripts/ui.sh + reading config/
# inference/daemon/hud/...). When piped via `curl ... | bash` there is nothing
# local: BASH_SOURCE[0] is unset and scripts/ui.sh is absent. So if we are not
# running from a real clone we fetch one (tarball first, git fallback), then
# re-run THIS script from inside it. A local `./install.sh` skips all of this
# and behaves exactly as before.
# ----------------------------------------------------------------------------

# SAFE SELF-LOCATION (set -u clean): never reference ${BASH_SOURCE[0]} bare.
_self="${BASH_SOURCE[0]:-}"
_src_root=""
[ -n "$_self" ] && _src_root="$(cd "$(dirname "$_self")" 2>/dev/null && pwd)"

# We are a real clone iff this script's dir exists AND has scripts/ui.sh next to
# it. If not (piped / standalone) AND we have not already bootstrapped, fetch.
if [ -z "${DARWIN_BOOTSTRAPPED:-}" ] && { [ -z "$_src_root" ] || [ ! -f "$_src_root/scripts/ui.sh" ]; }; then
    echo "  D.A.R.W.I.N. installer — fetching source…" >&2

    _tmp="$(mktemp -d "${TMPDIR:-/tmp}/darwin-install.XXXXXX")"
    # Remove the temp tree on ANY exit of THIS (bootstrap) shell. The child
    # install.sh runs in a SEPARATE process with its own ui.sh cursor traps, so
    # this EXIT/INT/TERM trap is independent of (does not clobber) the child's.
    trap 'rm -rf "$_tmp"' EXIT INT TERM

    _fetched=""
    # Tarball FIRST — no git / Xcode CLT needed. Extracts as darwin-main/.
    if curl -fsSL "https://codeload.github.com/darwin-capani/darwin/tar.gz/refs/heads/main" \
        | tar -xz -C "$_tmp" 2>/dev/null; then
        _fetched=1
    elif command -v git >/dev/null 2>&1 \
        && git clone --depth 1 "https://github.com/darwin-capani/darwin" "$_tmp/darwin-main" >/dev/null 2>&1; then
        _fetched=1
    fi

    if [ -z "$_fetched" ]; then
        echo "error: could not fetch the DARWIN source." >&2
        echo "       Need network access plus curl (or git)." >&2
        echo "       Manual: git clone https://github.com/darwin-capani/darwin && cd darwin && ./install.sh" >&2
        exit 1
    fi

    # Resolve the fetched dir (the darwin* dir under $_tmp holding install.sh).
    _dir=""
    for _cand in "$_tmp"/darwin-main "$_tmp"/darwin-* "$_tmp"/darwin; do
        if [ -f "$_cand/install.sh" ] && [ -f "$_cand/scripts/ui.sh" ]; then
            _dir="$_cand"; break
        fi
    done
    if [ -z "$_dir" ]; then
        echo "error: fetched archive is missing install.sh or scripts/ui.sh." >&2
        exit 1
    fi

    # Re-run from the real tree, passing ALL args through. RUN (not exec) so the
    # EXIT trap above can remove $_tmp after the child finishes; capture the
    # child's exit code so set -e does not abort before we propagate it.
    DARWIN_BOOTSTRAPPED=1 bash "$_dir/install.sh" "$@"
    rc=$?
    exit "$rc"
fi

# ----------------------------------------------------------------------------
# Locate the source tree (this script's dir) and source the UI library.
# ----------------------------------------------------------------------------
SRC_ROOT="$_src_root"

# shellcheck source=scripts/ui.sh
if [ -f "$SRC_ROOT/scripts/ui.sh" ]; then
    # shellcheck disable=SC1091
    source "$SRC_ROOT/scripts/ui.sh"
else
    echo "error: scripts/ui.sh not found next to install.sh (run from a DARWIN clone)" >&2
    exit 1
fi
ui_init

# ----------------------------------------------------------------------------
# Configuration.
# ----------------------------------------------------------------------------
DARWIN_HOME="$HOME/Library/Application Support/DARWIN"
PY311_CANDIDATES=(
    "/opt/homebrew/bin/python3.11"
    "/usr/local/bin/python3.11"
    "python3.11"
)
CARGO="$HOME/.cargo/bin/cargo"
TOTAL_STAGES=7

# Model ids — kept in sync with inference/server.py DEFAULT_* constants. The
# installer reads server.py at runtime (below) and these are only the fallback
# if that parse fails, so an out-of-date list here never silently ships.
FALLBACK_LLM="mlx-community/Qwen3-4B-Instruct-2507-4bit"
FALLBACK_STT="mlx-community/whisper-small-mlx"
FALLBACK_TTS="mlx-community/Kokoro-82M-bf16"
FALLBACK_VLM="mlx-community/Qwen2-VL-2B-Instruct-4bit"
FALLBACK_DRAFT="mlx-community/Qwen3-0.6B-4bit"

# Dirs that are BUILT or FETCHED fresh in the install home and must NOT be copied
# from the source tree (so we never ship a stale daemon binary, a wrong-path
# venv, someone else's state DB, or gigabytes of model weights).
EXCLUDE_DIRS=(target .venv node_modules .build .git state models dist gen)

# ----------------------------------------------------------------------------
# Flag parsing.
# ----------------------------------------------------------------------------
MODE="install"
ASSUME_YES=0
DO_MODELS=1
DO_PROVISION=1   # auto-provision missing build prereqs (Rust/Python3.11/Node + Homebrew); --no-provision opts out

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check|--dry-run) MODE="check" ;;
        -y|--yes)          ASSUME_YES=1 ;;
        --no-provision)    DO_PROVISION=0 ;;
        --no-models)       DO_MODELS=0 ;;
        -h|--help)
            # Print the header comment block (the doc lines above `set -euo
            # pipefail`) as the help text, stripping the leading "# ".
            sed -n '2,65p' "${BASH_SOURCE[0]:-$SRC_ROOT/install.sh}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            ui_err "unknown argument: $1 (try --help)"
            exit 2
            ;;
    esac
    shift
done

# ----------------------------------------------------------------------------
# Small helpers.
# ----------------------------------------------------------------------------

# Ask for consent unless --yes; in --check mode never prompt (assume no).
# Piped stdin (curl | bash) prompts via the controlling terminal when one
# exists (the same /dev/tty pattern ensure_homebrew uses); with no tty at all
# (true non-interactive run / CI) it DECLINES — fail-safe; pass -y to accept.
confirm() {
    local prompt="$1" in="/dev/stdin"
    [ "$ASSUME_YES" -eq 1 ] && return 0
    [ "$MODE" = "check" ] && return 1
    if [ ! -t 0 ]; then
        if { : < /dev/tty; } 2>/dev/null; then in="/dev/tty"; else return 1; fi
    fi
    local reply=""
    printf '  %s%s %s [y/N] %s' "${UI_BOLD}${UI_CYAN}" "$UI_G_INFO" "$prompt" "$UI_RESET"
    read -r reply < "$in" || true
    case "$reply" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

# Find a python3.11 interpreter; echo its path, or empty if none.
find_py311() {
    local p
    for p in "${PY311_CANDIDATES[@]}"; do
        if command -v "$p" >/dev/null 2>&1; then
            # Confirm it really is 3.11 (a bare "python3.11" on PATH might lie).
            if "$p" -c 'import sys; raise SystemExit(0 if sys.version_info[:2]==(3,11) else 1)' >/dev/null 2>&1; then
                command -v "$p"
                return 0
            fi
        fi
    done
    return 0
}

# Read a DEFAULT_* model id out of server.py; falls back to the constant given.
read_model_id() {
    local key="$1" fallback="$2" line=""
    line="$(grep -m1 -E "^${key}[[:space:]]*=" "$SRC_ROOT/inference/server.py" 2>/dev/null || true)"
    # Extract the quoted value.
    line="${line#*\"}"; line="${line%%\"*}"
    if [ -n "$line" ]; then printf '%s' "$line"; else printf '%s' "$fallback"; fi
}

# Plan-only printer for --check: describe a command instead of running it.
plan() { ui_note "would run: $*"; }

# QUIET-SUB-INSTALLER runner. Runs a (usually noisy) sub-installer with its
# stdout+stderr CAPTURED to a temp log so only the cinematic ui_spin spinner +
# a clean ui_ok appear on the HUD — never the raw rustup/Homebrew/brew chatter.
# On FAILURE it prints the last ~15 lines of that log (the actual cause) via a
# ui_err + dim tail, then returns non-zero so the caller can gate. The temp log
# is always removed. Usage:  run_quiet "installing <tool>" -- <command> [args...]
# (Pass the command through `--`, exactly like ui_spin.) Returns the command's
# exit status. set -e safe: callers MUST use it inside `if run_quiet ...; then`.
run_quiet() {
    local label="$1"; shift
    [ "${1:-}" = "--" ] && shift
    if [ "$#" -eq 0 ]; then ui_warn "run_quiet: no command given"; return 0; fi
    local log rc=0
    log="$(mktemp "${TMPDIR:-/tmp}/darwin-provision.XXXXXX")" || { ui_err "$label (could not create a log file)"; return 1; }
    # ui_spin animates the spinner and reports ok/err; the inner bash -c funnels
    # BOTH streams of the real installer into $log so nothing leaks onto the HUD.
    # `set -e` would abort on ui_spin's non-zero, so capture rc behind `|| rc=$?`.
    ui_spin "$label" -- bash -c '"$@" >"$0" 2>&1' "$log" "$@" || rc=$?
    if [ "$rc" -ne 0 ]; then
        ui_note "last lines of the install log:"
        # Last ~15 lines, indented + dimmed so the cause is legible but quiet.
        tail -n 15 "$log" 2>/dev/null | while IFS= read -r _ln; do
            printf '      %s%s%s\n' "${UI_DIM:-}" "$_ln" "${UI_RESET:-}"
        done
    fi
    rm -f "$log" 2>/dev/null || true
    return "$rc"
}

# ----------------------------------------------------------------------------
# Build-prereq provisioning helpers (STAGE 1).
#
# These auto-install the INSTALLABLE build prereqs (Rust via rustup; Python 3.11
# + Node via Homebrew, bootstrapping Homebrew itself if absent) so a fresh Mac
# just works. They are NOT gated on the DARWIN consequential confirm() — a build
# toolchain is a prerequisite, not DARWIN actuation. They run only when
# DO_PROVISION=1 and MODE=install; --check only PLANS them and --no-provision
# disables them (reverting to detect + instruct + fatal). Everything is
# idempotent: anything already present is detected and skipped.
# ----------------------------------------------------------------------------

# Locate the Homebrew binary if it exists; echo its path, or empty if none.
find_brew() {
    if command -v brew >/dev/null 2>&1; then command -v brew; return 0; fi
    if [ -x /opt/homebrew/bin/brew ]; then echo /opt/homebrew/bin/brew; return 0; fi
    if [ -x /usr/local/bin/brew ];  then echo /usr/local/bin/brew;  return 0; fi
    return 0
}

# Is a USABLE Command Line Tools toolchain present? True only when a developer
# dir is selected AND clang actually runs. The short-circuit is deliberate: we
# never invoke clang until `xcode-select -p` succeeds, so mere DETECTION can't
# trip macOS's own "install command line tools" GUI prompt out from under us.
clt_present() {
    xcode-select -p >/dev/null 2>&1 && clang --version >/dev/null 2>&1
}

# Ensure the Xcode Command Line Tools (clang / git / make) are installed — the
# FOUNDATIONAL build prereq: Homebrew's bootstrap, the Rust linker, every native
# `cargo build`, and `swift build` ALL require them. Run FIRST in preflight so the
# brew/rustup/python/node steps that follow have a working toolchain.
#
# Unlike brew/rustup, CLT has NO silent unattended installer — `xcode-select
# --install` opens a macOS GUI dialog and returns immediately. So we TRIGGER that
# dialog and then WAIT (poll) until clang actually runs, with honest heartbeats
# and a generous timeout. Same interaction model as the Homebrew password prompt:
# the installer runs in Terminal, the user completes one OS dialog, we proceed
# once it is really done. Returns 0 only when clang verifies; 1 on timeout / no
# GUI session to complete the dialog (the caller then sets PREFLIGHT_FATAL).
ensure_xcode_clt() {
    clt_present && return 0

    # Trigger Apple's GUI installer. Non-zero if a download is already in progress
    # or the tools are partially present — harmless; we poll regardless. </dev/null
    # so a piped stdin (curl | bash) can't wedge it.
    ui_info "Opening the macOS ${UI_BRIGHT}Command Line Tools${UI_RESET} installer — click ${UI_BRIGHT}Install${UI_RESET} in the dialog and accept the licence."
    xcode-select --install >/dev/null 2>&1 </dev/null || true

    # Poll until clang works, up to ~40 min (the CLT download is ~1 GB and can be
    # slow on a poor connection); the common case exits the instant clang runs.
    # Heartbeat every 60s so a long download never looks hung, and the user can
    # Ctrl-C if they dismissed the dialog.
    local waited=0 max=2400
    while ! clt_present; do
        sleep 10
        waited=$(( waited + 10 ))
        if [ "$waited" -ge "$max" ]; then
            return 1
        fi
        if [ $(( waited % 60 )) -eq 0 ]; then
            ui_info "…still installing the Command Line Tools (${waited}s elapsed; this is a large download)."
        fi
    done
    return 0
}

# Ensure Homebrew is available + on PATH for the rest of THIS process. Called
# LAZILY — only when python3.11 OR node is missing (never on a fully-provisioned
# machine). Returns 0 if brew is usable, 1 if it could not be made available
# (the caller then sets PREFLIGHT_FATAL so the run STOPS before STAGE 2 PLACE).
#
#   - If brew is already present -> eval its shellenv (puts brew + its installs
#     on PATH for this process) and return 0 (idempotent, NO prompt).
#   - MODE=check  -> plan the bootstrap line, return 0 (installs NOTHING).
#   - MODE=install + absent -> a robust INTERACTIVE bootstrap that actually
#     succeeds under `curl … | bash` (where stdin is the pipe, not a terminal):
#       a. require a usable controlling terminal (/dev/tty);
#       b. announce honestly that admin access is needed;
#       c. ACQUIRE sudo through /dev/tty so the macOS password prompt works
#          despite our piped stdin (`sudo -v < /dev/tty`);
#       d. KEEP the sudo timestamp ALIVE with a background refresher during the
#          slow install, KILLED on every exit path (NOT via a competing trap —
#          we kill the PID explicitly so ui.sh's cursor EXIT trap is untouched);
#       e. run the official Homebrew installer (NONINTERACTIVE, sudo now cached),
#          output captured to a temp log (quiet HUD);
#       f. kill the refresher, re-locate brew, eval shellenv, and VERIFY
#          `brew --version` actually runs before returning success.
#     Returns 0 only when brew VERIFIES; 1 (with a clear ui_err + ui_note) on
#     no-tty, not-admin, or an install that did not produce a usable brew.
ensure_homebrew() {
    local brew
    brew="$(find_brew)"
    if [ -n "$brew" ]; then
        eval "$("$brew" shellenv)"
        return 0
    fi

    if [ "$MODE" = "check" ]; then
        plan "sudo -v   # cache admin (password via /dev/tty); then:"
        plan "NONINTERACTIVE=1 /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"   # install Homebrew"
        return 0
    fi

    # --- (a) require a usable controlling terminal --------------------------
    # Under `curl … | bash` our own stdin is the pipe, so we cannot read a
    # password from it. The Homebrew installer's sudo needs a real terminal; we
    # supply /dev/tty. If there is none (true non-interactive run / CI), we
    # cannot prompt for admin at all — fail honestly instead of looping.
    if ! { : < /dev/tty; } 2>/dev/null; then
        ui_err "Homebrew install needs an interactive terminal."
        ui_note "Run the installer in Terminal, or install Homebrew first (https://brew.sh) then re-run."
        return 1
    fi

    # --- (b) announce honestly ---------------------------------------------
    ui_info "Homebrew is required and needs administrator access — macOS will ask for your Mac login password (your account must be an administrator)."

    # --- (c) acquire sudo THROUGH the terminal ------------------------------
    # `< /dev/tty` lets the password prompt reach the user even though our stdin
    # is the curl pipe. A wrong password or a non-admin account fails here.
    if ! sudo -v < /dev/tty; then
        ui_err "Could not get administrator access (wrong password, or this account is not an admin)."
        ui_note "Have an admin run the installer, or pre-install Homebrew / Python 3.11 + Node, then re-run."
        return 1
    fi

    # --- (d) keep the sudo timestamp alive during the slow install ----------
    # A background refresher tops up the cached credential every 50s; it stops
    # itself the instant sudo can no longer refresh non-interactively. We KILL
    # it explicitly on every exit path below (success AND failure) so we never
    # add a competing EXIT trap that would clobber ui.sh's cursor-restore trap.
    #
    # Hardened against leaving a stray `sleep` behind on EVERY teardown path:
    #   - Normal end (success/failure): the caller `kill`s + `wait`s this PID; the
    #     subshell's INT/TERM trap kills its own current `sleep` child first, so
    #     nothing is reparented. (We kill only our OWN child "$_s" — never the
    #     process group: `kill 0` / `kill -<pgid>` would also hit the installer.)
    #   - Abnormal end (Ctrl-C delivered to the whole group, or a SIGKILL where no
    #     trap can run): the 50s wait is split into short `sleep 2` chunks, so even
    #     an orphaned chunk self-exits within ~2s instead of lingering up to ~50s.
    # Net: the refresher still tops up sudo on the ~50s cadence, but no stray
    # process can outlive the install by more than a couple of seconds on any path.
    local _sudo_keepalive_pid=""
    ( _s=""
      trap 'kill "$_s" 2>/dev/null; exit 0' INT TERM
      while true; do
          sudo -n true 2>/dev/null || break    # stop the instant we can't refresh
          _i=0
          while [ "$_i" -lt 25 ]; do           # 25 x 2s ~= 50s between refreshes
              sleep 2 & _s=$!
              wait "$_s" || break
              _i=$(( _i + 1 ))
          done
      done ) &
    _sudo_keepalive_pid=$!

    # --- (e) run the official Homebrew installer (quiet, sudo cached) -------
    # NONINTERACTIVE=1 so it does not pause for confirmation; sudo is already
    # cached so its internal sudo calls are passwordless. </dev/tty as belt-and-
    # suspenders. run_quiet captures all output to a temp log + shows the clean
    # spinner; on failure it prints the last ~15 log lines (the real cause).
    local _hb_rc=0
    run_quiet "installing Homebrew" -- bash -c \
        'NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" </dev/tty' \
        || _hb_rc=$?

    # --- (f) kill the refresher, then re-locate + VERIFY brew ---------------
    # Kill the keep-alive on BOTH the success and failure path (explicit PID
    # kill — never a trap), so no orphaned refresher survives this function.
    if [ -n "$_sudo_keepalive_pid" ]; then
        kill "$_sudo_keepalive_pid" 2>/dev/null || true
        wait "$_sudo_keepalive_pid" 2>/dev/null || true
    fi

    if [ "$_hb_rc" -ne 0 ]; then
        ui_err "Homebrew installation failed (see the log lines above)."
        ui_note "Check your network / disk, or install Homebrew manually from https://brew.sh, then re-run."
        return 1
    fi

    brew="$(find_brew)"
    if [ -n "$brew" ]; then
        eval "$("$brew" shellenv)"
        if "$brew" --version >/dev/null 2>&1; then
            ui_ok "Homebrew installed: $("$brew" --version 2>/dev/null | head -1)"
            return 0
        fi
    fi
    ui_err "Homebrew installer ran but produced no working 'brew' (could not verify brew --version)."
    ui_note "Install Homebrew manually from https://brew.sh, then re-run."
    return 1
}

# ----------------------------------------------------------------------------
# HUD-APP PLACEMENT (STAGE 5 helper).
#
# The Tauri build produces DARWIN.app inside the build tree. On its own that is
# not discoverable — a user can't find or launch it. These helpers INSTALL the
# built app into /Applications (or ~/Applications) so the command-line install
# ends with a launchable app right where macOS users expect it, and set
# INSTALLED_APP_PATH for the "OPEN DARWIN" next-step.
# ----------------------------------------------------------------------------

# Is the app at $1 a NOTARIZED / Developer-ID-signed build (i.e. the official
# .dmg release) rather than an ad-hoc local build? Used so we NEVER overwrite the
# signed app with this locally-built one (that would downgrade the signature and
# could clobber a running copy in the .dmg first-run flow).
is_notarized_app() {
    codesign -dvv "$1" 2>&1 | grep -q "Authority=Developer ID Application"
}

# Install the freshly-built DARWIN.app ($1) into the first writable Applications
# dir. Sets the global INSTALLED_APP_PATH to where it ended up. Never fatal — if
# every copy fails, INSTALLED_APP_PATH falls back to the build path (the app DOES
# exist there) with an honest "move it yourself" note. Idempotent: a prior local
# build is replaced (an update); a notarized app is LEFT IN PLACE.
place_hud_app() {
    local src="$1"
    INSTALLED_APP_PATH=""
    if [ -z "$src" ] || [ ! -d "$src" ]; then
        ui_warn "tauri build produced no DARWIN.app to install"
        return 0
    fi

    local cand dest
    # Preference order: system /Applications (where users look first), then the
    # user's ~/Applications (always writable — no admin needed).
    for cand in "/Applications" "$HOME/Applications"; do
        { [ -d "$cand" ] || mkdir -p "$cand" 2>/dev/null; } || continue
        [ -w "$cand" ] || continue
        dest="$cand/DARWIN.app"

        # Never downgrade/clobber the official signed app (the .dmg build, which
        # may also be RUNNING in the first-run flow) — leave it, point the user at it.
        if [ -d "$dest" ] && is_notarized_app "$dest"; then
            ui_ok "DARWIN.app already installed (signed) at $dest — left in place."
            INSTALLED_APP_PATH="$dest"
            return 0
        fi

        # Fresh install, or updating a prior LOCAL build: replace it cleanly.
        if rm -rf "$dest" 2>/dev/null && ditto "$src" "$dest" 2>/dev/null; then
            ui_ok "HUD app installed: $dest"
            INSTALLED_APP_PATH="$dest"
            return 0
        fi
        # else: not writable for this item / ditto failed — try the next candidate.
    done

    # Could not place it anywhere — honest: the app exists in the build tree.
    ui_warn "Could not copy DARWIN.app into Applications (permissions?)."
    ui_note "It is built at:  $src"
    ui_note "Move it with:    ditto \"$src\" \"\$HOME/Applications/DARWIN.app\""
    INSTALLED_APP_PATH="$src"
    return 0
}

# FUTURISTIC NEXT-STEPS — the honest "directive" panels (cohesive HUD cards).
# Defined ONCE so the real-install finish and the --check preview render the SAME
# substance (no fork can let the two drift): every fact — keys, paths, commands,
# caveats — is preserved verbatim; ui_panel only adds framing. $1 is the lead-line
# suffix so the preview can mark itself "(preview)" without duplicating the cards.
print_next_steps_directives() {
    local lead_note="${1:-}"
    # Where the HUD app landed (set by place_hud_app in a real install); in --check
    # the build has not run, so show the default destination it WOULD install to.
    local _app_path="${INSTALLED_APP_PATH:-/Applications/DARWIN.app}"
    printf '\n'
    ui_info "${UI_BOLD}${UI_CYAN}NEXT-STEP DIRECTIVES${UI_RESET} — full-power default: consequential actions are ARMED, still gated per action${lead_note}:"

    ui_panel "1" "OPEN DARWIN" \
        "The HUD app is installed at ${UI_BRIGHT}${_app_path}${UI_RESET}." \
        "Open it from ${UI_ICE}Finder > Applications${UI_RESET}, or run:" \
        "  ${UI_CYAN}open -a DARWIN${UI_RESET}" \
        "It connects to the daemon automatically; the first launch triggers the" \
        "macOS permission prompts described next."

    ui_panel "2" "TCC PERMISSIONS" \
        "macOS will prompt for ${UI_BRIGHT}Accessibility${UI_RESET}, ${UI_BRIGHT}Microphone${UI_RESET}, and ${UI_BRIGHT}Screen Recording${UI_RESET}" \
        "the first time DARWIN needs each. Many full-power features (UI automation," \
        "live interpret, sound monitor, screen context) stay ${UI_BRIGHT}inert${UI_RESET} until you grant" \
        "these in ${UI_ICE}System Settings > Privacy & Security${UI_RESET}. They cannot be pre-granted."

    ui_panel "3" "API KEYS  (gates ON, but inert until set)" \
        "The feature gates ship ${UI_BRIGHT}ON${UI_RESET}, but key-dependent capabilities stay" \
        "${UI_BRIGHT}inert until a key is supplied${UI_RESET}: the cloud LLM fallback, the ElevenLabs cloud" \
        "voice/STT tier, and self-heal/forge drafting all need a key. To enable, put" \
        "  ${UI_CYAN}export ANTHROPIC_API_KEY=...${UI_RESET}" \
        "  ${UI_CYAN}export ELEVENLABS_API_KEY=...${UI_RESET}" \
        "in  ${UI_GREY}$DARWIN_HOME/state/env.sh${UI_RESET}  and  ${UI_BRIGHT}chmod 600${UI_RESET}  it (state/ is gitignored)," \
        "or store them in the macOS Keychain. Local inference works fully offline" \
        "with no key at all; enabling a gate != active without the key."

    ui_panel "4" "IMAGE GENERATION  (FLUX — gated model)" \
        "Image generation uses ${UI_BRIGHT}FLUX.1-schnell${UI_RESET}, a ${UI_BRIGHT}GATED${UI_RESET} Hugging Face model" \
        "that cannot download anonymously — so it stays ${UI_BRIGHT}inert${UI_RESET} until you authorize it." \
        "To enable image generation:" \
        "  1. accept the licence (free) at" \
        "     ${UI_ICE}https://huggingface.co/black-forest-labs/FLUX.1-schnell${UI_RESET}" \
        "  2. authenticate:  ${UI_CYAN}\"$DARWIN_HOME/.venv/bin/hf\" auth login${UI_RESET}" \
        "     (or  ${UI_CYAN}export HF_TOKEN=hf_...${UI_RESET}  before re-running)" \
        "  3. re-run the installer — it resumes from cache and pulls FLUX with your token." \
        "Everything else works without this; only image generation needs it."

    ui_panel "5" "VOICE WAKE WORD  +  GATES THAT STAY ENFORCED" \
        "Say \"${UI_BRIGHT}DARWIN${UI_RESET}\" to wake it once the daemon is up." \
        "The installer ships the ${UI_BRIGHT}master switch ON${UI_RESET} (consequential actions ARMED)," \
        "but every consequential action STILL requires a ${UI_BRIGHT}fresh per-action confirm${UI_RESET} +" \
        "voice-id (if enrolled) + per-action policy + ${UI_BRIGHT}!lockdown${UI_RESET} — these are enforced at" \
        "the chokepoints, not defaults you can flip away. Self-healing is ${UI_BRIGHT}ON but" \
        "PROPOSE-ONLY${UI_RESET} (drafts a validated patch you apply via scripts/apply_heal.sh," \
        "and inert until ANTHROPIC_API_KEY is set). Lockdown/panic forces everything off."

    ui_panel "6" "BOOT-TO-DARWIN" \
        "For a deployment Mac, enable auto-login so the gui-domain agents start at" \
        "power-on (see ${UI_ICE}scripts/install_boot.sh${UI_RESET} checklist)." \
        "${UI_YELLOW}Do not install these agents on a dev machine.${UI_RESET}"

    ui_panel "7" "REMOVE DARWIN COMPLETELY" \
        "Run" \
        "  ${UI_CYAN}\"$DARWIN_HOME/uninstall.sh\"${UI_RESET}" \
        "(two typed confirmations; --dry-run to preview)."

    printf '\n'
    return 0
}

# ----------------------------------------------------------------------------
# Banner.
# ----------------------------------------------------------------------------
# The banner runs the arc-reactor power-up + system diagnostic (motion only on
# an interactive tty), frames the HUD with "OPERATOR // $DARWIN_OPERATOR" along
# the top edge, and greets the operator by name as the system comes online.
darwin_banner
if [ "$MODE" = "check" ]; then
    ui_info "DRY RUN (--check): printing the plan + read-only detection only."
    ui_info "No venv, no downloads, no build, no ~/Library writes, no launchctl."
fi
ui_info "Operator:     ${DARWIN_OPERATOR}"
ui_info "Source tree:  $SRC_ROOT"
ui_info "Install home: $DARWIN_HOME"

# ============================================================================
# STAGE 1 — PREFLIGHT
# ============================================================================
ui_stage 1 "$TOTAL_STAGES" "PREFLIGHT"

PREFLIGHT_FATAL=0

# --- OS + arch ---
OS_NAME="$(uname -s)"
ARCH="$(uname -m)"
if [ "$OS_NAME" != "Darwin" ]; then
    ui_err "DARWIN requires macOS (found: $OS_NAME)."
    PREFLIGHT_FATAL=1
elif [ "$ARCH" != "arm64" ]; then
    ui_err "DARWIN requires Apple Silicon (arm64). Found: $ARCH — Intel Macs are unsupported (MLX is Metal/Apple-GPU only)."
    PREFLIGHT_FATAL=1
else
    OS_VER="$(sw_vers -productVersion 2>/dev/null || echo '?')"
    ui_ok "macOS $OS_VER on $ARCH (Apple Silicon)"
fi

# --- Xcode Command Line Tools (clang/git/make — the FOUNDATIONAL build prereq:
#     Homebrew, the Rust linker, every native compile + swift build need them;
#     AUTO-PROVISIONED when missing, exactly like Rust/Python/Node below) ---
if clt_present; then
    ui_ok "Xcode Command Line Tools: $(xcode-select -p)"
elif [ "$MODE" = "check" ]; then
    ui_warn "Xcode Command Line Tools not found."
    if [ "$DO_PROVISION" -eq 1 ]; then
        plan "xcode-select --install   # opens the macOS dialog; the installer WAITS for it (no fatal in a normal install)"
    else
        ui_note "--no-provision: install with:  xcode-select --install   (then re-run)"
        PREFLIGHT_FATAL=1
    fi
elif [ "$DO_PROVISION" -eq 0 ]; then
    # Opt-out: revert to the old detect + instruct + fatal behavior.
    ui_warn "Xcode Command Line Tools not found."
    ui_note "--no-provision: install with:  xcode-select --install   (then re-run)"
    ui_err "The Command Line Tools (clang/git/make) are required to build DARWIN."
    PREFLIGHT_FATAL=1
else
    # MODE=install + provisioning ON: trigger Apple's CLT installer and WAIT for
    # it to finish (it has no unattended mode — see ensure_xcode_clt). This runs
    # FIRST so Homebrew/rustup below have a working toolchain.
    ui_info "Xcode Command Line Tools not found — installing them now (a macOS dialog will open)."
    if ensure_xcode_clt; then
        ui_ok "Xcode Command Line Tools ready: $(xcode-select -p)"
    else
        ui_err "Xcode Command Line Tools did not install in time (or there is no GUI session to complete the dialog)."
        ui_note "Run  xcode-select --install  in Terminal, complete the dialog, then re-run this installer."
        PREFLIGHT_FATAL=1
    fi
fi

# --- Rust toolchain (auto-provisioned via rustup when missing) ---
# Idempotent: a present cargo is detected + reused, never reinstalled.
if [ -x "$CARGO" ] || command -v cargo >/dev/null 2>&1; then
    [ -x "$CARGO" ] || CARGO="$(command -v cargo)"
    # Source the cargo env so a freshly-installed toolchain is on PATH before probing.
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    _rustup="$HOME/.cargo/bin/rustup"; [ -x "$_rustup" ] || _rustup="$(command -v rustup 2>/dev/null || echo rustup)"
    _rustc="$HOME/.cargo/bin/rustc";   [ -x "$_rustc" ]  || _rustc="$(command -v rustc 2>/dev/null || echo rustc)"
    # VERIFY the toolchain is BUILD-CAPABLE — not just that cargo exists. The build
    # invokes `rustc -vV`, and a rustup proxy with NO default toolchain OR a
    # BROKEN/partial one ("Missing manifest in toolchain ...") must be caught HERE:
    # `cargo --version` can PASS while `rustc -vV` FAILS, so we probe BOTH. Masking
    # either (the old `|| echo cargo`) lets it sail through preflight and die at BUILD.
    if "$CARGO" --version >/dev/null 2>&1 && "$_rustc" -vV >/dev/null 2>&1; then
        ui_ok "Rust toolchain: $("$CARGO" --version)"
    elif [ "$MODE" = "check" ]; then
        ui_warn "cargo is present but its toolchain is missing/broken (no default, or a corrupt stable)."
        if [ "$DO_PROVISION" -eq 1 ]; then
            plan "rustup toolchain install stable && rustup default stable   # repair the Rust toolchain so the build can run"
        else
            ui_note "--no-provision: run  rustup toolchain install stable && rustup default stable  (then re-run)."
            PREFLIGHT_FATAL=1
        fi
    elif [ "$DO_PROVISION" -eq 0 ]; then
        ui_warn "cargo is present but its toolchain is missing/broken (no default, or a corrupt stable)."
        ui_note "--no-provision: run  rustup toolchain install stable && rustup default stable  (then re-run)."
        ui_err "cargo cannot build the daemon without a working toolchain."
        PREFLIGHT_FATAL=1
    else
        # AUTO-REPAIR — handles BOTH a missing default toolchain AND a broken/partial
        # one ("Missing manifest"). Install + set default; VERIFY the way the build
        # does (rustc -vV); if STILL broken, force a CLEAN reinstall (remove the
        # corrupt toolchain, install fresh). This fixes the exact stage-5 failures
        # "could not choose a version of cargo" and "Missing manifest in toolchain".
        ui_info "Rust toolchain missing/broken — repairing (rustup toolchain install + default stable)."
        run_quiet "rustup: install + default stable" -- bash -c \
            "'$_rustup' toolchain install stable && '$_rustup' default stable" || true
        if ! "$_rustc" -vV >/dev/null 2>&1; then
            ui_warn "the stable toolchain is corrupt — reinstalling it cleanly."
            run_quiet "rustup: clean reinstall stable" -- bash -c \
                "'$_rustup' toolchain uninstall stable >/dev/null 2>&1; '$_rustup' toolchain install stable && '$_rustup' default stable" || true
        fi
        # Final verdict: BOTH cargo AND rustc -vV must run (rustc -vV is what the build invokes).
        if "$CARGO" --version >/dev/null 2>&1 && "$_rustc" -vV >/dev/null 2>&1; then
            ui_ok "Rust ready: $("$CARGO" --version)"
        else
            ui_err "Rust toolchain still broken after repair (rustc -vV fails)."
            ui_note "Fix manually:  rustup toolchain uninstall stable; rustup toolchain install stable; rustup default stable   (then re-run)."
            PREFLIGHT_FATAL=1
        fi
    fi
elif [ "$MODE" = "check" ]; then
    ui_warn "Rust toolchain (cargo) not found."
    if [ "$DO_PROVISION" -eq 1 ]; then
        plan "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path   # install Rust (no fatal in a normal install)"
    else
        ui_note "--no-provision: install with:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   (then re-run)"
        PREFLIGHT_FATAL=1
    fi
elif [ "$DO_PROVISION" -eq 0 ]; then
    # Opt-out: revert to the old detect + instruct + fatal behavior.
    ui_warn "Rust toolchain (cargo) not found."
    ui_note "--no-provision: install with:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   (then re-run)"
    ui_err "Rust is required to build the daemon + app crates."
    PREFLIGHT_FATAL=1
else
    # MODE=install + provisioning ON: install via rustup, no confirm() gate (a
    # build toolchain is a prerequisite, not DARWIN actuation). rustup's verbose
    # "Rust is installed now. Great! … add ~/.cargo/bin to PATH …" block is
    # captured to a temp log by run_quiet so only the clean spinner shows; the
    # log tail surfaces on failure.
    ui_info "Rust toolchain not found — installing Rust via rustup (no sudo, into ~/.rustup + ~/.cargo)."
    _rust_rc=0
    run_quiet "installing Rust (rustup)" -- bash -c \
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path" \
        || _rust_rc=$?
    CARGO="$HOME/.cargo/bin/cargo"
    # Propagate PATH for the rest of THIS process (build stages run cargo).
    case ":$PATH:" in *":$HOME/.cargo/bin:"*) : ;; *) PATH="$HOME/.cargo/bin:$PATH"; export PATH ;; esac
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
    # VERIFY-AND-GATE: re-detect cargo + run its version command; proceed only if
    # it verifies, else set PREFLIGHT_FATAL with a clear, actionable error.
    if [ "$_rust_rc" -eq 0 ] && [ -x "$CARGO" ] && "$CARGO" --version >/dev/null 2>&1; then
        ui_ok "Rust ready: $("$CARGO" --version)"
    else
        ui_err "Rust toolchain did not install/verify (no working cargo at $CARGO)."
        ui_note "Install manually:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   (then re-run)."
        PREFLIGHT_FATAL=1
    fi
fi

# --- Python 3.11 (auto-provisioned via Homebrew when missing) ---
# Idempotent: a present 3.11 is detected + reused, never reinstalled.
PY311="$(find_py311)"
if [ -n "$PY311" ]; then
    ui_ok "Python 3.11: $PY311 ($("$PY311" --version 2>&1))"
elif [ "$MODE" = "check" ]; then
    ui_warn "Python 3.11 not found (MLX has no wheels for 3.12+/3.14 — 3.11 is required)."
    if [ "$DO_PROVISION" -eq 1 ]; then
        ensure_homebrew   # plans the Homebrew bootstrap if brew is also absent
        plan "brew install python@3.11   # (no fatal in a normal install)"
    else
        ui_note "--no-provision: install with:  brew install python@3.11   (then re-run)"
        PREFLIGHT_FATAL=1
    fi
elif [ "$DO_PROVISION" -eq 0 ]; then
    ui_warn "Python 3.11 not found (MLX has no wheels for 3.12+/3.14 — 3.11 is required)."
    ui_note "--no-provision: install with:  brew install python@3.11   (then re-run)"
    PREFLIGHT_FATAL=1
else
    # MODE=install + provisioning ON: ensure Homebrew, then brew install.
    # ensure_homebrew already gates (no-tty / not-admin / unverifiable brew); if
    # it fails we stop here rather than build on a missing toolchain.
    ui_info "Python 3.11 not found — provisioning via Homebrew."
    if ensure_homebrew; then
        _py_rc=0
        run_quiet "installing python@3.11" -- brew install python@3.11 || _py_rc=$?
        # VERIFY-AND-GATE: re-detect a real 3.11 (find_py311 confirms version)
        # and only then proceed; otherwise set PREFLIGHT_FATAL.
        PY311="$(find_py311)"
        if [ "$_py_rc" -eq 0 ] && [ -n "$PY311" ]; then
            ui_ok "Python 3.11 ready: $PY311 ($("$PY311" --version 2>&1))"
        else
            ui_err "Python 3.11 did not install/verify (brew install python@3.11 produced no usable python3.11)."
            ui_note "Install manually:  brew install python@3.11   (then re-run)."
            PREFLIGHT_FATAL=1
        fi
    else
        ui_err "Could not make Homebrew available — cannot install python@3.11."
        ui_note "Resolve the Homebrew error above (or install Python 3.11 yourself), then re-run."
        PREFLIGHT_FATAL=1
    fi
fi

# --- Node + npm (HUD / Tauri) — auto-provisioned via Homebrew when missing ---
# Idempotent: present node+npm are detected + reused, never reinstalled.
if command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1; then
    ui_ok "Node $(node --version) / npm $(npm --version)"
elif [ "$MODE" = "check" ]; then
    ui_warn "Node + npm not found (needed for the HUD / Tauri build)."
    if [ "$DO_PROVISION" -eq 1 ]; then
        ensure_homebrew   # plans the Homebrew bootstrap if brew is also absent
        plan "brew install node   # (no fatal in a normal install)"
    else
        ui_note "--no-provision: install with:  brew install node   (then re-run)"
        PREFLIGHT_FATAL=1
    fi
elif [ "$DO_PROVISION" -eq 0 ]; then
    ui_warn "Node + npm not found (needed for the HUD / Tauri build)."
    ui_note "--no-provision: install with:  brew install node   (then re-run)"
    PREFLIGHT_FATAL=1
else
    # MODE=install + provisioning ON: ensure Homebrew, then brew install.
    # ensure_homebrew already gates (no-tty / not-admin / unverifiable brew); if
    # it fails we stop here rather than build on a missing toolchain.
    ui_info "Node + npm not found — provisioning via Homebrew."
    if ensure_homebrew; then
        _node_rc=0
        run_quiet "installing node" -- brew install node || _node_rc=$?
        # VERIFY-AND-GATE: re-detect node + npm and run their version commands;
        # only proceed if BOTH verify, else set PREFLIGHT_FATAL.
        if [ "$_node_rc" -eq 0 ] && command -v node >/dev/null 2>&1 && command -v npm >/dev/null 2>&1 \
           && node --version >/dev/null 2>&1 && npm --version >/dev/null 2>&1; then
            ui_ok "Node ready: $(node --version) / npm $(npm --version)"
        else
            ui_err "Node did not install/verify (brew install node produced no usable node + npm)."
            ui_note "Install manually:  brew install node   (then re-run)."
            PREFLIGHT_FATAL=1
        fi
    else
        ui_err "Could not make Homebrew available — cannot install node."
        ui_note "Resolve the Homebrew error above (or install Node yourself), then re-run."
        PREFLIGHT_FATAL=1
    fi
fi

# PREFLIGHT_FATAL is now set ONLY for genuinely-unrecoverable gaps: non-macOS,
# non-arm64, a --no-provision run with a real missing dep, or a provisioning step
# that actually FAILED (including the Xcode CLT install timing out / having no GUI
# session to complete its dialog). Missing-but-installable deps — CLT, Rust,
# Python 3.11, Node — are auto-provisioned above in a normal install and are NOT
# fatal unless their install genuinely fails.
if [ "$PREFLIGHT_FATAL" -ne 0 ]; then
    if [ "$MODE" = "check" ]; then
        ui_warn "Preflight reported gaps (above). Resolve them, then run a real install."
    else
        ui_err "Preflight failed — resolve the items above and re-run."
        exit 1
    fi
fi

# ============================================================================
# STAGE 2 — PLACE (copy the tree into the install home)
# ============================================================================
ui_stage 2 "$TOTAL_STAGES" "PLACE"

# Build the rsync/cp exclude args once.
RSYNC_EXCLUDES=()
for d in "${EXCLUDE_DIRS[@]}"; do RSYNC_EXCLUDES+=(--exclude "$d/"); done
RSYNC_EXCLUDES+=(--exclude ".DS_Store" --exclude "*.pyc" --exclude "state/env.sh")

ui_info "Excluding from the copy (built/fetched fresh in the home): ${EXCLUDE_DIRS[*]}"

if [ "$MODE" = "check" ]; then
    plan "mkdir -p \"$DARWIN_HOME\""
    plan "rsync -a ${RSYNC_EXCLUDES[*]} \"$SRC_ROOT/\" \"$DARWIN_HOME/\""
    ui_note "(source == install home would be a no-op copy; the build/model/launch"
    ui_note " stages would still run against the home)"
else
    mkdir -p "$DARWIN_HOME"
    if [ "$SRC_ROOT" = "$DARWIN_HOME" ]; then
        ui_ok "Source IS the install home — already in place, nothing to copy."
    elif command -v rsync >/dev/null 2>&1; then
        ui_spin "copying project tree -> install home" -- \
            rsync -a "${RSYNC_EXCLUDES[@]}" "$SRC_ROOT/" "$DARWIN_HOME/"
        ui_ok "Project tree placed at $DARWIN_HOME"
    else
        # Fallback: tar-pipe with excludes (rsync is standard on macOS, but be safe).
        TAR_EXCLUDES=()
        for d in "${EXCLUDE_DIRS[@]}"; do TAR_EXCLUDES+=(--exclude "./$d"); done
        TAR_EXCLUDES+=(--exclude "./.DS_Store")
        ui_spin "copying project tree (tar) -> install home" -- bash -c \
            "tar -C '$SRC_ROOT' ${TAR_EXCLUDES[*]} -cf - . | tar -C '$DARWIN_HOME' -xf -"
        ui_ok "Project tree placed at $DARWIN_HOME"
    fi
fi

export DARWIN_ROOT="$DARWIN_HOME"
ui_info "export DARWIN_ROOT=\"$DARWIN_ROOT\""

# From here on, paths are relative to the install home (so we build IN PLACE
# where the LaunchAgents expect the artifacts). In --check the home may not
# exist yet, so the build/model commands are only described.
HOME_PY_REQ="$DARWIN_HOME/inference/requirements.txt"
VENV="$DARWIN_HOME/.venv"
VENV_PY="$VENV/bin/python"
VENV_PIP="$VENV/bin/pip"

# Model cache — ONE path shared by install time and runtime. The runtime (the
# boot wrappers boot/run_daemon.sh + boot/run_inference.sh, and
# scripts/bringup.sh) sources state/env.sh, so stage 4 persists HF_HOME there;
# without that the inference server defaults to ~/.cache/huggingface and
# re-downloads every pre-downloaded model (or fails offline) — the exact
# "install/runtime HF_HOME split" scripts/doctor.sh flags. If state/env.sh
# ALREADY sets HF_HOME (a prior run, or the user pointing at a bigger disk),
# HONOR it for the install-time download too — the two must never disagree.
ENV_SH="$DARWIN_HOME/state/env.sh"
HF_HOME_DIR="$DARWIN_HOME/models"
if [ -f "$ENV_SH" ] && grep -qE '^[[:space:]]*export[[:space:]]+HF_HOME=' "$ENV_SH" 2>/dev/null; then
    # Evaluate env.sh EXACTLY as the boot wrappers do (DARWIN_ROOT set, file
    # sourced by bash) — but in a THROWAWAY subshell so none of its other
    # exports leak into this installer process.
    _env_hf="$(DARWIN_ROOT="$DARWIN_HOME" bash -c '. "$1" >/dev/null 2>&1; printf "%s" "${HF_HOME:-}"' _ "$ENV_SH" 2>/dev/null || true)"
    [ -n "$_env_hf" ] && HF_HOME_DIR="$_env_hf"
fi

# Persist HF_HOME into the runtime env (state/env.sh — the file every boot
# wrapper sources) so the daemon + inference server find the models this
# installer downloads. Idempotent: an existing HF_HOME line is left in place
# (it was honored above); the file is created chmod 600 when absent (the
# documented secrets convention). Written DARWIN_ROOT-relative so the tree
# stays relocatable, with the install home as the literal fallback.
persist_hf_home() {
    if [ -f "$ENV_SH" ] && grep -qE '^[[:space:]]*export[[:space:]]+HF_HOME=' "$ENV_SH" 2>/dev/null; then
        ui_ok "runtime model cache already pinned: HF_HOME -> $HF_HOME_DIR (state/env.sh — left in place)"
        return 0
    fi
    mkdir -p "$DARWIN_HOME/state"
    if [ ! -f "$ENV_SH" ]; then
        {
            printf '# state/env.sh — DARWIN runtime environment (gitignored; keep this chmod 600).\n'
            printf '# Sourced by the boot wrappers + scripts/bringup.sh. Put API keys here, e.g.\n'
            printf '#   export ANTHROPIC_API_KEY=...\n'
        } > "$ENV_SH"
        chmod 600 "$ENV_SH"
    fi
    printf 'export HF_HOME="${DARWIN_ROOT:-%s}/models"\n' "$DARWIN_HOME" >> "$ENV_SH"
    ui_ok "runtime model cache pinned: HF_HOME -> $HF_HOME_DIR (persisted in state/env.sh, which the boot wrappers source)"
}

# ============================================================================
# STAGE 3 — PYTHON ENV (venv + the FULL dependency set)
# ============================================================================
ui_stage 3 "$TOTAL_STAGES" "PYTHON ENV"

# The "every bit" extras the base requirements.txt leaves OPT-IN or transitive,
# pinned to real, current versions. These are added on top of requirements.txt
# so a full install pulls every model backend the OS can use.
EVERYBIT_EXTRAS=(
    "mlx-vlm>=0.1"        # on-device VLM (op=describe_image)
    "mflux>=0.4"          # on-device text->image (op=generate_image)
    "soundfile>=0.12"     # WAV/PCM IO for the voice pipeline
    "huggingface_hub>=0.24"  # model pre-download (hf CLI / snapshot_download API)
    "elevenlabs>=1.0"     # OPTIONAL cloud voice tier SDK (stays OFF until a key is set)
)

if [ "$MODE" = "check" ]; then
    plan "$PY311 -m venv \"$VENV\""
    plan "$VENV_PIP install --upgrade pip wheel"
    plan "$VENV_PIP install -r \"$HOME_PY_REQ\""
    plan "$VENV_PIP install ${EVERYBIT_EXTRAS[*]}"
    plan "$VENV_PY -m spacy download en_core_web_sm   # misaki G2P fallback"
    ui_note "base deps come from inference/requirements.txt (mlx, mlx-lm, mlx-whisper,"
    ui_note " mlx-audio, misaki, numpy, coremltools, spacy, ...); extras add the full set."
else
    if [ -z "${PY311:-}" ]; then ui_err "no python3.11 — cannot create the venv"; exit 1; fi
    if [ -x "$VENV_PY" ]; then
        ui_ok ".venv already present ($("$VENV_PY" --version 2>&1)) — reusing."
    else
        ui_spin "creating .venv (python3.11)" -- "$PY311" -m venv "$VENV"
        ui_ok ".venv created"
    fi
    ui_spin "upgrading pip + wheel" -- "$VENV_PIP" install --quiet --upgrade pip wheel
    if [ -f "$HOME_PY_REQ" ]; then
        ui_spin "pip install -r inference/requirements.txt (base deps)" -- \
            "$VENV_PIP" install --quiet -r "$HOME_PY_REQ"
    else
        ui_warn "inference/requirements.txt missing in the home — installing the pinned base set."
        ui_spin "pip install base deps (fallback pins)" -- "$VENV_PIP" install --quiet \
            "mlx>=0.20" "mlx-lm>=0.31.1" "mlx-whisper>=0.4" "mlx-audio>=0.4.4" \
            "misaki>=0.9.4" espeakng-loader num2words phonemizer-fork "spacy>=3.8" \
            "numpy>=1.26" "coremltools>=8.0"
    fi
    ui_spin "pip install the 'every bit' extras (VLM, image, soundfile, hf_hub, elevenlabs)" -- \
        "$VENV_PIP" install --quiet "${EVERYBIT_EXTRAS[@]}"
    ui_spin "spacy: download en_core_web_sm (G2P fallback)" -- \
        "$VENV_PY" -m spacy download en_core_web_sm
    ui_ok "Python environment ready — full dependency set installed."
fi

# ============================================================================
# STAGE 4 — MODELS (pre-download every model the OS uses)
# ============================================================================
ui_stage 4 "$TOTAL_STAGES" "MODELS"

LLM_ID="$(read_model_id DEFAULT_LLM "$FALLBACK_LLM")"
STT_ID="$(read_model_id DEFAULT_STT "$FALLBACK_STT")"
TTS_ID="$(read_model_id DEFAULT_TTS "$FALLBACK_TTS")"
VLM_ID="$(read_model_id DEFAULT_VLM "$FALLBACK_VLM")"
# Small DRAFT model for #37 speculative decoding ([inference].draft_model). The
# canonical id is server.py's DEFAULT_DRAFT; speculative stays honestly inert
# (normal gen, speculative=false reported) until this is present + mlx_lm has
# speculative support. Tiny relative to the others (~0.5 GB class).
DRAFT_ID="$(read_model_id DEFAULT_DRAFT "$FALLBACK_DRAFT")"
# The image model id (server.py DEFAULT_IMAGE_MODEL == "schnell") is an mflux
# ALIAS, not an HF repo id, so we cannot "hf download" the bare "schnell".
# "schnell" resolves to this FLUX.1-schnell repo inside mflux's own resolver; we
# name the repo here so the pre-download covers the same weights mflux fetches on
# first generate. This is the LARGEST download by far (multi-GB diffusion weights).
IMG_ID="black-forest-labs/FLUX.1-schnell"

MODELS=("$LLM_ID" "$STT_ID" "$TTS_ID" "$VLM_ID" "$DRAFT_ID" "$IMG_ID")

ui_info "HF_HOME -> $HF_HOME_DIR (ONE cache for installer + runtime, via state/env.sh; never the repo)"

# Pin the runtime cache FIRST — even with --no-models: a model fetched on
# first use must land in this same install-home cache (removed with the home
# by uninstall.sh), never in a stray ~/.cache/huggingface the runtime would
# otherwise default to.
if [ "$MODE" = "check" ]; then
    plan "append 'export HF_HOME=…/models' to \"$DARWIN_HOME/state/env.sh\"   # the boot wrappers source it, so the RUNTIME uses this same cache"
else
    persist_hf_home
fi
ui_info "LLM   : $LLM_ID"
ui_info "STT   : $STT_ID"
ui_info "TTS   : $TTS_ID"
ui_info "VLM   : $VLM_ID            (vision describe — multi-GB)"
ui_info "DRAFT : $DRAFT_ID  (speculative decoding — small)"
ui_info "IMAGE : $IMG_ID    (text->image — LARGE, multi-GB)"

if [ "$DO_MODELS" -eq 0 ]; then
    ui_warn "--no-models: skipping ALL model pre-downloads (LLM, STT, TTS, VLM, DRAFT,"
    ui_note "and the IMAGE model). Features that need a model fetch it on first use,"
    ui_note "gated on enough RAM; nothing is downloaded now."
elif [ "$MODE" = "check" ]; then
    plan "mkdir -p \"$HF_HOME_DIR\""
    for m in "${MODELS[@]}"; do
        plan "HF_HOME=\"$HF_HOME_DIR\" \"$VENV/bin/hf\" download \"$m\"   # (or the snapshot_download API if 'hf' is absent)"
    done
    ui_note "Total download is LARGE, multi-GB (the VLM and especially the IMAGE"
    ui_note "diffusion model dominate; the DRAFT model is small). Each download shows"
    ui_note "progress and is cache-first (resumable); already-downloaded models are"
    ui_note "skipped. Pass --no-models to skip ALL of these."
else
    mkdir -p "$HF_HOME_DIR"
    export HF_HOME="$HF_HOME_DIR"
    # `huggingface-cli` was REMOVED from current huggingface_hub — it now errors
    # with "deprecated and no longer works. Use `hf` instead." Resolve a working
    # downloader ONCE: prefer the new `hf` CLI (live progress); else fall back to
    # the STABLE Python snapshot_download API (immune to CLI renames). Both are
    # cache-first + resumable and honor HF_HOME, so re-runs are cheap.
    if [ -x "$VENV/bin/hf" ]; then
        DL=("$VENV/bin/hf" download)
    elif [ -x "$VENV_PY" ]; then
        DL=("$VENV_PY" -c 'import sys; from huggingface_hub import snapshot_download; snapshot_download(sys.argv[1])')
    else
        ui_err "no model downloader in the venv (need 'hf' or huggingface_hub — Stage 3 installs huggingface_hub)"; exit 1
    fi
    n=0; total="${#MODELS[@]}"; failed_models=()
    for m in "${MODELS[@]}"; do
        n=$((n + 1))
        ui_progress $(( (n - 1) * 100 / total )) "model $n/$total: $m"
        # BEST-EFFORT: a single model failing (a renamed/removed repo, an HF hiccup,
        # a rate-limit) must NOT abort the whole multi-GB install after the others
        # already downloaded. Warn + continue; the daemon fetches a missing model on
        # first use, or the dependent feature stays honestly inert (e.g. speculative
        # decoding without the DRAFT model). Downloads are cache-first, so re-running
        # the installer resumes cheaply.
        if ui_spin "download $m" -- "${DL[@]}" "$m"; then
            :
        else
            failed_models+=("$m")
            ui_warn "could not pre-download $m — DARWIN will fetch it on first use (or the dependent feature stays inert)."
        fi
    done
    ui_progress 100 "models cached"
    if [ "${#failed_models[@]}" -eq 0 ]; then
        ui_ok "Every model the OS uses is pre-downloaded into $HF_HOME_DIR"
    else
        ui_warn "Pre-downloaded all but ${#failed_models[@]} of $total model(s): ${failed_models[*]}"
        ui_note "The rest of DARWIN installs + runs normally; only each skipped model's"
        ui_note "feature stays inert until it's available. Re-run to retry — downloads resume from cache."
        # GATED-MODEL guidance: a 'requires approval' / 'Access denied' failure (e.g.
        # FLUX.1-schnell, the IMAGE model) is NOT a bug — that repo is gated on Hugging
        # Face and cannot download anonymously. Tell the user the exact fix.
        case " ${failed_models[*]} " in
            *FLUX*|*flux*)
                ui_note "Note: ${UI_BRIGHT}FLUX.1-schnell${UI_RESET} (image generation) is a ${UI_BRIGHT}GATED${UI_RESET} Hugging Face model."
                ui_note "To enable image generation: accept its licence at"
                ui_note "  ${UI_ICE}https://huggingface.co/black-forest-labs/FLUX.1-schnell${UI_RESET}"
                ui_note "then authenticate — ${UI_CYAN}\"$VENV/bin/hf\" auth login${UI_RESET}  (or  ${UI_CYAN}export HF_TOKEN=hf_...${UI_RESET}) — and re-run."
                ;;
        esac
    fi
fi

# ============================================================================
# STAGE 5 — BUILD FRESH (daemon + app crates + swift vision + HUD/Tauri .app)
# ============================================================================
ui_stage 5 "$TOTAL_STAGES" "BUILD FRESH"

# Cargo manifests to build --release, in dependency-friendly order.
CARGO_MANIFESTS=(
    "daemon/Cargo.toml"                 # darwind (the daemon)
    "apps/silicon-canvas/Cargo.toml"    # silicon-canvas
    "apps/mark-forge/Cargo.toml"        # mark-forge
    "apps/nexus/core/Cargo.toml"        # nexus_core
)

if [ "$MODE" = "check" ]; then
    for mf in "${CARGO_MANIFESTS[@]}"; do
        plan "$CARGO build --release --manifest-path \"$DARWIN_HOME/$mf\""
    done
    plan "swift build -c release --package-path \"$DARWIN_HOME/apps/vision\""
    plan "(cd \"$DARWIN_HOME/hud\" && npm ci && npm run tauri build)   # -> DARWIN.app"
    plan "ditto <built DARWIN.app> /Applications/DARWIN.app   # install the app (left alone if a SIGNED copy is already there)"
    ui_note "every artifact is built FRESH in the install home (never shipped prebuilt)."
else
    for mf in "${CARGO_MANIFESTS[@]}"; do
        crate="$(dirname "$mf")"
        if [ -f "$DARWIN_HOME/$mf" ]; then
            ui_spin "cargo build --release ($crate)" -- \
                "$CARGO" build --release --manifest-path "$DARWIN_HOME/$mf"
        else
            ui_warn "skip $mf (not present in the home)"
        fi
    done
    # Verify the daemon binary landed (the LaunchAgent crash-loops without it).
    if [ -x "$DARWIN_HOME/daemon/target/release/darwind" ]; then
        ui_ok "daemon binary: daemon/target/release/darwind (fresh)"
    else
        ui_err "darwind not produced by the daemon build"; exit 1
    fi
    # Verify the PDF memory-jail helper landed next to darwind. Built by the same
    # `cargo build --release` (it is a [[bin]] in the daemon crate). Its absence is
    # NON-fatal — darwind falls back to the weaker in-process PDF guard and logs a
    # warning — but a real install should ship it, so warn (don't exit) if missing.
    if [ -x "$DARWIN_HOME/daemon/target/release/pdfjail" ]; then
        ui_ok "PDF memory-jail: daemon/target/release/pdfjail (fresh)"
    else
        ui_warn "pdfjail helper not produced — PDF extraction will use the weaker in-process guard"
    fi
    # Swift vision app.
    if [ -f "$DARWIN_HOME/apps/vision/Package.swift" ]; then
        ui_spin "swift build -c release (apps/vision)" -- \
            swift build -c release --package-path "$DARWIN_HOME/apps/vision"
    fi
    # HUD / Tauri release .app.
    if [ -f "$DARWIN_HOME/hud/package.json" ]; then
        ui_spin "npm ci (HUD deps)" -- bash -c "cd '$DARWIN_HOME/hud' && NPM_CONFIG_UPDATE_NOTIFIER=false npm ci --no-fund --no-audit --loglevel=error"
        # Build DARWIN.app for THIS Mac. Two deliberate --config overrides:
        #   * targets=["app"]              — a local install only needs the .app
        #     (place_hud_app copies it into /Applications). The .dmg is a
        #     DISTRIBUTION artifact that CI builds + signs + notarizes; building
        #     it here is wasted work and extra output, so skip it.
        #   * createUpdaterArtifacts=false — signing the updater .tar.gz needs the
        #     PRIVATE key, which only CI holds; leaving it on makes `tauri build`
        #     die at the signing step *after* the bundle. The installed app still
        #     auto-updates by pulling the CI-signed artifacts from the release.
        # A locally-built app is AD-HOC code-signed (signingIdentity "-") and is
        # NOT Apple-notarized — notarization needs a paid Developer account and
        # only matters for the downloadable release. Tauri's "skipping app
        # notarization" warning is therefore EXPECTED + harmless here; we state it
        # calmly below and filter the raw warn line so a clean build reads clean.
        # `set -o pipefail` keeps a REAL build failure fatal through the sed filter
        # (sed always exits 0, so without pipefail a failed build would look ok).
        ui_note "local build: DARWIN.app is ad-hoc code-signed (not Apple-notarized) — notarization is only for the downloadable release; this build runs fine here"
        ui_spin "npm run tauri build (-> DARWIN.app)" -- bash -c "set -o pipefail; cd '$DARWIN_HOME/hud' && NPM_CONFIG_UPDATE_NOTIFIER=false NPM_CONFIG_FUND=false npm run tauri -- build --config '{\"bundle\":{\"targets\":[\"app\"],\"createUpdaterArtifacts\":false}}' 2>&1 | sed -e '/skipping app notarization/d'"
        APP_BUNDLE="$(find "$DARWIN_HOME/hud/src-tauri/target/release/bundle" -maxdepth 2 -name 'DARWIN.app' 2>/dev/null | head -1 || true)"
        if [ -n "$APP_BUNDLE" ]; then
            ui_ok "HUD bundled: $APP_BUNDLE"
            # INSTALL the app into /Applications (or ~/Applications) so it is
            # launchable — never clobbering an already-installed signed copy.
            place_hud_app "$APP_BUNDLE"
        else
            # A successful build with targets=["app"] ALWAYS emits
            # bundle/macos/DARWIN.app, so reaching here means the build silently
            # failed — be FATAL like the daemon-binary check above, never report a
            # successful install without the HUD app.
            ui_err "tauri build reported success but produced no DARWIN.app"; exit 1
        fi
    fi
    ui_ok "All release artifacts built fresh in the install home."
fi

# ============================================================================
# STAGE 6 — AUTOSTART (delegate to the existing scripts/install_boot.sh)
# ============================================================================
ui_stage 6 "$TOTAL_STAGES" "AUTOSTART"

BOOT_SCRIPT="$DARWIN_HOME/scripts/install_boot.sh"
[ -f "$BOOT_SCRIPT" ] || BOOT_SCRIPT="$SRC_ROOT/scripts/install_boot.sh"

if [ "$MODE" = "check" ]; then
    plan "\"$BOOT_SCRIPT\" --install   # renders + bootstraps com.darwin.inference + com.darwin.daemon"
    ui_note "AUTOSTART is delegated to the existing scripts/install_boot.sh (its"
    ui_note "launchctl logic is reused verbatim, not reimplemented)."
    if [ -x "$BOOT_SCRIPT" ]; then
        ui_info "Boot installer dry-run (read-only) preview:"
        "$BOOT_SCRIPT" 2>&1 | sed 's/^/      /' || true
    fi
else
    # THE DARWIN-ACTUATION CONSENT GATE (the confirm() that --yes assumes yes
    # to): loading these LaunchAgents does not just place files — RunAtLoad
    # STARTS the armed OS (daemon + inference) now and at every login. All the
    # stages above were build/placement; this is the step that turns it on, so
    # it asks first. Declining is NOT fatal: the install stays complete, the
    # stage-7 board reports AUTOSTART: MANUAL, and install_boot.sh enables it
    # any time later.
    if confirm "Load the LaunchAgents and start DARWIN now (autostart at every login)?"; then
        ui_info "Loading the 2 LaunchAgents via scripts/install_boot.sh --install ..."
        "$BOOT_SCRIPT" --install
        ui_ok "LaunchAgents installed + loaded (RunAtLoad starts them)."
    else
        ui_warn "Autostart declined — the LaunchAgents were NOT loaded (nothing was started)."
        ui_note "Enable later with:  \"$BOOT_SCRIPT\" --install   (or re-run the installer with -y)."
    fi
fi

# ============================================================================
# STAGE 7 — FINISH
# ============================================================================
ui_stage 7 "$TOTAL_STAGES" "FINISH"

# ----------------------------------------------------------------------------
# READ-ONLY OUTCOME DETECTION (stage 7 only — no behavior change to stages 1-6).
# Every probe below is a pure read (filesystem stat / grep / file count); none
# builds, downloads, writes, or starts anything. The derived flags feed the
# HONEST status board so each row reflects ONLY what actually happened. Verbs are
# precise: BUILT / READY / RESIDENT / DEFERRED / SKIPPED / ENABLED / MANUAL / OFF
# — never RUNNING / ONLINE / ACTIVE (the installer installs; it does not start +
# health-check the live system).
# ----------------------------------------------------------------------------

# Agent constellation size: count the [[agent]] tables in the placed roster
# (falls back to the source tree if the home copy is not readable). A real,
# truthful count — never fabricated; omitted (neutral "CONFIGURED") if unknown.
AGENTS_TOML="$DARWIN_HOME/config/agents.toml"
[ -f "$AGENTS_TOML" ] || AGENTS_TOML="$SRC_ROOT/config/agents.toml"
AGENT_COUNT=""
if [ -f "$AGENTS_TOML" ]; then
    AGENT_COUNT="$(grep -cE '^\[\[agent\]\]' "$AGENTS_TOML" 2>/dev/null || true)"
    case "$AGENT_COUNT" in ''|*[!0-9]*) AGENT_COUNT="" ;; esac
fi

# Consequential-actions + self-healing posture: both SHIP ON (full-power default).
# ARMED-but-gated is the shipped truth: the master switch is on, but every
# consequential action still requires a fresh per-action confirm + voice-id +
# policy + !lockdown (enforced at the chokepoints, not by this tag). Self-healing
# is ON but PROPOSE-ONLY and inert without ANTHROPIC_API_KEY. Confirm from the
# placed config when readable; if the line is unreadable, state the ON default —
# we report the shipped truth, and detect a user OVERRIDE to OFF if present.
DARWIN_TOML="$DARWIN_HOME/config/darwin.toml"
[ -f "$DARWIN_TOML" ] || DARWIN_TOML="$SRC_ROOT/config/darwin.toml"
# NOTE: keep every status tag PURE ASCII — the board aligns the "[ TAG ]" column
# by byte length (${#tag}); a multibyte glyph (em-dash) would mis-measure and
# nudge the closing bracket out of column. "ARMED (gated)" is ASCII + honest.
CONSEQ_TAG="ARMED (gated)"
SELFHEAL_TAG="ON (propose)"
if [ -f "$DARWIN_TOML" ]; then
    if grep -qE '^[[:space:]]*allow_consequential[[:space:]]*=[[:space:]]*false' "$DARWIN_TOML" 2>/dev/null; then
        # Honesty: if a user pre-flipped the master switch OFF in their config, say
        # so plainly rather than printing the ON default. (Default ships true.)
        CONSEQ_TAG="OFF (disarmed)"
    fi
    # Self-heal is under [self_heal].enabled; report a user OVERRIDE to OFF if present.
    if grep -qE '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*false[[:space:]]*#?.*' "$DARWIN_TOML" 2>/dev/null \
        && awk '/^\[self_heal\]/{f=1;next} /^\[/{f=0} f && /^[[:space:]]*enabled[[:space:]]*=[[:space:]]*false/{print;exit}' "$DARWIN_TOML" 2>/dev/null | grep -q .; then
        SELFHEAL_TAG="OFF"
    fi
fi

if [ "$MODE" = "check" ]; then
    ui_ok "Plan complete. This was a dry run — nothing was installed."
    ui_info "Run without --check to perform the install (add -y to auto-accept consent prompts)."

    # HONEST DRY-RUN STATUS BOARD: a clearly-labeled PLAN/PREVIEW. Every tag is a
    # planned/"would" state — nothing claims completion. Models reflect the
    # --no-models choice even in the plan (DEFERRED vs WILL FETCH).
    models_plan="WILL FETCH"
    [ "$DO_MODELS" -eq 0 ] && models_plan="DEFERRED"
    agents_plan="WILL CONFIGURE"
    [ -n "$AGENT_COUNT" ] && agents_plan="$AGENT_COUNT PLANNED"
    ui_status_board "BOOT MANIFEST  (dry-run preview)" \
        "CORE DAEMON (darwind)|WILL BUILD" \
        "INFERENCE ENGINE / MLX|WILL BUILD" \
        "ON-DEVICE MODELS|$models_plan" \
        "HUD INTERFACE|WILL INSTALL" \
        "AGENT CONSTELLATION|$agents_plan" \
        "AUTOSTART (LaunchAgents)|WILL LOAD" \
        "CONSEQUENTIAL ACTIONS|$CONSEQ_TAG" \
        "SELF-HEALING|$SELFHEAL_TAG"
    ui_note "(dry-run preview — no component above was built, fetched, or loaded)"

    # Preview the SAME next-steps directive panels a real finish would print (so
    # the dry run shows the full operator guidance), marked "(preview)" and sealed
    # below by the DRY RUN frame so nothing here reads as a completed install.
    print_next_steps_directives " (preview)"

    # Render the cinematic finale so a dry run previews the full "coming online"
    # flow. ui_online is PURE UI output (no writes); the bottom HUD frame seals
    # the dry-run preview with a DRY RUN status so nobody mistakes it for a real
    # install completing.
    ui_online "DRY RUN — NO CHANGES MADE"
    exit 0
fi

# --- REAL-INSTALL outcome flags (read-only stats of what stages 1-6 produced) ---

# CORE DAEMON: the release binary the LaunchAgent runs. Stage 5 hard-exits if it
# is absent, so reaching here with the file present means it BUILT.
DAEMON_TAG="SKIPPED"
[ -x "$DARWIN_HOME/daemon/target/release/darwind" ] && DAEMON_TAG="BUILT"

# INFERENCE ENGINE / MLX: the venv python with the full dep set (stage 3). READY
# means the interpreter is present in the home; it is installed, not "running".
ENGINE_TAG="DEFERRED"
[ -x "$DARWIN_HOME/.venv/bin/python" ] && ENGINE_TAG="READY"

# ON-DEVICE MODELS: DEFERRED when --no-models; otherwise count the model dirs that
# actually materialized under HF_HOME (models--<org>--<repo>). A real count — if
# the cache layout is unexpected we report RESIDENT without a fabricated number.
if [ "$DO_MODELS" -eq 0 ]; then
    MODELS_TAG="DEFERRED"
else
    _mcount="$(find "$HF_HOME_DIR" -maxdepth 3 -type d -name 'models--*' 2>/dev/null | wc -l | tr -d '[:space:]' || true)"
    case "$_mcount" in ''|*[!0-9]*) _mcount=0 ;; esac
    if [ "$_mcount" -gt 0 ]; then MODELS_TAG="$_mcount RESIDENT"; else MODELS_TAG="RESIDENT"; fi
fi

# HUD INTERFACE: the Tauri bundle. INSTALLED if place_hud_app put DARWIN.app into
# an Applications dir; BUILT if it landed in the bundle tree but wasn't placed;
# else SKIPPED (e.g. no hud/package.json or the bundle did not appear).
HUD_TAG="SKIPPED"
if find "$DARWIN_HOME/hud/src-tauri/target/release/bundle" -maxdepth 2 -name 'DARWIN.app' 2>/dev/null | grep -q . ; then
    HUD_TAG="BUILT"
fi
case "${INSTALLED_APP_PATH:-}" in
    /Applications/*|"$HOME"/Applications/*) HUD_TAG="INSTALLED" ;;
esac

# AGENT CONSTELLATION: READY (the roster config is placed + the daemon that reads
# it is BUILT). A truthful count when we have it; otherwise a neutral CONFIGURED.
if [ -n "$AGENT_COUNT" ]; then AGENTS_TAG="$AGENT_COUNT READY"; else AGENTS_TAG="CONFIGURED"; fi

# AUTOSTART: ENABLED only when BOTH rendered LaunchAgent plists are present in
# ~/Library/LaunchAgents (stage 6 renders + bootstraps both); otherwise MANUAL.
AGENT_DIR="$HOME/Library/LaunchAgents"
_loaded=0
[ -f "$AGENT_DIR/com.darwin.inference.plist" ] && _loaded=$(( _loaded + 1 ))
[ -f "$AGENT_DIR/com.darwin.daemon.plist" ]    && _loaded=$(( _loaded + 1 ))
if [ "$_loaded" -eq 2 ]; then AUTOSTART_TAG="ENABLED"; else AUTOSTART_TAG="MANUAL"; fi

# --- the cinematic finale + the HONEST status board ---
ui_online

# HONEST SYSTEM STATUS BOARD — precise verbs, real flags, the SAFE posture stated
# plainly. No RUNNING/ONLINE on a factual row; no fabricated counts.
ui_status_board "BOOT MANIFEST" \
    "CORE DAEMON (darwind)|$DAEMON_TAG" \
    "INFERENCE ENGINE / MLX|$ENGINE_TAG" \
    "ON-DEVICE MODELS|$MODELS_TAG" \
    "HUD INTERFACE|$HUD_TAG" \
    "AGENT CONSTELLATION|$AGENTS_TAG" \
    "AUTOSTART (LaunchAgents)|$AUTOSTART_TAG" \
    "CONSEQUENTIAL ACTIONS|$CONSEQ_TAG" \
    "SELF-HEALING|$SELFHEAL_TAG"

# FUTURISTIC NEXT-STEPS — five honest "directive" panels (shared renderer, so the
# real finish + the dry-run preview can never drift). Every fact is verbatim.
print_next_steps_directives
exit 0
