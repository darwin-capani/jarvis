# shellcheck shell=bash
# ui.sh — D.A.R.W.I.N. install terminal UI library (PURE BASH, no deps).
#
#   THE ARC REACTOR  ::  a cinematic, Iron-Man / Stark-HUD install experience.
# ----------------------------------------------------------------------------
# The whole install reads like an arc reactor powering up inside a heads-up
# display. A bracketed HUD frame carries "OPERATOR // <machine-name>" along the
# top edge; a targeting reticle frames a large glowing concentric-ring arc
# reactor as it spins up at the center (a sweeping highlight + a core that
# charges deep-blue -> cyan -> white); a brief "system diagnostic" reveals
# self-check readouts; the wordmark decodes from scrambled glyphs into
# D.A.R.W.I.N.; a short decorative boot readout settles its subsystems; one
# clean scan-sweep passes the banner; and a centered "Made by <author>" credit
# byline signs the build. Each install stage renders as a HUD readout panel with
# a targeting reticle; the finale runs a final reactor charge to 100%, lights
# the wordmark, and seals the display with a neutral ONLINE status — no butler
# address.
#
# Palette: ONE cohesive cool holographic HUD — a premium (not neon) arc-reactor
# cyan core, a pale ice highlight, a charged near-white core, deep-blue rings,
# and a steel-blue frame for every rule + structure element. Stark GOLD is a
# single deliberate warm signature reserved for two places ONLY: (a) the author
# NAME in the "MADE BY" byline and (b) one thin reactor rim (the d6 outer ring)
# — nothing else. The top-frame OPERATOR machine name is cool/ice, NOT gold.
# 24-bit truecolor
# with graceful fallback to 256-color, 16-color, and a clean no-color mode.
# Glyphs degrade to ASCII off a UTF-8 locale.
#
# API COMPATIBILITY (install.sh depends on these — signatures UNCHANGED):
#   ui_init
#   ui_fg <r> <g> <b>
#   darwin_banner
#   ui_hr
#   ui_stage   <n> <total> <label>
#   ui_ok / ui_warn / ui_err / ui_info / ui_note   <msg>
#   ui_progress <pct> [label]
#   ui_spin     <label> [--] <command...>
#   ui_online
#   (+ all UI_* palette/glyph globals the installer prints in its heredoc:
#    UI_BOLD UI_CYAN UI_RESET UI_G_INFO UI_BRIGHT UI_GREY ...)
#
# Cinematic ADDITIONS (purely additive — install.sh may call them, never must):
#   ui_reactor_powerup   — animated arc-reactor spin-up + system diagnostic
#   ui_greet [name]      — centered "Made by <author>" credit byline (name kept
#                          for API; PRINTS the byline, not a time-of-day greeting)
#   ui_frame_top [tag]   — top HUD frame bar with OPERATOR // <name>
#   ui_frame_bottom [s]  — bottom HUD frame bar that seals the display
#   ui_bg <r> <g> <b>    — truecolor background (silent below truecolor)
#
# Operator name: DARWIN_OPERATOR (defaults to the auto-detected machine name —
# scutil ComputerName -> hostname -s -> hostname -> $HOSTNAME -> "operator")
# tags the HUD frame; an explicit env value always wins. Author name:
# DARWIN_AUTHOR (default "Darwin Capani") signs the byline, rendered UPPERCASE.
# Both are sanitised of control chars so they can never inject escape sequences.
#
# Design rules (honoured):
#   - NO external binaries beyond coreutils + tput. No python/node/awk/figlet.
#   - Robust under `set -euo pipefail`: every function returns 0 and never trips
#     errexit. Runs under bash 3.2 (the macOS system bash).
#   - Degrades cleanly on a dumb/non-tty terminal and honours NO_COLOR.
#   - ALL motion auto-skips on non-tty / NO_COLOR / dumb-TERM (and via the
#     DARWIN_NO_ANIM escape hatch) so a piped/CI run stays clean and a real
#     install gains only a fraction of a second of wall-clock.
#   - All drawing goes to stdout; the caller decides where that points.
#
# This file is meant to be *sourced*; it defines functions and sets UI_* and
# DARWIN_* globals. It does not enable errexit itself (that is the installer's
# job).

# --- guard against double-sourcing ------------------------------------------
if [ -n "${__DARWIN_UI_SH_LOADED:-}" ]; then
    return 0 2>/dev/null || true
fi
__DARWIN_UI_SH_LOADED=1

# --- capability + state globals ----------------------------------------------
# UI_COLOR_MODE is one of: truecolor | 256 | 16 | none
UI_COLOR_MODE="none"
UI_WIDTH=80
UI_UTF8=0
UI_ANIM=0   # 1 only on a real interactive, colored, non-dumb tty

# Operator the HUD frame tags ("OPERATOR // ..."). Overridable: an explicit
# DARWIN_OPERATOR in the environment always wins. Otherwise it defaults to the
# auto-detected machine name (see _ui_detect_operator), e.g. macOS's friendly
# ComputerName ("Darwin's MacBook Pro"), which may carry a multibyte glyph (a
# curly apostrophe) — the top-frame border measures DISPLAY width, not bytes, so
# it stays aligned. Set lazily in ui_init so the detection runs set -e safe.
DARWIN_OPERATOR="${DARWIN_OPERATOR:-}"

# Author credited in the intro byline ("MADE BY <AUTHOR>"). Overridable; the
# name carries the single warm gold signature (the operator tag is cool/ice).
DARWIN_AUTHOR="${DARWIN_AUTHOR:-Darwin Capani}"

# A UTF-8 locale name we can lean on for character-accurate measuring even when
# the runtime locale is C/POSIX (where `wc -m` counts bytes). Resolved lazily.
_UI_UTF8_LOCALE=""

# Resolve the operator name (machine name) when DARWIN_OPERATOR is not already
# set in the environment. Tries, in order, the first that yields a non-empty
# value: macOS friendly ComputerName, short hostname, full hostname, $HOSTNAME,
# then the literal "operator". Every probe is guarded (command -v + 2>/dev/null
# + || true) so a missing tool can never trip `set -e`. Echoes the chosen name
# (raw — ui_init sanitises control chars afterwards). Never fails.
_ui_detect_operator() {
    local n=""
    if command -v scutil >/dev/null 2>&1; then
        n="$(scutil --get ComputerName 2>/dev/null || true)"
    fi
    if [ -z "$n" ] && command -v hostname >/dev/null 2>&1; then
        n="$(hostname -s 2>/dev/null || true)"
    fi
    if [ -z "$n" ] && command -v hostname >/dev/null 2>&1; then
        n="$(hostname 2>/dev/null || true)"
    fi
    [ -z "$n" ] && n="${HOSTNAME:-}"
    [ -z "$n" ] && n="operator"
    printf '%s' "$n"
    return 0
}

# Pick a UTF-8 locale usable for char-accurate measuring. Prefers the runtime's
# own locale when it is already UTF-8 (so we honour the user's exact charset),
# then falls back to en_US.UTF-8 / C.UTF-8 if `locale -a` advertises them. Sets
# _UI_UTF8_LOCALE to "" when none is available (the caller then degrades to a
# byte count). Idempotent + set -e safe. Never fails.
_ui_resolve_utf8_locale() {
    [ -n "${_UI_UTF8_LOCALE+x}" ] && [ -n "$_UI_UTF8_LOCALE" ] && return 0
    local cur="${LC_ALL:-${LC_CTYPE:-${LANG:-}}}"
    case "$cur" in
        *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) _UI_UTF8_LOCALE="$cur"; return 0 ;;
    esac
    if command -v locale >/dev/null 2>&1; then
        local avail; avail="$(locale -a 2>/dev/null || true)"
        case "$avail" in
            *en_US.UTF-8*|*en_US.utf8*) _UI_UTF8_LOCALE="en_US.UTF-8"; return 0 ;;
        esac
        case "$avail" in
            *C.UTF-8*|*C.utf8*) _UI_UTF8_LOCALE="C.UTF-8"; return 0 ;;
        esac
    fi
    _UI_UTF8_LOCALE=""
    return 0
}

# Display COLUMN width of a string, counting CHARACTERS (not bytes) so a name
# carrying a multibyte glyph (a 3-byte curly apostrophe ’, or spaces) measures
# its true on-screen width and the HUD frame border aligns exactly. Strategy:
# count chars via `wc -m` under a forced UTF-8 locale (so it is char-accurate
# even when the runtime locale is C, where wc -m would otherwise count bytes);
# if no UTF-8 locale exists or wc fails, degrade to the bash byte length ${#s}
# (still a sane, non-negative number — the border degrades gracefully, never
# breaks). Echoes a non-negative integer. set -e safe; never fails.
_ui_display_width() {
    local s="$1" w=""
    _ui_resolve_utf8_locale
    if [ -n "$_UI_UTF8_LOCALE" ]; then
        w="$(printf '%s' "$s" | LC_ALL="$_UI_UTF8_LOCALE" wc -m 2>/dev/null | tr -d '[:space:]' || true)"
    fi
    case "$w" in
        ''|*[!0-9]*) w="${#s}" ;;   # wc unavailable/odd output -> byte length
    esac
    printf '%s' "$w"
    return 0
}

# Detect once. Safe to call repeatedly. Honours an explicit override via
# DARWIN_UI_COLOR (truecolor|256|16|none) for testing/rendering.
ui_init() {
    # Width: prefer COLUMNS, then tput, then a sane default. Never fail.
    local cols=""
    if [ -n "${COLUMNS:-}" ]; then
        cols="$COLUMNS"
    elif command -v tput >/dev/null 2>&1; then
        cols="$(tput cols 2>/dev/null || true)"
    fi
    case "$cols" in
        ''|*[!0-9]*) UI_WIDTH=80 ;;
        *)           UI_WIDTH="$cols" ;;
    esac
    # Clamp width to a comfortable band so the HUD does not wrap or sprawl.
    [ "$UI_WIDTH" -lt 40 ] && UI_WIDTH=40
    [ "$UI_WIDTH" -gt 120 ] && UI_WIDTH=120

    # UTF-8 glyph support (for ✓ ⚠ ✗ ▸, box drawing, reactor, brackets).
    case "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" in
        *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) UI_UTF8=1 ;;
        *) UI_UTF8=0 ;;
    esac

    # Operator name: an explicit DARWIN_OPERATOR from the environment wins; if it
    # is empty, default to the auto-detected machine name. Detection is guarded so
    # `set -e` never aborts here.
    [ -z "$DARWIN_OPERATOR" ] && DARWIN_OPERATOR="$(_ui_detect_operator)"
    # Sanitise the operator name: strip CR/LF/ESC so it can never inject an
    # escape sequence or break the centered layout.
    DARWIN_OPERATOR="$(printf '%s' "$DARWIN_OPERATOR" | tr -d '\r\n\033')"
    [ -z "$DARWIN_OPERATOR" ] && DARWIN_OPERATOR="operator"

    # Sanitise the author name identically (it is rendered in the intro byline).
    DARWIN_AUTHOR="$(printf '%s' "$DARWIN_AUTHOR" | tr -d '\r\n\033')"
    [ -z "$DARWIN_AUTHOR" ] && DARWIN_AUTHOR="Darwin Capani"

    # Explicit override wins (lets the render harness force a mode).
    if [ -n "${DARWIN_UI_COLOR:-}" ]; then
        UI_COLOR_MODE="$DARWIN_UI_COLOR"
        _ui_finalize
        return 0
    fi

    # NO_COLOR (https://no-color.org/) or a non-tty stdout => no color.
    if [ -n "${NO_COLOR:-}" ] || [ ! -t 1 ]; then
        UI_COLOR_MODE="none"
        _ui_finalize
        return 0
    fi

    # A dumb/unset TERM cannot be trusted with escapes.
    case "${TERM:-}" in
        ''|dumb|unknown) UI_COLOR_MODE="none"; _ui_finalize; return 0 ;;
    esac

    # Truecolor: COLORTERM advertises it explicitly.
    case "${COLORTERM:-}" in
        *truecolor*|*24bit*) UI_COLOR_MODE="truecolor"; _ui_finalize; return 0 ;;
    esac

    # Otherwise fall back on the terminfo color count.
    local ncolors=0
    if command -v tput >/dev/null 2>&1; then
        ncolors="$(tput colors 2>/dev/null || echo 0)"
        case "$ncolors" in ''|*[!0-9]*) ncolors=0 ;; esac
    fi
    if [ "$ncolors" -ge 256 ]; then
        UI_COLOR_MODE="256"
    elif [ "$ncolors" -ge 8 ]; then
        UI_COLOR_MODE="16"
    else
        UI_COLOR_MODE="none"
    fi
    _ui_finalize
    return 0
}

# Settle derived state after a color mode is chosen: decide motion, build the
# palette. Motion is allowed ONLY on a real interactive colored terminal that is
# not dumb and not under NO_COLOR; a forced color mode (rendering) never animates
# unless stdout is also a tty; DARWIN_NO_ANIM forces static for testing/CI.
_ui_finalize() {
    UI_ANIM=0
    if [ "$UI_COLOR_MODE" != "none" ] && [ -t 1 ] && [ -z "${NO_COLOR:-}" ] \
       && [ -z "${DARWIN_NO_ANIM:-}" ]; then
        case "${TERM:-}" in
            ''|dumb|unknown) UI_ANIM=0 ;;
            *)               UI_ANIM=1 ;;
        esac
    fi
    # A forced color mode used purely for rendering (output piped) must not move.
    [ -n "${DARWIN_UI_COLOR:-}" ] && [ ! -t 1 ] && UI_ANIM=0
    _ui_define_palette

    # Cursor safety: when motion is on, guarantee the cursor is shown again on
    # ANY exit path — including Ctrl-C / kill mid-animation — so we never leave a
    # hidden cursor behind. Registered once, only when animating. (install.sh
    # sets no traps of its own, so this is additive, not a clobber.)
    if [ "$UI_ANIM" -eq 1 ] && [ -z "${__DARWIN_UI_TRAP:-}" ]; then
        __DARWIN_UI_TRAP=1
        trap '_ui_show_cursor' EXIT
        trap '_ui_show_cursor; trap - INT; kill -INT $$ 2>/dev/null || exit 130' INT
        trap '_ui_show_cursor; trap - TERM; kill -TERM $$ 2>/dev/null || exit 143' TERM
    fi
    return 0
}

# Restore the terminal cursor (idempotent, never fails). Used by exit traps.
_ui_show_cursor() {
    [ "$UI_COLOR_MODE" != "none" ] && printf '\033[?25h' 2>/dev/null
    return 0
}

# Emit an SGR escape for a 24-bit foreground color, degrading by UI_COLOR_MODE.
# Usage: ui_fg R G B   (each 0-255). Prints nothing in none-mode.
ui_fg() {
    case "$UI_COLOR_MODE" in
        truecolor) printf '\033[38;2;%d;%d;%dm' "$1" "$2" "$3" ;;
        256)       printf '\033[38;5;%dm' "$(_ui_rgb_to_256 "$1" "$2" "$3")" ;;
        16)        printf '\033[%dm' "$(_ui_rgb_to_16 "$1" "$2" "$3")" ;;
        *)         : ;;
    esac
    return 0
}

# Emit an SGR escape for a 24-bit BACKGROUND color (truecolor only; degrades to
# nothing otherwise so a fill never corrupts a 256/16 line). Usage: ui_bg R G B.
ui_bg() {
    case "$UI_COLOR_MODE" in
        truecolor) printf '\033[48;2;%d;%d;%dm' "$1" "$2" "$3" ;;
        *)         : ;;
    esac
    return 0
}

# Convert an RGB triple to the nearest xterm-256 cube/grey index.
_ui_rgb_to_256() {
    local r="$1" g="$2" b="$3"
    # Greyscale shortcut when r≈g≈b.
    local mx=$r mn=$r
    [ "$g" -gt "$mx" ] && mx=$g; [ "$b" -gt "$mx" ] && mx=$b
    [ "$g" -lt "$mn" ] && mn=$g; [ "$b" -lt "$mn" ] && mn=$b
    if [ $((mx - mn)) -le 12 ]; then
        local grey=$(( (r + g + b) / 3 ))
        if [ "$grey" -lt 8 ]; then printf '16'; return 0; fi
        if [ "$grey" -gt 248 ]; then printf '231'; return 0; fi
        printf '%d' $(( ((grey - 8) * 24 / 247) + 232 ))
        return 0
    fi
    # 6x6x6 color cube.
    local ri gi bi
    ri=$(( r * 5 / 255 )); gi=$(( g * 5 / 255 )); bi=$(( b * 5 / 255 ))
    printf '%d' $(( 16 + 36 * ri + 6 * gi + bi ))
    return 0
}

# Convert an RGB triple to a basic 16-color SGR code (30-37 / 90-97).
_ui_rgb_to_16() {
    local r="$1" g="$2" b="$3"
    local bright=0
    [ $(( (r + g + b) / 3 )) -ge 128 ] && bright=1
    local rb=0 gb=0 bb=0
    [ "$r" -ge 96 ] && rb=1
    [ "$g" -ge 96 ] && gb=1
    [ "$b" -ge 96 ] && bb=1
    local base=$(( rb + gb * 2 + bb * 4 ))   # 0..7 ANSI color order
    if [ "$bright" -eq 1 ]; then
        printf '%d' $(( 90 + base ))
    else
        printf '%d' $(( 30 + base ))
    fi
    return 0
}

# Palette + glyphs. Sets UI_RESET/UI_BOLD/UI_DIM and named color escapes.
# ONE cohesive cool HUD: a premium arc-reactor cyan core, an ice highlight, a
# charged near-white core, deep-blue + steel-blue rings/frame. GOLD is a single
# warm signature reserved for the author NAME in the byline (+ one thin reactor
# rim); the operator tag is cool/ice. Every member is defined for completeness
# so any consumer can use it.
# shellcheck disable=SC2034
_ui_define_palette() {
    if [ "$UI_COLOR_MODE" = "none" ]; then
        UI_RESET="" ; UI_BOLD="" ; UI_DIM="" ; UI_ITAL=""
        UI_CYAN="" ; UI_BRIGHT="" ; UI_GREEN="" ; UI_YELLOW="" ; UI_RED="" ; UI_BLUE="" ; UI_GREY=""
        UI_GOLD="" ; UI_AMBER="" ; UI_ICE="" ; UI_DEEP="" ; UI_STEEL="" ; UI_DIMCYAN=""
    else
        UI_RESET=$'\033[0m'
        UI_BOLD=$'\033[1m'
        UI_DIM=$'\033[2m'
        UI_ITAL=$'\033[3m'
        # --- the cool family: ALL structure ---------------------------------
        UI_CYAN="$(ui_fg 40 175 220)"    # arc-reactor cyan core — premium, not neon
        UI_BRIGHT="$(ui_fg 232 248 255)" # charged near-white core
        UI_ICE="$(ui_fg 158 222 248)"    # pale-ice halo / highlight
        UI_DIMCYAN="$(ui_fg 28 118 150)" # quieted cyan for pips / rim accents
        UI_DEEP="$(ui_fg 30 96 150)"     # deep-blue rings
        UI_STEEL="$(ui_fg 104 140 176)"  # HUD frame + rules (steel-blue)
        UI_BLUE="$(ui_fg 90 150 255)"
        UI_GREY="$(ui_fg 122 142 162)"   # meta / sub-notes
        # --- semantics (unchanged in role) ----------------------------------
        UI_GREEN="$(ui_fg 80 230 160)"   # ok / confirmed
        UI_YELLOW="$(ui_fg 245 200 70)"  # warn
        UI_RED="$(ui_fg 255 90 90)"      # err / fault
        # --- the SINGLE warm signature: byline author name (+ d6 rim) only --
        UI_GOLD="$(ui_fg 224 176 92)"    # elegant Stark gold (not yellow)
        UI_AMBER="$(ui_fg 224 176 92)"   # kept == gold so any stray use stays cohesive
    fi

    # Glyphs degrade to ASCII when UTF-8 is unavailable.
    if [ "$UI_UTF8" -eq 1 ]; then
        UI_G_OK="✓" ; UI_G_WARN="⚠" ; UI_G_ERR="✗" ; UI_G_INFO="▸"
        UI_G_DOT="•" ; UI_HR_CH="─"
        UI_SPIN='⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏'
        UI_BAR_FULL="█" ; UI_BAR_HALF="▌" ; UI_BAR_EMPTY="·"
        # HUD frame pieces + cinematic glyphs.
        UI_TL="╭" ; UI_TR="╮" ; UI_BL="╰" ; UI_BR="╯"
        UI_DIAMOND="◈" ; UI_SWEEP="›" ; UI_REACT_O="◉"
        UI_REACTOR_SPIN='◜ ◝ ◞ ◟'
    else
        UI_G_OK="+" ; UI_G_WARN="!" ; UI_G_ERR="x" ; UI_G_INFO=">"
        UI_G_DOT="*" ; UI_HR_CH="-"
        UI_SPIN='| / - \'
        UI_BAR_FULL="#" ; UI_BAR_HALF="=" ; UI_BAR_EMPTY="."
        UI_TL="+" ; UI_TR="+" ; UI_BL="+" ; UI_BR="+"
        UI_DIAMOND="o" ; UI_SWEEP=">" ; UI_REACT_O="O"
        UI_REACTOR_SPIN='| / - \'
    fi
    return 0
}

# Ensure a palette exists even if the caller forgot ui_init.
_ui_ensure() {
    [ -n "${UI_RESET+x}" ] || ui_init
    return 0
}

# --- internal helpers --------------------------------------------------------

# Repeat a string $1 exactly $2 times -> stdout (no color). Pure bash, no seq.
_ui_repeat() {
    local ch="$1" n="$2" out="" i=0
    [ "$n" -lt 0 ] && n=0
    while [ "$i" -lt "$n" ]; do out="${out}${ch}"; i=$((i + 1)); done
    printf '%s' "$out"
    return 0
}

# Left pad (spaces) needed to center a string of visible length $1 in UI_WIDTH.
_ui_pad() {
    local len="$1"
    local pad=$(( (UI_WIDTH - len) / 2 ))
    [ "$pad" -lt 0 ] && pad=0
    _ui_repeat ' ' "$pad"
    return 0
}

# Sleep a fractional second only when animating; never on a non-tty. Tolerates a
# coreutils sleep without sub-second support by falling back to a no-op.
_ui_nap() {
    [ "$UI_ANIM" -eq 1 ] || return 0
    sleep "$1" 2>/dev/null || true
    return 0
}

# Split a space-separated frame string into an array with globbing disabled, so
# ASCII ramps containing * # \ never pathname-expand. Restores caller noglob.
# Usage: _ui_split DESTARRAY "$SPACE_SEPARATED"
_ui_split() {
    local __name="$1" __src="$2" __had_f=0
    case "$-" in *f*) __had_f=1 ;; esac
    set -f
    # shellcheck disable=SC2206,SC2229
    eval "$__name=(\$__src)"
    [ "$__had_f" -eq 1 ] || set +f
    return 0
}

# Center one line at UI_WIDTH with an optional color prefix (reset appended).
_ui_centerline() {
    local text="$1" color="${2:-}"
    local sp; sp="$(_ui_pad "${#text}")"
    if [ "$UI_COLOR_MODE" = "none" ] || [ -z "$color" ]; then
        printf '%s%s\n' "$sp" "$text"
    else
        printf '%s%s%s%s\n' "$sp" "$color" "$text" "$UI_RESET"
    fi
    return 0
}

# Reveal a centered wordmark one glyph at a time (a "lights coming on" effect).
# On a tty each character types in with a tiny delay; off-tty / NO_COLOR / no
# color it prints instantly and centered (identical final artifact). Cursor is
# hidden during the reveal and ALWAYS restored. Usage: _ui_wordmark_reveal TEXT COLOR.
_ui_wordmark_reveal() {
    local text="$1" color="${2:-}"
    local sp; sp="$(_ui_pad "${#text}")"
    if [ "$UI_COLOR_MODE" = "none" ] || [ -z "$color" ]; then
        printf '%s%s\n' "$sp" "$text"
        return 0
    fi
    if [ "$UI_ANIM" -eq 0 ]; then
        printf '%s%s%s%s\n' "$sp" "$color" "$text" "$UI_RESET"
        return 0
    fi
    printf '\033[?25l'
    printf '%s%s' "$sp" "$color"
    local i=0 ch
    while [ "$i" -lt "${#text}" ]; do
        ch="${text:$i:1}"
        printf '%s' "$ch"
        i=$(( i + 1 ))
        # A beat only on the visible letters (skip the spaces/dots for snappiness).
        case "$ch" in [A-Za-z]) _ui_nap 0.05 ;; *) : ;; esac
    done
    printf '%s\n' "$UI_RESET"
    printf '\033[?25h'
    return 0
}

# DECODE-IN wordmark (futuristic flourish A): the final TEXT resolves from a few
# frames of SCRAMBLED glyphs (random from a fixed charset) that lock in
# left-to-right until the whole wordmark is settled. Redrawn in place on one
# line (\r). tty-only motion: off-tty / NO_COLOR / no-color it prints the final
# text instantly and centered (identical final artifact to _ui_wordmark_reveal).
# Cursor hidden during motion and ALWAYS restored. Usage: _ui_wordmark_decode TEXT COLOR.
_UI_DECODE_CHARSET='ABCDEFGHJKLMNPQRSTUVWXYZ0123456789#%*<>/\=+:.'
_ui_wordmark_decode() {
    local text="$1" color="${2:-}"
    local sp; sp="$(_ui_pad "${#text}")"
    if [ "$UI_COLOR_MODE" = "none" ] || [ -z "$color" ]; then
        printf '%s%s\n' "$sp" "$text"
        return 0
    fi
    if [ "$UI_ANIM" -eq 0 ]; then
        printf '%s%s%s%s\n' "$sp" "$color" "$text" "$UI_RESET"
        return 0
    fi
    local len="${#text}" cs="$_UI_DECODE_CHARSET" cslen
    cslen="${#cs}"
    printf '\033[?25l'
    # Progressive left-to-right lock-in: at each pass `locked` more glyphs are
    # final; the rest scramble. ~len/2 + 1 passes keeps it brief (~0.4-0.7s).
    local locked=0 step=2
    while [ "$locked" -lt "$len" ]; do
        local out="" i=0 ch
        while [ "$i" -lt "$len" ]; do
            ch="${text:$i:1}"
            if [ "$i" -lt "$locked" ] || [ "$ch" = " " ]; then
                out="${out}${ch}"                       # settled (or a space gap)
            else
                # Scrambled glyph from the fixed charset (no external rng beyond $RANDOM).
                local pick=$(( RANDOM % cslen ))
                out="${out}${cs:$pick:1}"
            fi
            i=$(( i + 1 ))
        done
        printf '\r%s%s%s%s' "$sp" "${UI_DIM}${UI_CYAN}" "$out" "$UI_RESET"
        _ui_nap 0.045
        locked=$(( locked + step ))
    done
    # Final settled wordmark in the real color.
    printf '\r%s%s%s%s\n' "$sp" "$color" "$text" "$UI_RESET"
    printf '\033[?25h'
    return 0
}

# --- HUD frame bars ----------------------------------------------------------

# Top HUD bar:   ╭── D.A.R.W.I.N. // ARC OS ─────── [ OPERATOR // darwin-capani ] ╮
# Carries the operator name + a system tag inside the frame's top edge.
ui_frame_top() {
    _ui_ensure
    local tag="${1:-D.A.R.W.I.N. // ARC OS}"
    local op="OPERATOR // ${DARWIN_OPERATOR}"
    local left="${UI_TL}${UI_HR_CH}${UI_HR_CH} "
    local opcell="[ ${op} ]"
    # DISPLAY-WIDTH-safe measure of the operator cell: the machine name may carry
    # a multibyte glyph (a 3-byte curly apostrophe ’) + spaces, so ${#opcell}
    # would over-count BYTES and run the border short. Count CHARACTERS instead
    # (UTF-8 aware; degrades to ${#opcell} when no UTF-8 locale is available, so
    # the border is always sane, never negative). All other cells are ASCII, so
    # ${#...} is exact for them.
    local opcell_w; opcell_w="$(_ui_display_width "$opcell")"
    # Visible: left + tag + ' ' + rule + ' ' + opcell + ' ' + corner
    local visible=$(( ${#left} + ${#tag} + 1 + 1 + opcell_w + 1 + 1 ))
    local fill=$(( UI_WIDTH - visible ))
    [ "$fill" -lt 1 ] && fill=1
    local rule; rule="$(_ui_repeat "$UI_HR_CH" "$fill")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s %s %s %s\n' "$left" "$tag" "$rule" "$opcell" "$UI_TR"
    else
        printf '%s%s%s%s%s%s %s%s%s %s%s%s%s%s%s%s %s%s%s\n' \
            "${UI_STEEL}" "$left" "$UI_RESET" \
            "${UI_BOLD}${UI_CYAN}" "$tag" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$rule" "$UI_RESET" \
            "${UI_STEEL}" "[ " "${UI_BOLD}${UI_ICE}" "$op" "${UI_RESET}${UI_STEEL}" " ]" "$UI_RESET" \
            "${UI_STEEL}" "$UI_TR" "$UI_RESET"
    fi
    return 0
}

# Bottom HUD bar:  ╰──[ ALL SYSTEMS NOMINAL ]──────────────────────────────╯
ui_frame_bottom() {
    _ui_ensure
    local status="${1:-ALL SYSTEMS NOMINAL}"
    local left="${UI_BL}${UI_HR_CH}${UI_HR_CH}"
    local cell="[ ${status} ]"
    # Visible: left + cell + ' ' + rule + corner
    local visible=$(( ${#left} + ${#cell} + 1 + 1 ))
    local fill=$(( UI_WIDTH - visible ))
    [ "$fill" -lt 1 ] && fill=1
    local rule; rule="$(_ui_repeat "$UI_HR_CH" "$fill")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s %s%s\n' "$left" "$cell" "$rule" "$UI_BR"
    else
        printf '%s%s%s%s[ %s%s%s%s ]%s %s%s%s%s%s%s\n' \
            "${UI_STEEL}" "$left" "$UI_RESET" \
            "${UI_STEEL}" "${UI_BOLD}${UI_GREEN}" "$status" "$UI_RESET" "${UI_STEEL}" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$rule" "$UI_RESET" \
            "${UI_STEEL}" "$UI_BR" "$UI_RESET"
    fi
    return 0
}

# --- primitives --------------------------------------------------------------

# A horizontal rule spanning UI_WIDTH (dim cyan) with steel-blue endcaps.
ui_hr() {
    _ui_ensure
    local n="$UI_WIDTH"
    [ "$n" -ge 2 ] && n=$((n - 2))
    local line; line="$(_ui_repeat "$UI_HR_CH" "$n")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s%s\n' "$UI_HR_CH" "$line" "$UI_HR_CH"
    else
        printf '%s%s%s%s%s%s%s\n' \
            "${UI_STEEL}" "$UI_HR_CH" \
            "${UI_DIM}${UI_CYAN}" "$line" \
            "${UI_RESET}${UI_STEEL}" "$UI_HR_CH" "$UI_RESET"
    fi
    return 0
}

# Stage header rendered as a HUD readout panel:
#   ▸▸ [ 1/7 ] PREFLIGHT  ›››  ─────────────────────────────────  ◈
# Same call shape as the original: ui_stage <n> <total> <label>.
ui_stage() {
    _ui_ensure
    local n="$1" total="$2" label="$3"
    local tag="[ ${n}/${total} ]"
    local lead="${UI_HR_CH}${UI_HR_CH} "
    local reticle="${UI_SWEEP}${UI_SWEEP}${UI_SWEEP}"
    # Visible: lead + tag + ' ' + label + ' ' + reticle + ' ' + rule + ' ' + diamond
    local visible=$(( ${#lead} + ${#tag} + 1 + ${#label} + 1 + ${#reticle} + 1 + 1 + 1 ))
    local fill=$(( UI_WIDTH - visible ))
    [ "$fill" -lt 1 ] && fill=1
    local rule; rule="$(_ui_repeat "$UI_HR_CH" "$fill")"

    # Optional reveal sweep before the panel resolves (animated tty only).
    if [ "$UI_ANIM" -eq 1 ]; then
        local sw=0 swn=$(( UI_WIDTH - 4 ))
        [ "$swn" -gt 48 ] && swn=48
        printf '\n'
        while [ "$sw" -lt "$swn" ]; do
            printf '\r %s%s%s%s%s' \
                "${UI_DIM}${UI_CYAN}" "$(_ui_repeat "$UI_HR_CH" "$sw")" \
                "${UI_BOLD}${UI_CYAN}" "$UI_SWEEP" "$UI_RESET"
            sw=$(( sw + 6 ))
            _ui_nap 0.012
        done
        printf '\r\033[K'
    else
        printf '\n'
    fi

    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s %s %s %s %s\n' "$lead" "$tag" "$label" "$reticle" "$rule" "$UI_DIAMOND"
    else
        printf '%s%s%s %s%s%s %s%s%s %s%s%s %s%s%s %s%s%s\n' \
            "${UI_STEEL}" "$lead" "$UI_RESET" \
            "${UI_BOLD}${UI_CYAN}" "$tag" "$UI_RESET" \
            "${UI_BOLD}${UI_BRIGHT}" "$label" "$UI_RESET" \
            "${UI_CYAN}" "$reticle" "$UI_RESET" \
            "${UI_DIM}${UI_CYAN}" "$rule" "$UI_RESET" \
            "${UI_BOLD}${UI_CYAN}" "$UI_DIAMOND" "$UI_RESET"
    fi
    return 0
}

# A tiny per-line beat so status / plan lines reveal as a "scan" instead of a
# dump. A no-op off-tty. Kept short so a whole --check stays a few seconds and a
# real install's many lines never drag (heavy stages use ui_spin / real work).
_ui_scanbeat() { [ "$UI_ANIM" -eq 1 ] && _ui_nap 0.018; return 0; }

# Status lines. Each takes a message; all return 0. The reveal beat lands BEFORE
# the line so successive checks scan in one at a time on a tty.
ui_ok()   { _ui_ensure; _ui_scanbeat; printf '  %s%s%s %s\n' "${UI_BOLD}${UI_GREEN}"  "$UI_G_OK"   "$UI_RESET" "$1"; return 0; }
ui_warn() { _ui_ensure; printf '  %s%s%s %s%s%s\n' "${UI_BOLD}${UI_YELLOW}" "$UI_G_WARN" "$UI_RESET" "$UI_YELLOW" "$1" "$UI_RESET"; return 0; }
ui_err()  { _ui_ensure; printf '  %s%s%s %s%s%s\n' "${UI_BOLD}${UI_RED}"    "$UI_G_ERR"  "$UI_RESET" "$UI_RED" "$1" "$UI_RESET"; return 0; }
ui_info() { _ui_ensure; _ui_scanbeat; printf '  %s%s%s %s%s%s\n' "${UI_CYAN}" "$UI_G_INFO" "$UI_RESET" "$UI_GREY" "$1" "$UI_RESET"; return 0; }

# A plain dim sub-note, indented under a status line.
ui_note() { _ui_ensure; _ui_scanbeat; printf '    %s%s %s%s\n' "$UI_DIM" "$UI_G_DOT" "$1" "$UI_RESET"; return 0; }

# A smooth "reactor charge" progress bar:
#   [██████████····················]  42%  label
# Args: pct (0-100), label (optional). Truecolor draws a deep-blue -> cyan ->
# white gradient (a fuller bar reads as a hotter core) with an ice-white shimmer
# cell tracking the leading edge.
ui_progress() {
    _ui_ensure
    local pct="$1" label="${2:-}"
    case "$pct" in ''|*[!0-9]*) pct=0 ;; esac
    [ "$pct" -gt 100 ] && pct=100
    [ "$pct" -lt 0 ] && pct=0

    local barwidth=30
    local filled=$(( pct * barwidth / 100 ))
    local empty=$(( barwidth - filled ))

    local bar="" i=0
    if [ "$UI_COLOR_MODE" = "truecolor" ]; then
        # "Reactor charge": deep blue (0,110,150) -> cyan (0,200,255) -> white
        # (235,250,255). The leading edge gets an ice-white shimmer cell.
        local lead=$(( filled - 1 ))
        while [ "$i" -lt "$filled" ]; do
            if [ "$i" -eq "$lead" ]; then
                bar="${bar}${UI_BRIGHT}${UI_BAR_FULL}"
            else
                local t=$(( barwidth > 1 ? i * 255 / (barwidth - 1) : 255 ))
                local r g b u
                if [ "$t" -lt 128 ]; then
                    u=$(( t * 255 / 127 ))
                    r=0
                    g=$(( 110 + (200 - 110) * u / 255 ))
                    b=$(( 150 + (255 - 150) * u / 255 ))
                else
                    u=$(( (t - 128) * 255 / 127 ))
                    r=$(( (235) * u / 255 ))
                    g=$(( 200 + (250 - 200) * u / 255 ))
                    b=255
                fi
                bar="${bar}$(ui_fg "$r" "$g" "$b")${UI_BAR_FULL}"
            fi
            i=$((i + 1))
        done
        bar="${bar}${UI_RESET}"
    else
        i=0
        while [ "$i" -lt "$filled" ]; do bar="${bar}${UI_BAR_FULL}"; i=$((i + 1)); done
        bar="${UI_CYAN}${bar}${UI_RESET}"
    fi
    local empties; empties="$(_ui_repeat "$UI_BAR_EMPTY" "$empty")"

    # \r so repeated calls overwrite the same line on a tty; trailing newline at
    # 100% (final state persists) and on any non-tty (each step on its own line
    # in a captured log rather than one mangled line).
    local eol="\r"
    [ "$pct" -ge 100 ] && eol="\n"
    [ ! -t 1 ] && eol="\n"
    printf '  %s[%s%s%s]%s %s%3d%%%s %s%s%s%b' \
        "$UI_DIM" "$UI_RESET" "$bar" "${UI_DIM}${UI_GREY}${empties}" "$UI_RESET" \
        "${UI_BOLD}${UI_BRIGHT}" "$pct" "$UI_RESET" \
        "$UI_GREY" "$label" "$UI_RESET" "$eol"
    return 0
}

# Spinner for a long opaque step. Runs the given command in the background and
# animates a reactor-style spinner (pulsing cyan<->ice to read as a charging
# core) until it exits, then reports ok/err. Falls back to a plain ok/err (no
# animation) when motion is off so logs stay clean.
#   ui_spin "compiling daemon" -- some_command --with args
ui_spin() {
    _ui_ensure
    local label="$1"; shift
    [ "${1:-}" = "--" ] && shift
    if [ "$#" -eq 0 ]; then ui_warn "ui_spin: no command given"; return 0; fi

    if [ "$UI_ANIM" -eq 0 ]; then
        printf '  %s%s%s %s ...\n' "$UI_CYAN" "$UI_G_INFO" "$UI_RESET" "$label"
        if "$@"; then ui_ok "$label"; return 0; else ui_err "$label (failed)"; return 1; fi
    fi

    "$@" &
    local pid=$!
    local -a frames
    _ui_split frames "$UI_SPIN"
    local nf="${#frames[@]}"
    local k=0
    while kill -0 "$pid" 2>/dev/null; do
        local f="${frames[$(( k % nf ))]}"
        local col="${UI_BOLD}${UI_CYAN}"
        [ $(( (k / nf) % 2 )) -eq 1 ] && col="${UI_BOLD}${UI_ICE}"
        printf '\r  %s%s%s %s%s%s' "$col" "$f" "$UI_RESET" "$UI_GREY" "$label" "$UI_RESET"
        k=$((k + 1))
        sleep 0.08 2>/dev/null || true
    done
    if wait "$pid"; then
        printf '\r\033[K'; ui_ok "$label"; return 0
    else
        printf '\r\033[K'; ui_err "$label (failed)"; return 1
    fi
}

# --- arc-reactor banner ------------------------------------------------------
# The centerpiece: a glowing concentric-ring arc reactor with a cyan->white
# core, framed by a HUD top bar carrying the operator name, plus the D.A.R.W.I.N.
# wordmark. On an interactive tty the reactor is drawn IN PLACE and visibly
# CHARGES (deep-blue -> cyan -> ice -> white over several frames, redrawing over
# itself), ending exactly on the settled reactor; the wordmark, expansion line,
# subtitle and greeting then reveal in sequence. On non-tty/NO_COLOR it renders
# the static structure only (no escapes, no motion).

# The reactor art — a COILED concentric-ring ARC REACTOR (reads as a clean
# CIRCLE, not a face). 13 rows, each EXACTLY 25 visible chars wide so the
# centering math (which counts ${#line}) matches what the terminal renders.
#
# Why it reads circular + how it stays symmetric:
#   - The block is 25 wide x 13 tall (~1.9 : 1). Terminal cells are ~1 : 2
#     (twice as tall as wide), so a ~1.9 : 1 character block renders close to
#     SQUARE on screen -> a round silhouette, not a squashed oval.
#   - PURE ASCII (no multibyte glyphs) so ${#line} == the on-screen column
#     width EXACTLY; UTF-8 ring glyphs would make ${#line} count bytes (>1 per
#     glyph) and mis-center + break the radial coloring, so we deliberately stay
#     ASCII (the rounded rim still reads as a circle).
#   - Each row is internally LEFT-RIGHT mirror-symmetric (a '/' on the left
#     answers a '\' on the right, '.:' answers ':.', etc.) and the block is a
#     fixed 25 wide, so centering the block (LEFT pad == RIGHT pad) keeps the
#     reactor on the screen's center axis and the rim reads as a true circle.
#   - Structure: a rounded OUTER ring '.:=========:.' (its curved corners read as
#     a circle), a concentric INNER coil ring '.:=====:.' , short RADIAL spokes
#     ('+'/':' on the axes, '--' arms bridging the rings at symmetric angles),
#     and a single '@' CORE at dead center. No parentheses, no face — concentric
#     rings coiled around a bright core.
#
# Single-quoted: every char is literal. Each of the 13 rows below is the file
# art RIGHT-PADDED with spaces to EXACTLY 25 chars, forming a 25-wide x 13-row
# centered block. A file-scope array so the static draw and the in-place charge
# share one source of truth (the settled frame == the NO_COLOR artifact).
# shellcheck disable=SC2034
_UI_REACTOR_ART=(
'      .:=========:.      '
'   .:'\''      |      '\'':.   '
'  .'\''    .:=====:.    '\''.  '
' /     :    |    :     \ '
'|     :  .--+--.  :     |'
'|    :  :   |   :  :    |'
'|----+--+   @   +--+----|'
'|    :  :   |   :  :    |'
'|     :  '\''--+--'\''  :     |'
' \     :    |    :     / '
'  '\''.    '\'':=====:'\''    .'\''  '
'   '\'':.      |      .:'\''   '
'      '\'':=========:'\''      '
)

# Pick the foreground escape for one reactor row at charge LEVEL (0..4), given
# its radial distance D = |row - 6| from the core row (the middle row of the
# 13-row coil). Returns "<emph><color>" on stdout so the caller can wrap the row.
# Level rises the lit zone deep-blue -> cyan -> ice -> white; only the settled
# level (4) carries the radial 7-band map and the ONE warm gold OUTER rim. Pure
# string emission, no escapes when NO_COLOR (caller guards that).
#
# Settled radial bands by d (d6 = the outermost '.:=========:.' ring only):
#   d0 (row6)      bright WHITE core   d4 (rows2,10)  DEEP blue
#   d1 (rows5,7)   ICE                 d5 (rows1,11)  STEEL
#   d2 (rows4,8)   CYAN                d6 (rows0,12)  GOLD  <- single warm rim
#   d3 (rows3,9)   CYAN (dim/dimcyan)
_ui_reactor_rowcolor() {
    local level="$1" d="$2"
    if [ "$level" -le 0 ]; then
        # Cold: the whole block sits in deep blue, core just a touch up.
        if [ "$d" -eq 0 ]; then printf '%s' "$UI_CYAN"; else printf '%s' "$UI_DEEP"; fi
    elif [ "$level" -eq 1 ]; then
        # Charging: rings deep, inner bands warm to cyan.
        if [ "$d" -le 1 ]; then printf '%s' "$UI_CYAN"; else printf '%s' "$UI_DEEP"; fi
    elif [ "$level" -eq 2 ]; then
        # Brighter: core ice, mid rings cyan, outer deep.
        if [ "$d" -eq 0 ]; then printf '%s' "${UI_BOLD}${UI_ICE}"
        elif [ "$d" -le 3 ]; then printf '%s' "$UI_CYAN"
        else printf '%s' "$UI_DEEP"; fi
    elif [ "$level" -eq 3 ]; then
        # Almost there: core near-white, halo ice, rings cyan/deep, frame steel.
        if [ "$d" -eq 0 ]; then printf '%s' "${UI_BOLD}${UI_BRIGHT}"
        elif [ "$d" -le 1 ]; then printf '%s' "${UI_BOLD}${UI_ICE}"
        elif [ "$d" -le 2 ]; then printf '%s' "$UI_CYAN"
        elif [ "$d" -le 3 ]; then printf '%s' "$UI_DIMCYAN"
        elif [ "$d" -le 4 ]; then printf '%s' "$UI_DEEP"
        else printf '%s' "$UI_STEEL"; fi
    else
        # Settled (canonical): cool 7-band radial map; ONE warm gold rim at d6.
        if [ "$d" -eq 0 ]; then printf '%s' "${UI_BOLD}${UI_BRIGHT}"   # d0 core white
        elif [ "$d" -le 1 ]; then printf '%s' "${UI_BOLD}${UI_ICE}"    # d1 ice
        elif [ "$d" -le 2 ]; then printf '%s' "$UI_CYAN"               # d2 cyan
        elif [ "$d" -le 3 ]; then printf '%s' "$UI_DIMCYAN"            # d3 dim cyan
        elif [ "$d" -le 4 ]; then printf '%s' "$UI_DEEP"               # d4 deep blue
        elif [ "$d" -le 5 ]; then printf '%s' "$UI_STEEL"              # d5 steel
        else printf '%s' "$UI_GOLD"; fi   # d6 outer ring: single warm gold rim
    fi
    return 0
}

# Draw the settled reactor block (static path / NO_COLOR). The canonical artifact
# a NO_COLOR capture verifies. Returns 0.
_ui_reactor_draw() {
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        local sp; sp="$(_ui_pad "${#line}")"
        if [ "$UI_COLOR_MODE" = "none" ]; then
            printf '%s%s\n' "$sp" "$line"
        else
            local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
            local col; col="$(_ui_reactor_rowcolor 4 "$d")"
            printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        fi
        idx=$((idx + 1))
    done
    return 0
}

# --- futuristic flourishes (reticle / scan-sweep / boot readout) -------------
# All three are tty-only motion (UI_ANIM gated): a complete no-op off-tty /
# NO_COLOR / no-color so a piped/CI capture stays clean. Each hides the cursor
# only for the duration of its own motion and always restores it.

# TARGETING RETICLE (flourish C): subtle corner brackets framing the coiled
# reactor block during charge-up, then they fade and clear. Drawn ABOVE the
# reactor block as a brief lock-on cue; the brackets sit at the block's width so
# they bracket the reactor without overlapping or obscuring it. A no-op off-tty.
# Returns 0; on a tty it leaves the cursor on a fresh line ready for the block.
_ui_reactor_reticle() {
    [ "$UI_ANIM" -eq 1 ] || return 0
    local w="${#_UI_REACTOR_ART[0]}"          # reactor block visible width (25)
    local sp; sp="$(_ui_pad "$w")"
    # Bracket arms hug the block corners; inner span is the rest.
    local arm=3
    [ "$arm" -gt $(( w / 2 )) ] && arm=$(( w / 2 ))
    local inner=$(( w - arm * 2 ))
    [ "$inner" -lt 0 ] && inner=0
    local gap; gap="$(_ui_repeat ' ' "$inner")"
    local tl="$UI_TL" tr="$UI_TR" bl="$UI_BL" br="$UI_BR"
    local lead; lead="$(_ui_repeat "$UI_HR_CH" "$(( arm - 1 ))")"
    local top="${tl}${lead}${gap}${lead}${tr}"
    local bot="${bl}${lead}${gap}${lead}${br}"
    printf '\033[?25l'
    # Settle the reticle in two quick brightness beats, then fade + clear it so
    # it reads as a lock-on that releases as the reactor takes over.
    local beat col
    for beat in 1 2; do
        col="${UI_DIM}${UI_STEEL}"
        [ "$beat" -eq 2 ] && col="${UI_BOLD}${UI_CYAN}"
        printf '\r\033[K%s%s%s%s\n' "$sp" "$col" "$top" "$UI_RESET"
        printf '%s%s%s%s' "$sp" "$col" "$bot" "$UI_RESET"
        _ui_nap 0.05
        # Rewind to overwrite both reticle rows on the next beat.
        printf '\r\033[1A'
    done
    # Clear the two reticle rows entirely (let the reactor block draw clean).
    printf '\r\033[K\n\033[K\r\033[1A'
    printf '\033[?25h'
    return 0
}

# SCAN-SWEEP (flourish D): ONE clean horizontal scanline travels top->bottom
# across the just-settled banner region (the reactor block height), a single
# elegant pass, then gone. Assumes the cursor is just BELOW the reactor block;
# it moves up over the block, paints a moving bright underline-row marker, and
# returns the cursor to where it started — leaving the settled reactor intact.
# A no-op off-tty. Returns 0.
_ui_reactor_scansweep() {
    [ "$UI_ANIM" -eq 1 ] || return 0
    local rows="${#_UI_REACTOR_ART[@]}"
    local w="${#_UI_REACTOR_ART[0]}"
    local sp; sp="$(_ui_pad "$w")"
    local bar; bar="$(_ui_repeat "$UI_HR_CH" "$w")"
    printf '\033[?25l'
    local r=0
    while [ "$r" -lt "$rows" ]; do
        # Move to row r of the block (cursor starts just below the block).
        printf '\033[%dA' "$(( rows - r ))"
        # Paint a single bright scan marker across the block width on this row,
        # then immediately restore that row's settled reactor glyphs.
        local line="${_UI_REACTOR_ART[$r]}"
        local mid=$(( rows / 2 ))
        local d=$(( r - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col; col="$(_ui_reactor_rowcolor 4 "$d")"
        printf '\r\033[K%s%s%s%s' "$sp" "${UI_BOLD}${UI_ICE}" "$bar" "$UI_RESET"
        _ui_nap 0.018
        printf '\r\033[K%s%s%s%s' "$sp" "$col" "$line" "$UI_RESET"
        # Back down to the start row (just below the block).
        printf '\033[%dB\r' "$(( rows - r ))"
        r=$(( r + 1 ))
    done
    printf '\033[?25h'
    return 0
}

# BOOT READOUT (flourish B): a short DECORATIVE sequence of right-aligned status
# lines themed to the installer powering up — each ticks from a dim "....." to a
# settled cool "[ OK ]" / "[ READY ]" tag. This is intro FLAVOR for the installer
# only; it does NOT assert that any real DARWIN subsystem is online (honesty-
# first). tty-only motion; off-tty it prints the settled lines instantly, right-
# aligned, in the cool palette. Cursor hidden during motion + always restored.
_ui_boot_readout() {
    _ui_ensure
    # label|settled-tag pairs (decorative install-sequence subsystems).
    local -a labels tags
    labels=( "ARC REACTOR" "RENDER PIPELINE" "OPERATOR LINK" "INSTALL CORE" )
    tags=(   "OK"          "READY"           "OK"            "READY" )
    local n="${#labels[@]}"

    # Each line reads "<label> <dot-leader> [ TAG ]". The dot-leader for every
    # line fills to the SAME total width so the labels left-align and the tags
    # line up in one clean right column. Leader width = (maxlabel - thislabel) +
    # a fixed pad; total dotzone is computed from the widest label.
    local i lab maxlbl=0
    i=0
    while [ "$i" -lt "$n" ]; do
        [ "${#labels[$i]}" -gt "$maxlbl" ] && maxlbl="${#labels[$i]}"
        i=$(( i + 1 ))
    done
    local fixeddots=5
    local dotzone=$(( maxlbl + fixeddots ))   # label+leader span (constant/line)
    # Block width: "<dotzone> [ TAG ]" where TAG width can vary; pad tag cell to
    # the widest tag so the closing bracket lines up too.
    local maxtag=0
    i=0
    while [ "$i" -lt "$n" ]; do
        [ "${#tags[$i]}" -gt "$maxtag" ] && maxtag="${#tags[$i]}"
        i=$(( i + 1 ))
    done
    local blockw=$(( dotzone + 1 + 2 + maxtag + 2 ))   # +space +"[ " +tag +" ]"
    local rpad=$(( UI_WIDTH - blockw - 2 ))
    [ "$rpad" -lt 0 ] && rpad=0
    local indent; indent="$(_ui_repeat ' ' "$rpad")"

    # Compose one settled line. Emits to stdout (no newline). $1 = index, $2 =
    # label-color escape (none-mode passes empty). Leader length per line keeps
    # the tag column aligned: dots = dotzone - labellen.
    if [ "$UI_COLOR_MODE" = "none" ]; then
        i=0
        while [ "$i" -lt "$n" ]; do
            lab="${labels[$i]}"
            local nd=$(( dotzone - ${#lab} ))
            local tagpad; tagpad="$(_ui_repeat ' ' "$(( maxtag - ${#tags[$i]} ))")"
            printf '%s%s %s [ %s%s ]\n' \
                "$indent" "$lab" "$(_ui_repeat '.' "$nd")" "${tags[$i]}" "$tagpad"
            i=$(( i + 1 ))
        done
        return 0
    fi

    if [ "$UI_ANIM" -eq 0 ]; then
        i=0
        while [ "$i" -lt "$n" ]; do
            lab="${labels[$i]}"
            local nd=$(( dotzone - ${#lab} ))
            local tagpad; tagpad="$(_ui_repeat ' ' "$(( maxtag - ${#tags[$i]} ))")"
            printf '%s%s%s%s %s%s%s %s[ %s%s%s%s%s ]%s\n' \
                "$indent" "${UI_DIM}${UI_STEEL}" "$lab" "$UI_RESET" \
                "${UI_DIM}${UI_GREY}" "$(_ui_repeat '.' "$nd")" "$UI_RESET" \
                "${UI_STEEL}" "${UI_BOLD}${UI_CYAN}" "${tags[$i]}" "$tagpad" "$UI_RESET" "${UI_STEEL}" "$UI_RESET"
            i=$(( i + 1 ))
        done
        return 0
    fi

    printf '\033[?25l'
    i=0
    while [ "$i" -lt "$n" ]; do
        lab="${labels[$i]}"
        local nd=$(( dotzone - ${#lab} ))
        local tagpad; tagpad="$(_ui_repeat ' ' "$(( maxtag - ${#tags[$i]} ))")"
        # Tick the dot-leader in 1 -> nd, then settle the tag.
        local dots=1
        while [ "$dots" -le "$nd" ]; do
            printf '\r\033[K%s%s%s%s %s%s%s' \
                "$indent" "${UI_DIM}${UI_STEEL}" "$lab" "$UI_RESET" \
                "${UI_DIM}${UI_GREY}" "$(_ui_repeat '.' "$dots")" "$UI_RESET"
            _ui_nap 0.012
            dots=$(( dots + 1 ))
        done
        # Settle this line with its cool tag and commit it (newline).
        printf '\r\033[K%s%s%s%s %s%s%s %s[ %s%s%s%s%s ]%s\n' \
            "$indent" "${UI_BOLD}${UI_STEEL}" "$lab" "$UI_RESET" \
            "${UI_DIM}${UI_GREY}" "$(_ui_repeat '.' "$nd")" "$UI_RESET" \
            "${UI_STEEL}" "${UI_BOLD}${UI_CYAN}" "${tags[$i]}" "$tagpad" "$UI_RESET" "${UI_STEEL}" "$UI_RESET"
        _ui_nap 0.03
        i=$(( i + 1 ))
    done
    printf '\033[?25h'
    return 0
}

darwin_banner() {
    _ui_ensure

    printf '\n'

    # Top HUD frame bar (names the operator along the top edge).
    ui_frame_top "D.A.R.W.I.N. // ARC OS"

    # Brief diagnostic pre-roll (motion only; no-op on non-tty).
    _ui_reactor_animate

    printf '\n'

    # TARGETING RETICLE (flourish C): brief corner-bracket lock-on framing the
    # reactor as it charges, then it fades + clears. tty-only; no-op off-tty.
    _ui_reactor_reticle

    local rows="${#_UI_REACTOR_ART[@]}"
    if [ "$UI_ANIM" -eq 1 ]; then
        # ANIMATED SPIN-UP IN PLACE. Cursor hidden for all motion, restored at the
        # end (and on any exit by the trap). Every frame rewinds to the block top
        # (\033[<rows>A) and clears each row (\033[K) so frames overwrite cleanly
        # ending EXACTLY on the settled reactor (the NO_COLOR artifact).
        printf '\033[?25l'                       # hide cursor during motion
        _ui_reactor_build_glint_path            # precompute the rim path once

        # (1) CHARGE: brighten the whole block deep-blue -> cyan -> ice -> white.
        local lvl
        for lvl in 0 2 3 4; do
            [ "$lvl" -eq 0 ] || printf '\033[%dA' "$rows"
            _ui_reactor_charge_frame "$lvl"
            _ui_nap 0.055
        done

        # (2) POWER-UP RING BY RING: light the rings inner-to-outer in sequence.
        local mid=$(( rows / 2 )) lit
        for lit in 0 1 2 3 4 5 6; do
            printf '\033[%dA' "$rows"
            _ui_reactor_ring_frame "$lit"
            _ui_nap 0.042
        done

        # (3) ROTATING SWEEP: a bright glint travels around the rim ~1.25 turns.
        local steps="${#_UI_GLINT_ROW[@]}"
        if [ "$steps" -gt 0 ]; then
            local total=$(( steps * 5 / 4 )) g=0 stride=1
            [ "$steps" -ge 36 ] && stride=2   # keep a big rim brisk + smooth
            while [ "$g" -lt "$total" ]; do
                local p=$(( g % steps ))
                printf '\033[%dA' "$rows"
                _ui_reactor_glint_frame "${_UI_GLINT_ROW[$p]}" "${_UI_GLINT_COL[$p]}"
                _ui_nap 0.012
                g=$(( g + stride ))
            done
        fi

        # (4) CORE PULSE: the center pulses dim -> bright a few beats, settles bright.
        local beat
        for beat in 1 2 1 2; do
            printf '\033[%dA' "$rows"
            _ui_reactor_core_frame "$beat"
            _ui_nap 0.085
        done
        # End EXACTLY on the settled reactor.
        printf '\033[%dA' "$rows"
        _ui_reactor_charge_frame 4

        printf '\033[?25h'                       # restore cursor
    else
        _ui_reactor_draw
    fi

    # SCAN-SWEEP (flourish D): one clean horizontal scanline passes down the
    # just-settled reactor block, then gone. Cursor is one row below the block
    # here, exactly where the sweep expects it. tty-only; no-op off-tty.
    _ui_reactor_scansweep

    printf '\n'

    # PROGRESSIVE REVEAL — each HUD element comes online in sequence with a beat.
    _ui_nap 0.12
    # WORDMARK DECODE-IN (flourish A): "D . A . R . W . I . N ." resolves from a
    # few frames of scrambled glyphs, locking in left-to-right (instant +
    # centered off-tty — identical final artifact).
    _ui_wordmark_decode "D . A . R . W . I . N ." "${UI_BOLD}${UI_BRIGHT}"
    _ui_nap 0.10
    _ui_centerline "Digital Assistant Running Wholly In-house, Natively" "${UI_DIM}${UI_ICE}"
    printf '\n'
    _ui_nap 0.10

    # Subtitle rule (cool diamonds + dim cyan tagline — no gold here).
    if [ "$UI_COLOR_MODE" = "none" ]; then
        _ui_centerline "${UI_DIAMOND}  one-command install // built fresh // honesty-first  ${UI_DIAMOND}"
    else
        local sub="one-command install // built fresh // honesty-first"
        local subwidth=$(( ${#sub} + 6 ))
        local sp; sp="$(_ui_pad "$subwidth")"
        printf '%s%s%s%s %s%s%s %s%s%s%s\n' \
            "$sp" "${UI_CYAN}" "$UI_DIAMOND" "$UI_RESET" \
            "${UI_DIM}${UI_CYAN}" "$sub" "$UI_RESET" \
            "${UI_CYAN}" "$UI_DIAMOND" "$UI_RESET" ""
    fi
    _ui_nap 0.14

    # BOOT READOUT (flourish B): a short, right-aligned, DECORATIVE sequence of
    # install-sequence subsystems ticking from "....." to a cool [ OK ]/[ READY ]
    # tag. Installer intro flavor only — it does NOT claim a real subsystem is
    # online. Instant + right-aligned off-tty.
    printf '\n'
    _ui_boot_readout
    _ui_nap 0.12

    # CREDIT BYLINE — "Made by <author>", the author name in the single warm gold
    # signature (decodes in on a tty; instant + centered off-tty).
    ui_greet
    return 0
}

# Redraw the reactor block at a charge LEVEL with each row cleared first, so an
# in-place charge overwrites the previous frame without leftover glyphs. The
# cursor is assumed to be at the block's top-left column. Returns 0.
_ui_reactor_charge_frame() {
    local level="$1"
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        printf '\033[K'   # clear this row before (re)printing it
        local sp; sp="$(_ui_pad "${#line}")"
        local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col; col="$(_ui_reactor_rowcolor "$level" "$d")"
        printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        idx=$((idx + 1))
    done
    return 0
}

# RING-BY-RING POWER-UP frame. Rings whose radial distance d <= LIT are drawn at
# their settled (level-4) color (lit); rings still beyond the lit front stay cold
# in deep-blue. Driving LIT from 0 -> max d lights the reactor inner-to-outer (the
# core ignites first, energy radiates outward); the final LIT==max equals the
# settled frame. Each row is cleared first so frames overwrite cleanly. Cursor is
# assumed at the block's top-left. Returns 0.
_ui_reactor_ring_frame() {
    local lit="$1"
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        printf '\033[K'
        local sp; sp="$(_ui_pad "${#line}")"
        local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col
        if [ "$d" -le "$lit" ]; then
            col="$(_ui_reactor_rowcolor 4 "$d")"   # lit: settled radial color
        else
            col="$UI_DEEP"                          # still cold: deep-blue
        fi
        printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        idx=$((idx + 1))
    done
    return 0
}

# A ROTATING GLINT frame: the settled reactor with ONE bright cell lit at
# (GROW, GCOL) — GCOL is a 0-based index into the row's glyph string. The glint
# is split out as a bright ice-white character so it reads as a highlight
# travelling around the rim. A GROW out of range / a space at GCOL just draws the
# settled frame (the glint skips gaps). Each row cleared first. Returns 0.
_ui_reactor_glint_frame() {
    local grow="$1" gcol="$2"
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        printf '\033[K'
        local sp; sp="$(_ui_pad "${#line}")"
        local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col; col="$(_ui_reactor_rowcolor 4 "$d")"
        if [ "$idx" -eq "$grow" ] && [ "$gcol" -ge 0 ] && [ "$gcol" -lt "${#line}" ] \
           && [ "${line:$gcol:1}" != " " ]; then
            # Split the row: [pre]<bright glint><post>, restoring the row color after.
            local pre="${line:0:$gcol}" ch="${line:$gcol:1}" post="${line:$((gcol + 1))}"
            printf '%s%s%s%s%s%s%s%s%s\n' \
                "$sp" "$col" "$pre" \
                "${UI_BOLD}${UI_BRIGHT}" "$ch" \
                "$UI_RESET" "$col" "$post" "$UI_RESET"
        else
            printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        fi
        idx=$((idx + 1))
    done
    return 0
}

# A CORE-PULSE frame: the settled reactor, but the central core band (d <= 1) is
# drawn at brightness LEVEL — 1 = dim (cyan core), 2 = settled (near-white) — so
# alternating the two beats reads as the core pulsing. Outer rings stay at their
# settled radial color throughout. Each row cleared first. Returns 0.
_ui_reactor_core_frame() {
    local pulse="$1"   # 1 = dim, 2 = bright
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        printf '\033[K'
        local sp; sp="$(_ui_pad "${#line}")"
        local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col
        if [ "$d" -le 1 ]; then
            if [ "$pulse" -eq 1 ]; then col="$UI_CYAN"; else col="$(_ui_reactor_rowcolor 4 "$d")"; fi
        else
            col="$(_ui_reactor_rowcolor 4 "$d")"
        fi
        printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        idx=$((idx + 1))
    done
    return 0
}

# A POWER-SURGE frame: the radial bands ramp OUTWARD toward a brief full-bright
# PEAK. LEVEL (0..3) drives how far the bright front has pushed from the core:
# bands at radial distance d <= (LEVEL+1) flash bright (core/halo white -> ice ->
# cyan), bands beyond the front stay at their settled radial color. At the top
# LEVEL the whole disc reads near-fully-lit — a clean energy bloom — WITHOUT ever
# introducing gold to a non-rim band (the bloom uses only the cool family: white,
# ice, cyan; the gold d6 rim is preserved at its settled color throughout). The
# caller settles back to the canonical reactor (_ui_reactor_charge_frame 4) on the
# next frame, so the bloom never persists. Each row cleared first; cursor assumed
# at the block's top-left. tty-only (caller gates on UI_ANIM). Returns 0.
_ui_reactor_surge_frame() {
    local level="$1"
    local rows="${#_UI_REACTOR_ART[@]}"
    local mid=$(( rows / 2 ))
    local front=$(( level + 1 ))   # bright front radius
    local idx=0 line
    for line in "${_UI_REACTOR_ART[@]}"; do
        printf '\033[K'
        local sp; sp="$(_ui_pad "${#line}")"
        local d=$(( idx - mid )); [ "$d" -lt 0 ] && d=$(( -d ))
        local col
        if [ "$d" -le "$front" ]; then
            # Inside the bloom front: brighten on the cool ramp by depth into the
            # front (closer to the front edge = cooler), never gold.
            local depth=$(( front - d ))
            if [ "$depth" -ge 2 ]; then col="${UI_BOLD}${UI_BRIGHT}"
            elif [ "$depth" -eq 1 ]; then col="${UI_BOLD}${UI_ICE}"
            else col="${UI_BOLD}${UI_CYAN}"; fi
        else
            # Beyond the front: settled radial color (keeps the d6 gold rim intact).
            col="$(_ui_reactor_rowcolor 4 "$d")"
        fi
        printf '%s%s%s%s\n' "$sp" "$col" "$line" "$UI_RESET"
        idx=$((idx + 1))
    done
    return 0
}

# Build the outer-ring perimeter path (row col pairs, clockwise from the top) for
# the rotating glint. Sets the file-scope arrays _UI_GLINT_ROW / _UI_GLINT_COL.
# Walks the actual non-space glyphs of the TOP row left->right, down the RIGHT
# edge (rightmost glyph per row), the BOTTOM row right->left, and up the LEFT
# edge (leftmost glyph per row) so the glint hugs the visible rim. Idempotent
# (built once). Returns 0.
_ui_reactor_build_glint_path() {
    [ -n "${_UI_GLINT_BUILT:-}" ] && return 0
    _UI_GLINT_BUILT=1
    _UI_GLINT_ROW=() ; _UI_GLINT_COL=()
    local rows="${#_UI_REACTOR_ART[@]}"
    local last=$(( rows - 1 ))
    local top="${_UI_REACTOR_ART[0]}"
    local bot="${_UI_REACTOR_ART[$last]}"
    local w="${#top}"
    local c r ch
    # Top edge: left -> right across non-space glyphs.
    c=0
    while [ "$c" -lt "$w" ]; do
        ch="${top:$c:1}"; [ "$ch" != " " ] && { _UI_GLINT_ROW+=(0); _UI_GLINT_COL+=("$c"); }
        c=$(( c + 1 ))
    done
    # Right edge: top+1 -> bottom-1, rightmost non-space glyph of each row.
    r=1
    while [ "$r" -lt "$last" ]; do
        local lr="${_UI_REACTOR_ART[$r]}"; c=$(( w - 1 ))
        while [ "$c" -ge 0 ]; do [ "${lr:$c:1}" != " " ] && break; c=$(( c - 1 )); done
        [ "$c" -ge 0 ] && { _UI_GLINT_ROW+=("$r"); _UI_GLINT_COL+=("$c"); }
        r=$(( r + 1 ))
    done
    # Bottom edge: right -> left across non-space glyphs.
    c=$(( w - 1 ))
    while [ "$c" -ge 0 ]; do
        ch="${bot:$c:1}"; [ "$ch" != " " ] && { _UI_GLINT_ROW+=("$last"); _UI_GLINT_COL+=("$c"); }
        c=$(( c - 1 ))
    done
    # Left edge: bottom-1 -> top+1, leftmost non-space glyph of each row.
    r=$(( last - 1 ))
    while [ "$r" -gt 0 ]; do
        local ll="${_UI_REACTOR_ART[$r]}"; c=0
        while [ "$c" -lt "$w" ]; do [ "${ll:$c:1}" != " " ] && break; c=$(( c + 1 )); done
        [ "$c" -lt "$w" ] && { _UI_GLINT_ROW+=("$r"); _UI_GLINT_COL+=("$c"); }
        r=$(( r - 1 ))
    done
    return 0
}

# --- credit byline -----------------------------------------------------------
# A centered "MADE BY <AUTHOR>" CREDIT BYLINE — the build's signature, given a
# slightly elevated treatment with thin bracket caps so it reads as a credit,
# not a chat line. Rendered FULL UPPERCASE ("MADE BY DARWIN CAPANI") via tr at
# render time only (the stored DARWIN_AUTHOR keeps its title case). "MADE BY "
# sits in a cool/dim tone; the author name carries the single warm GOLD
# signature (consistent with the operator-name gold rule). NO time-of-day, NO
# butler address.
#
# Kept named ui_greet (and accepts an optional name arg) so install.sh's existing
# call site keeps working unchanged; the optional $1 overrides DARWIN_AUTHOR.
# On an interactive tty the name DECODES IN from scrambled glyphs into gold,
# resolving into the UPPERCASE name; off-tty / NO_COLOR / no-color it prints
# instantly + centered. Static + log-safe. Used by BOTH the opening banner and
# the ui_online finale restatement, so both render uppercase.
ui_greet() {
    _ui_ensure
    local name="${1:-$DARWIN_AUTHOR}"
    name="$(printf '%s' "$name" | tr -d '\r\n\033')"
    [ -z "$name" ] && name="Darwin Capani"
    # Render the byline FULL UPPERCASE without mutating the stored DARWIN_AUTHOR:
    # uppercase only the local render copies via tr (bash 3.2 — no ${var^^}). The
    # lead "Made by " and the name both uppercase => "MADE BY DARWIN CAPANI".
    name="$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')"
    printf '\n'

    # Thin bracket caps elevate it to a signature: ⟦ MADE BY <NAME> ⟧
    local cap_l="[" cap_r="]"
    if [ "$UI_UTF8" -eq 1 ]; then cap_l="⟦"; cap_r="⟧"; fi
    local lead="MADE BY "
    # Center on the full visible line: "⟦ MADE BY <NAME> ⟧" (all ASCII => ${#}
    # is exact; uppercasing does not change the visible width).
    local full="${cap_l} ${lead}${name} ${cap_r}"
    local sp; sp="$(_ui_pad "${#full}")"

    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s\n' "$sp" "$full"
        return 0
    fi

    if [ "$UI_ANIM" -eq 0 ]; then
        # Static colored print: caps steel, "Made by" dim cool, name in gold.
        printf '%s%s%s %s%s%s%s%s %s%s%s\n' \
            "$sp" \
            "${UI_DIM}${UI_STEEL}" "$cap_l" \
            "${UI_RESET}${UI_DIM}${UI_ICE}" "$lead" \
            "${UI_RESET}${UI_BOLD}${UI_GOLD}" "$name" \
            "${UI_RESET}${UI_DIM}${UI_STEEL}" "$cap_r" "$UI_RESET" ""
        return 0
    fi

    # tty: caps + lead settle instantly, then the author name DECODES IN from
    # scrambled glyphs into gold. Cursor hidden during motion + ALWAYS restored.
    printf '\033[?25l'
    local cs="$_UI_DECODE_CHARSET" cslen; cslen="${#cs}"
    local len="${#name}" locked=0 step=2
    while [ "$locked" -lt "$len" ]; do
        local scram="" i=0 ch
        while [ "$i" -lt "$len" ]; do
            ch="${name:$i:1}"
            if [ "$i" -lt "$locked" ] || [ "$ch" = " " ]; then
                scram="${scram}${ch}"
            else
                scram="${scram}${cs:$(( RANDOM % cslen )):1}"
            fi
            i=$(( i + 1 ))
        done
        printf '\r%s%s%s %s%s%s%s%s' \
            "$sp" \
            "${UI_DIM}${UI_STEEL}" "$cap_l" \
            "${UI_RESET}${UI_DIM}${UI_ICE}" "$lead" \
            "${UI_RESET}${UI_DIM}${UI_GOLD}" "$scram" "$UI_RESET"
        _ui_nap 0.05
        locked=$(( locked + step ))
    done
    # Settle: full byline, name in bold gold, closing cap.
    printf '\r%s%s%s %s%s%s%s%s %s%s%s\n' \
        "$sp" \
        "${UI_DIM}${UI_STEEL}" "$cap_l" \
        "${UI_RESET}${UI_DIM}${UI_ICE}" "$lead" \
        "${UI_RESET}${UI_BOLD}${UI_GOLD}" "$name" \
        "${UI_RESET}${UI_DIM}${UI_STEEL}" "$cap_r" "$UI_RESET" ""
    printf '\033[?25h'
    return 0
}

# --- reactor pre-roll diagnostic ---------------------------------------------
# A brief, tasteful "system diagnostic" shown only on an interactive tty just
# before the reactor charges into view: each self-check readout scans on one
# overwritten line and clears, naming the operator profile. The reactor's actual
# brightening now happens IN PLACE in darwin_banner (charge-up), so this is only
# the quick pre-roll. Adds a fraction of a second; a complete no-op when
# UI_ANIM=0. Cursor hidden during motion and ALWAYS restored on exit.
_ui_reactor_animate() {
    [ "$UI_ANIM" -eq 1 ] || return 0

    printf '\033[?25l'   # hide cursor during motion

    # System diagnostic: each readout scans then clears (one overwritten line).
    local -a checks
    checks=(
        "arc reactor ........ core priming"
        "HUD telemetry ...... linking"
        "voice cortex ....... priming"
        "operator profile ... ${DARWIN_OPERATOR}"
    )
    local c
    for c in "${checks[@]}"; do
        printf '\r\033[K  %s%s%s %sscanning%s %s%s%s' \
            "${UI_CYAN}" "$UI_SWEEP" "$UI_RESET" \
            "${UI_DIM}${UI_GREY}" "$UI_RESET" \
            "$UI_STEEL" "$c" "$UI_RESET"
        _ui_nap 0.055
    done
    printf '\r\033[K'    # erase the diagnostic line entirely

    printf '\033[?25h'   # restore cursor
    return 0
}

# Public alias: a standalone power-up + banner ("coming online" entry). The
# installer does not require it, but it is a clean cinematic entry point.
ui_reactor_powerup() {
    _ui_ensure
    darwin_banner
    return 0
}

# --- SYSTEM STATUS BOARD -----------------------------------------------------
# A framed HUD "BOOT MANIFEST": a titled panel whose rows are <label> <dot-leader>
# [ TAG ], the tags right-aligned in one clean column (like _ui_boot_readout). The
# rows are NOT decorative — the CALLER passes real install outcomes, so this is an
# HONEST readout. On a tty each row TICKS in (the dot-leader grows, then the tag
# settles) with a brief beat; off-tty / NO_COLOR every row prints settled
# instantly. Cursor hidden during motion + ALWAYS restored.
#
# Tag color is chosen by the tag TEXT so honesty + the GOLD RULE both hold:
#   GREEN  for good/ready/done  : BUILT READY RESIDENT ENABLED ONLINE OK DONE
#   STEEL/GREY (cool, neutral)  : everything else (DEFERRED MANUAL SKIPPED OFF
#                                  WILL BUILD PLANNED N RESIDENT ...) — never gold.
# (gold stays reserved for the reactor rim + the author byline only.)
#
# Usage:
#   ui_status_board "BOOT MANIFEST" \
#       "CORE DAEMON (darwind)|BUILT" \
#       "ON-DEVICE MODELS|5 RESIDENT" \
#       ...
# Each extra arg is a "LABEL|TAG" pair (the FIRST '|' splits; the label may not
# contain '|', tags here never do). The title is the framed header. Returns 0.

# Classify a tag as "good" (green) when its FIRST word is an affirmative state;
# anything else is neutral (cool steel). Echoes "good" or "neutral". Pure string
# test, set -e safe. The leading count form "N RESIDENT" stays neutral on purpose
# (a count is informational, not a green "done") UNLESS it is a bare affirmative.
_ui_tag_class() {
    local tag="$1" head=""
    # First whitespace-delimited token, uppercased for a stable compare.
    head="${tag%%[ ]*}"
    head="$(printf '%s' "$head" | tr '[:lower:]' '[:upper:]')"
    case "$head" in
        BUILT|READY|RESIDENT|ENABLED|ONLINE|OK|DONE|ACTIVE|LOADED) printf 'good' ;;
        *) printf 'neutral' ;;
    esac
    return 0
}

# Emit one settled board row to stdout (with trailing newline). Args:
#   $1 indent  $2 label  $3 tag  $4 dotzone  $5 maxtag  $6 dim(1=dim/ticking,0=settled)
# Honors UI_COLOR_MODE. Internal helper for ui_status_board.
_ui_board_row() {
    local indent="$1" lab="$2" tag="$3" dotzone="$4" maxtag="$5" dim="$6"
    local nd=$(( dotzone - ${#lab} )); [ "$nd" -lt 1 ] && nd=1
    local dots; dots="$(_ui_repeat '.' "$nd")"
    local tagpad; tagpad="$(_ui_repeat ' ' "$(( maxtag - ${#tag} ))")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s %s [ %s%s ]\n' "$indent" "$lab" "$dots" "$tag" "$tagpad"
        return 0
    fi
    local labcol="${UI_BOLD}${UI_STEEL}" tagcol="${UI_BOLD}${UI_GREEN}"
    [ "$dim" -eq 1 ] && labcol="${UI_DIM}${UI_STEEL}"
    [ "$(_ui_tag_class "$tag")" = "neutral" ] && tagcol="${UI_BOLD}${UI_ICE}"
    printf '%s%s%s%s %s%s%s %s[ %s%s%s%s%s ]%s\n' \
        "$indent" "$labcol" "$lab" "$UI_RESET" \
        "${UI_DIM}${UI_GREY}" "$dots" "$UI_RESET" \
        "${UI_STEEL}" "$tagcol" "$tag" "$tagpad" "$UI_RESET" "${UI_STEEL}" "$UI_RESET"
    return 0
}

ui_status_board() {
    _ui_ensure
    local title="${1:-SYSTEM STATUS}"; shift 2>/dev/null || true

    # Parse the "LABEL|TAG" pairs into parallel arrays (bash 3.2: no assoc arrays,
    # no mapfile). Split on the FIRST '|' only.
    local -a labels tags
    local pair lab tag
    for pair in "$@"; do
        lab="${pair%%|*}"
        tag="${pair#*|}"
        [ "$tag" = "$pair" ] && tag=""   # no '|' => empty tag (still aligns)
        labels+=("$lab")
        tags+=("$tag")
    done
    local n="${#labels[@]}"
    [ "$n" -eq 0 ] && return 0

    # Column math: widest label + fixed leader => dot-leader zone; widest tag =>
    # tag cell width, so every "[ TAG ]" closes in one column.
    local i maxlbl=0 maxtag=0
    i=0
    while [ "$i" -lt "$n" ]; do
        [ "${#labels[$i]}" -gt "$maxlbl" ] && maxlbl="${#labels[$i]}"
        [ "${#tags[$i]}" -gt "$maxtag" ] && maxtag="${#tags[$i]}"
        i=$(( i + 1 ))
    done
    local fixeddots=4
    local dotzone=$(( maxlbl + fixeddots ))

    # Frame: a titled top bar + a sealing bottom bar spanning a sensible block
    # width. Block content width = dotzone + ' ' + '[ ' + tag + ' ]'.
    local contentw=$(( dotzone + 1 + 2 + maxtag + 2 ))
    local boxw=$(( contentw + 4 ))            # +2 left gutter, +2 right margin
    [ "$boxw" -gt $(( UI_WIDTH - 2 )) ] && boxw=$(( UI_WIDTH - 2 ))
    local lpad=$(( (UI_WIDTH - boxw) / 2 ))
    [ "$lpad" -lt 0 ] && lpad=0
    local outer; outer="$(_ui_repeat ' ' "$lpad")"
    local indent="${outer}  "               # row indent inside the frame

    # --- top frame bar:  ╭── [ TITLE ] ─────────────────╮ ---
    local tcell="[ ${title} ]"
    local tlead="${UI_TL}${UI_HR_CH}${UI_HR_CH} "
    local tvis=$(( ${#tlead} + ${#tcell} + 1 + 1 ))
    local tfill=$(( boxw - tvis )); [ "$tfill" -lt 1 ] && tfill=1
    local trule; trule="$(_ui_repeat "$UI_HR_CH" "$tfill")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '\n%s%s%s %s%s\n' "$outer" "$tlead" "$tcell" "$trule" "$UI_TR"
    else
        printf '\n%s%s%s%s%s[ %s%s%s%s ]%s %s%s%s%s%s%s\n' \
            "$outer" "${UI_STEEL}" "$tlead" "$UI_RESET" \
            "${UI_STEEL}" "${UI_BOLD}${UI_CYAN}" "$title" "$UI_RESET" "${UI_STEEL}" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$trule" "$UI_RESET" \
            "${UI_STEEL}" "$UI_TR" "$UI_RESET"
    fi

    # --- rows ---
    if [ "$UI_ANIM" -eq 0 ]; then
        i=0
        while [ "$i" -lt "$n" ]; do
            _ui_board_row "$indent" "${labels[$i]}" "${tags[$i]}" "$dotzone" "$maxtag" 0
            i=$(( i + 1 ))
        done
    else
        printf '\033[?25l'
        i=0
        while [ "$i" -lt "$n" ]; do
            lab="${labels[$i]}"; tag="${tags[$i]}"
            local nd=$(( dotzone - ${#lab} )); [ "$nd" -lt 1 ] && nd=1
            # Tick the dot-leader in, dim, then settle the row with its tag.
            local dd=1
            while [ "$dd" -le "$nd" ]; do
                printf '\r\033[K%s%s%s%s %s%s%s' \
                    "$indent" "${UI_DIM}${UI_STEEL}" "$lab" "$UI_RESET" \
                    "${UI_DIM}${UI_GREY}" "$(_ui_repeat '.' "$dd")" "$UI_RESET"
                _ui_nap 0.01
                dd=$(( dd + 1 ))
            done
            printf '\r\033[K'
            _ui_board_row "$indent" "$lab" "$tag" "$dotzone" "$maxtag" 0
            _ui_nap 0.03
            i=$(( i + 1 ))
        done
        printf '\033[?25h'
    fi

    # --- bottom seal bar ---
    local blead="${UI_BL}${UI_HR_CH}${UI_HR_CH}"
    local bfill=$(( boxw - ${#blead} - 1 )); [ "$bfill" -lt 1 ] && bfill=1
    local brule; brule="$(_ui_repeat "$UI_HR_CH" "$bfill")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s%s%s\n' "$outer" "$blead" "$brule" "$UI_BR"
    else
        printf '%s%s%s%s%s%s%s%s%s%s\n' \
            "$outer" "${UI_STEEL}" "$blead" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$brule" "$UI_RESET" \
            "${UI_STEEL}" "$UI_BR" "$UI_RESET"
    fi
    return 0
}

# --- DIRECTIVE PANEL (next-steps card) ---------------------------------------
# A cohesive HUD "directive" card for one next-steps item: a bracketed header bar
# carrying an index + a TITLE, then the body lines indented under it with a thin
# left rail. The body lines are passed VERBATIM (already-colored substance is
# preserved exactly — keys, paths, commands, caveats), so this only adds framing,
# never edits content. Cool palette only (no gold). Static + log-safe; no motion
# of its own beyond the standard per-line scan beat. Returns 0.
#
# Usage:
#   ui_panel "1" "TCC PERMISSIONS" \
#       "macOS will prompt for Accessibility, Microphone, ..." \
#       "Grant them in System Settings > Privacy & Security ..."
# $1 = index tag (e.g. "1" or "01"); $2 = TITLE; $3.. = body lines (each printed
# on its own row under the header). Empty body lines render as a blank spacer row.
ui_panel() {
    _ui_ensure
    local idx="$1" title="$2"; shift 2 2>/dev/null || true

    # Header bar:  ▸ [ 01 ] TITLE ──────────────────────────────
    local tag="[ ${idx} ]"
    local lead="$UI_G_INFO"
    local headtext="${lead} ${tag} ${title}"
    local hvis=$(( ${#headtext} + 1 + 1 ))       # +space +rule start
    local hfill=$(( UI_WIDTH - hvis - 2 )); [ "$hfill" -lt 2 ] && hfill=2
    [ "$hfill" -gt 56 ] && hfill=56
    local hrule; hrule="$(_ui_repeat "$UI_HR_CH" "$hfill")"
    _ui_scanbeat
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '\n  %s %s %s %s\n' "$lead" "$tag" "$title" "$hrule"
    else
        printf '\n  %s%s%s %s%s%s %s%s%s %s%s%s\n' \
            "${UI_BOLD}${UI_CYAN}" "$lead" "$UI_RESET" \
            "${UI_BOLD}${UI_CYAN}" "$tag" "$UI_RESET" \
            "${UI_BOLD}${UI_BRIGHT}" "$title" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$hrule" "$UI_RESET"
    fi

    # Body rows under a thin left rail. The rail glyph is a dim steel vertical;
    # lines are printed verbatim (callers embed their own color escapes/resets).
    local rail="|"
    [ "$UI_UTF8" -eq 1 ] && rail="│"
    local line
    for line in "$@"; do
        _ui_scanbeat
        if [ "$UI_COLOR_MODE" = "none" ]; then
            if [ -z "$line" ]; then printf '\n'; else printf '    %s %s\n' "$rail" "$line"; fi
        else
            if [ -z "$line" ]; then
                printf '\n'
            else
                printf '    %s%s%s %s\n' "${UI_DIM}${UI_STEEL}" "$rail" "$UI_RESET" "$line"
            fi
        fi
    done
    return 0
}

# --- ONLINE flourish ---------------------------------------------------------
# The finale: a final reactor charge to 100%, the wordmark decodes in, a neutral
# "D.A.R.W.I.N. // ONLINE" status line + a restated "Made by <author>" credit,
# and the bottom HUD frame seals the display. NO butler address, NO time-of-day.
# Static + clean on non-tty/NO_COLOR.
# Optional arg $1 overrides the bottom-frame status (default ALL SYSTEMS NOMINAL);
# the original no-arg call site keeps the default unchanged.
ui_online() {
    _ui_ensure
    local seal="${1:-ALL SYSTEMS NOMINAL}"
    ui_hr

    # On an interactive tty, run a quick final reactor charge so the finish reads
    # as the core stabilizing. Skipped on non-tty/NO_COLOR.
    if [ "$UI_ANIM" -eq 1 ]; then
        local p=0
        while [ "$p" -le 100 ]; do
            ui_progress "$p" "stabilizing arc reactor"
            p=$(( p + 20 ))
            _ui_nap 0.04
        done

        # FINAL REACTOR FLOURISH: redraw the reactor, send one bright glint around
        # the rim, then pulse the core bright -> settled — the core locking in as
        # the system goes live. All in-place, cursor hidden + restored, ending on
        # the settled reactor. A pure no-op when motion is off (the block below
        # never runs and nothing is drawn).
        printf '\n'
        local rows="${#_UI_REACTOR_ART[@]}"
        printf '\033[?25l'
        _ui_reactor_build_glint_path
        _ui_reactor_charge_frame 4               # draw the settled reactor
        local steps="${#_UI_GLINT_ROW[@]}"
        if [ "$steps" -gt 0 ]; then
            local g=0 stride=1
            [ "$steps" -gt 40 ] && stride=2
            while [ "$g" -lt "$steps" ]; do
                printf '\033[%dA' "$rows"
                _ui_reactor_glint_frame "${_UI_GLINT_ROW[$g]}" "${_UI_GLINT_COL[$g]}"
                _ui_nap 0.01
                g=$(( g + stride ))
            done
        fi
        local beat
        for beat in 2 1 2 1 2; do
            printf '\033[%dA' "$rows"
            _ui_reactor_core_frame "$beat"
            _ui_nap 0.08
        done

        # POWER-SURGE BLOOM: the radial bands ramp OUTWARD to a brief full-bright
        # PEAK (a clean energy bloom), then snap back to the canonical reactor —
        # the final flourish before settle. In-place; uses only the cool family
        # (white/ice/cyan), so it never adds gold to a non-rim band; ends EXACTLY
        # on the settled art. A quick hold at peak, a single relax frame, settle.
        local surge
        for surge in 0 1 2 3; do
            printf '\033[%dA' "$rows"
            _ui_reactor_surge_frame "$surge"
            _ui_nap 0.045
        done
        printf '\033[%dA' "$rows"
        _ui_reactor_surge_frame 3                # hold the peak one extra beat
        _ui_nap 0.06
        printf '\033[%dA' "$rows"
        _ui_reactor_surge_frame 1                # relax inward
        _ui_nap 0.045

        printf '\033[%dA' "$rows"
        _ui_reactor_charge_frame 4               # settle exactly
        printf '\033[?25h'
        printf '\n'
    fi

    # CRISP HEADLINE REVEAL — a bracketed "SYSTEM ONLINE" headline that decodes in
    # (instant + centered off-tty). Cohesive cool palette: steel bracket caps + a
    # bright cyan/white headline; no gold. This is the brand completion headline
    # (a stylized state line); the FACTUAL component posture lives in the status
    # board above, which uses precise verbs and never overclaims.
    local hl_l="[" hl_r="]"
    [ "$UI_UTF8" -eq 1 ] && { hl_l="⟦"; hl_r="⟧"; }
    local headline="${hl_l} SYSTEM ONLINE ${hl_r}"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        _ui_centerline "$headline"
    elif [ "$UI_ANIM" -eq 0 ]; then
        local hsp; hsp="$(_ui_pad "${#headline}")"
        printf '%s%s%s %s%s%s %s%s%s\n' \
            "$hsp" "${UI_DIM}${UI_STEEL}" "$hl_l" \
            "${UI_BOLD}${UI_BRIGHT}" "SYSTEM ONLINE" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$hl_r" "$UI_RESET"
    else
        _ui_wordmark_decode "$headline" "${UI_BOLD}${UI_CYAN}"
    fi
    _ui_nap 0.08

    # ONLINE wordmark — decodes in on a tty (instant + centered off-tty).
    local msg="D . A . R . W . I . N .   I S   O N L I N E"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        _ui_centerline "$msg"
    else
        _ui_wordmark_decode "$msg" "${UI_BOLD}${UI_BRIGHT}"
    fi

    # NEUTRAL STATUS LINE — a clean "D.A.R.W.I.N. // ONLINE" tag in the cool
    # palette. No butler address, no time-of-day; the system does not address the
    # user.
    local status="D.A.R.W.I.N. // ONLINE"
    local ssp; ssp="$(_ui_pad "${#status}")"
    if [ "$UI_COLOR_MODE" = "none" ]; then
        printf '%s%s\n' "$ssp" "$status"
    else
        printf '%s%sD.A.R.W.I.N.%s %s//%s %s%sONLINE%s\n' \
            "$ssp" "${UI_BOLD}${UI_CYAN}" "$UI_RESET" \
            "${UI_DIM}${UI_STEEL}" "$UI_RESET" \
            "${UI_BOLD}${UI_GREEN}" "" "$UI_RESET"
    fi

    # Restated CREDIT BYLINE — "Made by <author>" (author name in gold). Reuses
    # the same byline renderer so the finale closes on the build's signature, not
    # a service address.
    ui_greet

    ui_hr
    # Seal the display with the bottom HUD frame bar.
    ui_frame_bottom "$seal"
    return 0
}
