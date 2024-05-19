//! Emits the necessary "checked" compile time flag warning for "loom".
//!
//! Without this, the compiler complains about the unspecified "loom" flag.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(loom)");
}
