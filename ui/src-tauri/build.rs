use std::{env, fs, path::PathBuf};

/// Commands defined by this app (mirrors `generate_handler!` in `lib.rs`).
/// Declaring them autogenerates `allow-<command>` permissions that
/// `capabilities/default.json` grants. Without an app manifest Tauri denies
/// every app command on the daemon-served remote origin the main window loads
/// from, while still allowing them on the local loading page.
const APP_COMMANDS: &[&str] = &[
    "daemon_state",
    "load_view_state",
    "save_inspector_collapsed",
    "save_tile",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS)),
    )
    .expect("tauri-build");
    stage_sidecar();
}

/// Copy the workspace-built `naiad` binary to `binaries/naiad-<target-triple>`
/// (with the platform exe suffix) so Tauri can bundle it as a sidecar. The
/// workspace `target/<profile>/` dir is two levels up from this crate
/// (`ui/src-tauri/` -> repo root) because the shell crate is excluded from the
/// workspace and shares the repo-root target only by relative path.
fn stage_sidecar() {
    let triple = env::var("TARGET").expect("TARGET set by cargo");
    let profile = env::var("PROFILE").expect("PROFILE set by cargo"); // debug | release
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let exe = if cfg!(windows) { ".exe" } else { "" };

    let src = manifest
        .join("..")
        .join("..")
        .join("target")
        .join(&profile)
        .join(format!("naiad{exe}"));
    let bin_dir = manifest.join("binaries");
    let dst = bin_dir.join(format!("naiad-{triple}{exe}"));

    fs::create_dir_all(&bin_dir).expect("create binaries/ dir");
    if src.exists() {
        fs::copy(&src, &dst).expect("copy naiad sidecar");
    } else {
        println!(
            "cargo:warning=naiad sidecar not found at {} — run `cargo build` (or `cargo build --release`) at the repo root first",
            src.display()
        );
    }
    println!("cargo:rerun-if-changed={}", src.display());
}
