//! Fullscreen "kiosk takeover" — the Tauri BACKEND side of the full-desktop HUD
//! mode (Phase-2, DEVICE-GATED render).
//!
//! SAFETY CONTRACT (the headline property — the user must NEVER be locked out of
//! macOS):
//!
//!   * Takeover ships OFF. Nothing here is called at startup; `tauri.conf.json`
//!     stays `fullscreen:false`. Entering is an EXPLICIT user action only.
//!   * ENTER applies exactly four window/app mutations — fullscreen, no
//!     decorations, always-on-top, and (macOS) hide-Dock|hide-menu-bar — and
//!     RECORDS each one in [`TakeoverState`].
//!   * EXIT reverses EVERY recorded mutation in the inverse order and clears the
//!     state. Exit is TOTAL and IDEMPOTENT: calling it when not in takeover is a
//!     clean no-op, and calling it twice leaves the desktop fully restored.
//!   * A RESET-ON-EXIT safety net (wired on the window `Destroyed` event + the
//!     `Drop` path in `lib.rs`) restores the default macOS presentation options
//!     when the window closes or the app quits, so Cmd+Q / force-quit ALWAYS
//!     un-hides the Dock and menu bar. macOS itself also auto-restores
//!     presentation options when the owning process dies, so even a hard crash
//!     can never permanently hide them.
//!
//! HONESTY: the real fullscreen render + the real Dock/menu-bar hide require a
//! live Tauri app on a real display and are DEVICE-GATED — they are NOT exercised
//! by any test here. The macOS `setPresentationOptions` calls are
//! `#[cfg(target_os = "macos")]` RUNTIME code, never invoked from a test. What
//! IS proven hermetically (the tests at the bottom of this file) is the
//! enter/exit STATE MACHINE, that exit reverses every tracked mutation, and that
//! enter/exit are idempotent — all via the pure [`TakeoverState`] +
//! [`TakeoverPlan`] model, with no window handle and no objc call.

use std::sync::Mutex;

/// The four window/app mutations takeover applies. Modeling them explicitly (not
/// as a bare bool) is what lets a test PROVE that exit reverses every one of them
/// and that the reversal is total — without ever touching a real window or the
/// device-gated objc presentation-options call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// `window.set_fullscreen(true)` on enter / `(false)` on exit.
    Fullscreen,
    /// `window.set_decorations(false)` on enter / `(true)` on exit.
    Decorations,
    /// `window.set_always_on_top(true)` on enter / `(false)` on exit.
    AlwaysOnTop,
    /// macOS only: hide Dock + menu bar via `NSApplication.setPresentationOptions`
    /// on enter, restore the default on exit. DEVICE-GATED at runtime.
    PresentationOptions,
}

impl Mutation {
    /// The full set of mutations a takeover applies, in the order they are
    /// applied on ENTER. EXIT walks this in reverse.
    pub const ALL: [Mutation; 4] = [
        Mutation::Fullscreen,
        Mutation::Decorations,
        Mutation::AlwaysOnTop,
        Mutation::PresentationOptions,
    ];
}

/// Explicit, idempotent takeover state. `active` is the single source of truth;
/// `applied` records exactly which mutations are currently in effect so exit can
/// reverse precisely those (and nothing else). Held in Tauri-managed state behind
/// a `Mutex` (see [`Takeover`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TakeoverState {
    active: bool,
    applied: Vec<Mutation>,
}

/// A reversible step the I/O layer should perform. The PURE planner
/// ([`TakeoverState::plan_enter`] / [`plan_exit`]) returns these; the device side
/// in `lib.rs`/this module turns each into the matching window/objc call. Keeping
/// the plan a pure value is what makes the state machine unit-testable without a
/// window handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub mutation: Mutation,
    /// The target value for this mutation: ENTER drives every mutation "on"
    /// (fullscreen=true, decorations=false-as-"on", etc.); EXIT drives every
    /// recorded mutation "off" (its default). The I/O layer maps `on` to the
    /// concrete boolean each setter wants.
    pub on: bool,
}

/// The whole transition the I/O layer must perform — an ordered, reversible list
/// of steps. Empty when the transition is a no-op (already in the target state).
pub type TakeoverPlan = Vec<Step>;

impl TakeoverState {
    /// Whether takeover is currently in effect. Public inspection API exercised by
    /// the state-machine tests; the runtime path plans via [`plan_enter`]/
    /// [`plan_exit`] instead, so this is unused outside tests in the lib build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The mutations currently in effect. Public inspection API exercised by the
    /// tests (proving exit records/clears the exact applied set); unused outside
    /// tests in the lib build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn applied(&self) -> &[Mutation] {
        &self.applied
    }

    /// Plan the ENTER transition. PURE — no I/O. When already active this returns
    /// an EMPTY plan (idempotent: a second enter does nothing). Otherwise it
    /// returns one "on" step per [`Mutation::ALL`], in apply order. This does NOT
    /// mutate state; the caller commits via [`commit_enter`] only after the I/O
    /// it represents has been driven.
    pub fn plan_enter(&self) -> TakeoverPlan {
        if self.active {
            return Vec::new();
        }
        Mutation::ALL
            .iter()
            .map(|&mutation| Step { mutation, on: true })
            .collect()
    }

    /// Plan the EXIT transition. PURE — no I/O. Reverses EXACTLY the mutations
    /// currently recorded as applied, in the INVERSE of apply order, each driven
    /// "off". When not active this returns an EMPTY plan (idempotent: exit is
    /// always safe to call). This is the headline safety planner — it guarantees
    /// every recorded mutation gets an off step.
    pub fn plan_exit(&self) -> TakeoverPlan {
        self.applied
            .iter()
            .rev()
            .map(|&mutation| Step { mutation, on: false })
            .collect()
    }

    /// Commit the ENTER once its I/O has been driven: mark active and record the
    /// applied mutations. Idempotent — committing when already active is a no-op
    /// (the recorded set is unchanged).
    pub fn commit_enter(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        self.applied = Mutation::ALL.to_vec();
    }

    /// Commit the EXIT once its reversal I/O has been driven: clear active and
    /// forget all recorded mutations. Idempotent and TOTAL — after this the state
    /// equals `TakeoverState::default()`, so no residual mutation can survive an
    /// exit.
    pub fn commit_exit(&mut self) {
        self.active = false;
        self.applied.clear();
    }
}

/// Tauri-managed wrapper: the takeover state behind a `Mutex` so the enter/exit
/// commands (async commands, which run on Tauri's async runtime — NOT the main
/// thread; the AppKit step is dispatched to the main thread in `lib.rs`) and the
/// reset-on-exit safety net can share one source of truth.
#[derive(Default)]
pub struct Takeover {
    pub state: Mutex<TakeoverState>,
}

/* --------------------------------------------- macOS presentation options (gated) */

/// Apply the macOS kiosk presentation options (hide Dock + hide menu bar) to the
/// shared application. DEVICE-GATED: this drives the real AppKit call and is NEVER
/// invoked from a test.
///
/// MAIN-THREAD REQUIRED: AppKit only allows `setPresentationOptions` from the
/// main thread, and this function REFUSES (returns `Err`) anywhere else rather
/// than risk UB. Callers off the main thread — notably the async `enter_takeover`
/// / `exit_takeover` commands, which run on Tauri's async runtime — must hop over
/// via `run_on_main_thread` (see `set_kiosk_presentation_on_main` in `lib.rs`).
/// On non-macOS this is a compile-time no-op.
#[cfg(target_os = "macos")]
pub fn macos_set_kiosk_presentation(on: bool) -> Result<(), String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationPresentationOptions};

    // AppKit insists this happen on the main thread. `MainThreadMarker::new()`
    // returns None off-main; we refuse rather than provoke UB. NOTE: async Tauri
    // commands do NOT run on the main thread — they must dispatch here via
    // `run_on_main_thread` (lib.rs does), or this guard fails their kiosk step.
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("presentation options must be set on the main thread".to_string());
    };
    let app = NSApplication::sharedApplication(mtm);
    let options = if on {
        NSApplicationPresentationOptions::HideDock | NSApplicationPresentationOptions::HideMenuBar
    } else {
        // The DEFAULT presentation — Dock and menu bar both visible/normal.
        NSApplicationPresentationOptions::empty()
    };
    app.setPresentationOptions(options);
    Ok(())
}

/// Non-macOS no-op so the enter/exit logic compiles everywhere. There is no Dock
/// or menu bar to hide off macOS; the window fullscreen/decorations carry the
/// kiosk effect there.
#[cfg(not(target_os = "macos"))]
pub fn macos_set_kiosk_presentation(_on: bool) -> Result<(), String> {
    Ok(())
}

/// The RESET-ON-EXIT safety net, callable from the window `Destroyed` event and
/// the app `Drop`/exit path: unconditionally restore the DEFAULT macOS
/// presentation options so quitting or closing the window always un-hides the
/// Dock and menu bar — even if the webview hung or the user never pressed an
/// in-HUD exit. Best-effort and infallible from the caller's view (it swallows a
/// non-main-thread refusal; macOS also auto-restores on process death). MAIN
/// THREAD: the `Destroyed`/`RunEvent::Exit` call sites already run on the main
/// thread and call this directly; the async takeover commands instead dispatch it
/// via `run_on_main_thread` so the AppKit call actually lands. On non-macOS this
/// is a no-op.
pub fn reset_presentation_to_default() {
    #[cfg(target_os = "macos")]
    {
        let _ = macos_set_kiosk_presentation(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ENTER from the default (inactive) state plans every mutation, in apply
    /// order, each "on".
    #[test]
    fn enter_plan_covers_all_mutations_in_order() {
        let st = TakeoverState::default();
        let plan = st.plan_enter();
        assert_eq!(plan.len(), Mutation::ALL.len());
        for (step, expected) in plan.iter().zip(Mutation::ALL.iter()) {
            assert_eq!(step.mutation, *expected, "enter applies in canonical order");
            assert!(step.on, "every enter step drives the mutation on");
        }
    }

    /// Committing ENTER flips active and records the full applied set.
    #[test]
    fn commit_enter_records_every_applied_mutation() {
        let mut st = TakeoverState::default();
        assert!(!st.is_active());
        st.commit_enter();
        assert!(st.is_active());
        assert_eq!(st.applied(), &Mutation::ALL);
    }

    /// EXIT reverses EVERY mutation that enter applied — in inverse order, each
    /// driven off. This is the headline safety property: nothing the user can do
    /// leaves a tracked mutation un-reversed.
    #[test]
    fn exit_reverses_every_tracked_mutation_in_inverse_order() {
        let mut st = TakeoverState::default();
        st.commit_enter();

        let plan = st.plan_exit();
        assert_eq!(
            plan.len(),
            Mutation::ALL.len(),
            "exit must reverse exactly the mutations enter applied"
        );

        // Inverse order: the last-applied mutation is reversed first.
        let mut expected: Vec<Mutation> = Mutation::ALL.to_vec();
        expected.reverse();
        for (step, want) in plan.iter().zip(expected.iter()) {
            assert_eq!(step.mutation, *want, "exit walks mutations in reverse");
            assert!(!step.on, "every exit step drives the mutation off");
        }

        // Every mutation kind appears in the exit plan — none is forgotten.
        for m in Mutation::ALL {
            assert!(
                plan.iter().any(|s| s.mutation == m && !s.on),
                "exit must drive {m:?} off — no mutation may survive an exit"
            );
        }
    }

    /// A full enter->commit->exit->commit cycle returns the state to default:
    /// totally restored, no residual mutation.
    #[test]
    fn exit_is_total_state_returns_to_default() {
        let mut st = TakeoverState::default();
        st.commit_enter();
        assert!(st.is_active());
        st.commit_exit();
        assert_eq!(st, TakeoverState::default(), "exit fully restores the state");
        assert!(!st.is_active());
        assert!(st.applied().is_empty());
    }

    /// ENTER is idempotent: a second enter while already active plans nothing and
    /// does not change the recorded set (no double-apply).
    #[test]
    fn enter_is_idempotent() {
        let mut st = TakeoverState::default();
        st.commit_enter();
        let snapshot = st.clone();
        assert!(st.plan_enter().is_empty(), "second enter is a no-op plan");
        st.commit_enter();
        assert_eq!(st, snapshot, "committing enter twice does not change state");
    }

    /// EXIT is idempotent and always safe: exiting when NOT active plans nothing
    /// and leaves the (already-clean) state untouched — the user can always call
    /// exit without harm.
    #[test]
    fn exit_when_inactive_is_a_safe_noop() {
        let mut st = TakeoverState::default();
        assert!(st.plan_exit().is_empty(), "exit while inactive plans nothing");
        st.commit_exit();
        assert_eq!(st, TakeoverState::default());
    }

    /// EXIT after ENTER, then EXIT again: the second exit is a clean no-op. Proves
    /// double-exit can never re-mutate or wedge the desktop.
    #[test]
    fn double_exit_is_clean() {
        let mut st = TakeoverState::default();
        st.commit_enter();
        st.commit_exit();
        // Second exit: nothing left to reverse.
        assert!(st.plan_exit().is_empty());
        st.commit_exit();
        assert_eq!(st, TakeoverState::default());
    }

    /// The managed wrapper shares one state behind the mutex (enter then read back
    /// active through the lock). No window/objc calls — pure state-machine check.
    #[test]
    fn managed_takeover_shares_one_state() {
        let takeover = Takeover::default();
        {
            let mut guard = takeover.state.lock().unwrap();
            assert!(!guard.is_active());
            guard.commit_enter();
        }
        {
            let guard = takeover.state.lock().unwrap();
            assert!(guard.is_active(), "the committed enter is visible through the mutex");
            assert_eq!(guard.applied(), &Mutation::ALL);
        }
    }
}
