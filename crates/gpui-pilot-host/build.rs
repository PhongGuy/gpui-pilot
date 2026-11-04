//! Exports the profile's optimisation level so the host can warn when it runs unoptimised.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=GPUI_PILOT_HOST_OPT_LEVEL={}",
        std::env::var("OPT_LEVEL").unwrap_or_default()
    );
}
