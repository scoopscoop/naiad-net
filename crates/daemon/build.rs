//! Ensure `ui/dist` exists at compile time. `rust-embed` (see `src/ui.rs`) fails
//! to build if its `#[folder]` is missing, which a fresh checkout / CI is — so
//! write a minimal placeholder `index.html` when none is present. A real Vite
//! build (`npm --prefix ui run build`) replaces it (Vite empties the dir).

use std::env;
use std::fs;
use std::path::Path;

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <title>Naiad</title>
  </head>
  <body>
    <div id="app"></div>
    <p>
      Naiad is running, but the web UI has not been built. Run
      <code>npm --prefix ui install &amp;&amp; npm --prefix ui run build</code>
      and restart the daemon.
    </p>
  </body>
</html>
"#;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let dist = Path::new(&manifest).join("../../ui/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        fs::create_dir_all(&dist).expect("create ui/dist placeholder dir");
        fs::write(&index, PLACEHOLDER).expect("write placeholder index.html");
    }
    println!("cargo:rerun-if-changed=../../ui/dist");
}
