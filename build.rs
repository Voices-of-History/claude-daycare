// Bake the Rust target triple into the binary so `daycare-runner update` can
// pick its own entry from the per-target release pointer.
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=DAYCARE_RUNNER_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
