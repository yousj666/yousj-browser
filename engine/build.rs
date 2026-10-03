//! Yousj browser engine — build script.
//! Compiles the C layer (c/yousj_str.c) and links it into the cdylib.

fn main() {
    cc::Build::new()
        .file("../c/yousj_str.c")
        .include("../c")
        .compile("yousj_str");

    // Friendly hint: the `js` feature needs yousj_js via RUSTFLAGS --extern
    // (see ./build-js.sh). A bare `cargo build --features js` fails later at
    // `use yousj_js` without it.
    if std::env::var("CARGO_FEATURE_JS").is_ok() {
        println!("cargo:warning=feature `js` on: ensure RUSTFLAGS contains --extern yousj_js=<...>/libyousj_js.rlib (use ./build-js.sh)");
    }
}
