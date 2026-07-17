// Build script — compiles the Endpoint Security C shim ONLY under the
// `endpoint-security` feature. The default build never touches this (no ES link,
// no added attack surface). The shim is compiled against Apple's real ES header
// from the macOS SDK, so its es_message_t struct usage is compiler-verified;
// linking -lEndpointSecurity/-lbsm needs no entitlement (that is a RUNTIME check),
// so a feature build succeeds anywhere with the SDK — it just can't *run* the
// client without root + the restricted entitlement + notarization.
fn main() {
    // THERMAL SHIM (hardware.vitals): compile the tiny Objective-C shim that
    // reads NSProcessInfo.thermalState. Compiled UNCONDITIONALLY on macOS —
    // Foundation is always present and links freely (unlike EndpointSecurity,
    // reading thermalState needs no entitlement/root), so the normal
    // `cargo build`/`cargo test` link it and the live thermal read wires up.
    // We let `cc` emit the static-lib link directive itself (the shim function
    // is referenced from power.rs, so the linker never drops it) and add the
    // Foundation framework link. On non-macOS targets the Rust side compiles a
    // Nominal fallback and this shim is skipped.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("csrc/thermal_shim.m")
            // Emit a CLASSIC `_objc_msgSend` call, not the newer per-selector
            // `_objc_msgSend$thermalState` stub — the classic symbol is resolved
            // by linking libobjc (declared in power.rs), so the object has no
            // dangling selector-stub reference.
            .flag_if_supported("-fno-objc-msgsend-selector-stubs")
            // Don't let cc auto-emit link directives — a build-script static lib
            // referenced only from the bin is dropped by this linker (the same
            // reason es.rs force-loads its shim). The real link (with
            // +whole-archive + the Foundation/objc dylibs) is declared via
            // #[link] in power.rs; build.rs only compiles the shim and points
            // rustc at where the archive lives.
            .cargo_metadata(false)
            .compile("darwin_thermal_shim");
        let out = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
        println!("cargo:rustc-link-search=native={out}");
        println!("cargo:rerun-if-changed=csrc/thermal_shim.m");
    }

    if std::env::var("CARGO_FEATURE_ENDPOINT_SECURITY").is_ok() {
        cc::Build::new()
            .file("csrc/es_shim.c")
            // Blocks are a clang/Darwin extension (enabled by default on macOS,
            // but requested explicitly for robustness) — es_new_client takes a block.
            .flag_if_supported("-fblocks")
            // Don't let cc auto-emit link directives — the actual link (with
            // +whole-archive + the ES/bsm dylibs in the right order) is declared via
            // #[link] attributes in daemon/src/es.rs, which forward the whole-archive
            // modifier to the linker reliably (a build-script link-lib modifier is
            // not honored the same way on this cargo). build.rs only compiles the
            // shim and points rustc at where the archive lives.
            .cargo_metadata(false)
            .compile("darwin_es_shim");
        let out = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
        println!("cargo:rustc-link-search=native={out}");
        println!("cargo:rerun-if-changed=csrc/es_shim.c");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
