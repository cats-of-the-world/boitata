//! Ensure the embedded-assets folder exists before `rust-embed` reads it.
//!
//! The web UI is built by Vite into `frontend/dist` and embedded at compile
//! time. On a fresh checkout (or in CI without a Node build) that folder may not
//! exist yet, which would make the `#[derive(RustEmbed)]` macro fail. Creating an
//! empty folder lets the backend compile and run without Node — it just serves a
//! "build the UI" hint until `npm run build` populates the folder.

use std::path::Path;

fn main() {
    let dist = Path::new("frontend/dist");
    if !dist.exists() {
        if let Err(e) = std::fs::create_dir_all(dist) {
            // Surface the real cause; otherwise RustEmbed fails later with an
            // opaque "folder does not exist" error.
            println!("cargo:warning=could not create {}: {e}", dist.display());
        }
    }
    // Re-embed when the built assets change.
    println!("cargo:rerun-if-changed=frontend/dist");
}
