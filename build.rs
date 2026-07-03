fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Build-info stamping (src/build_info.rs reads these via option_env!):
    // re-link when the release pipeline changes them.
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
}
