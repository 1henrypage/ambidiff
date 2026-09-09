//! Keep the wasm32 fixture embeds fresh: the conformance and projection
//! corpora are compiled into the wasm test binaries with `include_dir!`,
//! which cargo cannot see. Declaring the directories as rebuild inputs
//! (cargo scans a directory path recursively) makes every fixture edit
//! rebuild the crate and its tests, so no script needs to touch sources.

fn main() {
    println!("cargo:rerun-if-changed=../../fixtures/cases");
    println!("cargo:rerun-if-changed=../../fixtures/projections");
    println!("cargo:rerun-if-changed=build.rs");
}
