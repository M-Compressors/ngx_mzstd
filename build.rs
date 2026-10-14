//! Build script.
//!
//! Compiles the C bit-field accessors (`src/bitfields.c`) against the NGINX
//! headers that `nginx-sys` generated its bindings from. `nginx-sys` must be a
//! direct dependency for its `DEP_NGINX_*` metadata to reach this script:
//! <https://github.com/rust-lang/cargo/issues/3544>.

use std::env;

fn main() {
    println!("cargo::rerun-if-changed=src/bitfields.c");
    println!("cargo::rerun-if-env-changed=DEP_NGINX_INCLUDE");

    let includes = env::var_os("DEP_NGINX_INCLUDE").expect("nginx-sys exports DEP_NGINX_INCLUDE");

    cc::Build::new()
        .file("src/bitfields.c")
        .includes(env::split_paths(&includes))
        // NGINX headers are not warning-clean under every compiler's defaults.
        .warnings(false)
        .compile("ngx_mzstd_bitfields");

    // `ngx_feature = "..."` for every feature NGINX was configured with, as
    // ngx-rust does for itself: `threads` enables `zstd_thread_pool`.
    println!("cargo::rerun-if-env-changed=DEP_NGINX_FEATURES_CHECK");
    println!(
        "cargo::rustc-check-cfg=cfg(ngx_feature, values({}))",
        env::var("DEP_NGINX_FEATURES_CHECK").unwrap_or_else(|_| "any()".to_owned())
    );
    println!("cargo::rerun-if-env-changed=DEP_NGINX_FEATURES");
    if let Ok(features) = env::var("DEP_NGINX_FEATURES") {
        for feature in features.split(',').map(str::trim) {
            println!("cargo::rustc-cfg=ngx_feature=\"{feature}\"");
        }
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // NGINX symbols are resolved when the module is loaded.
        println!("cargo::rustc-link-arg=-undefined");
        println!("cargo::rustc-link-arg=dynamic_lookup");
    }
}
