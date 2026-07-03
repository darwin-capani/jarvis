// Build script — compiles the Endpoint Security C shim ONLY under the
// `endpoint-security` feature. The default build never touches this (no ES link,
// no added attack surface). The shim is compiled against Apple's real ES header
// from the macOS SDK, so its es_message_t struct usage is compiler-verified;
// linking -lEndpointSecurity/-lbsm needs no entitlement (that is a RUNTIME check),
// so a feature build succeeds anywhere with the SDK — it just can't *run* the
// client without root + the restricted entitlement + notarization.
fn main() {
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
            .compile("jarvis_es_shim");
        let out = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
        println!("cargo:rustc-link-search=native={out}");
        println!("cargo:rerun-if-changed=csrc/es_shim.c");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
