// Compile the real build script as a module so its HEAD-advance regression
// tests run under the package's normal `cargo test` surface.
#![allow(dead_code)]

#[path = "../build.rs"]
mod build_script;
