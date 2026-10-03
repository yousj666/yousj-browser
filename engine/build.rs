//! Yousj browser engine — build script.
//! Compiles the C layer (c/yousj_str.c) and links it into the cdylib.

fn main() {
    cc::Build::new()
        .file("../c/yousj_str.c")
        .include("../c")
        .compile("yousj_str");
}
