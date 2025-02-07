extern crate cbindgen;

use std::env;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_language(cbindgen::Language::C)
        .with_include_guard("PARABEL_H")
        .with_item_prefix("PARABEL_")
        .generate()
        .expect("Unable to generate C bindings")
        .write_to_file("target/include/parabel.h");

    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_language(cbindgen::Language::Cxx)
        .with_include_guard("PARABEL_H")
        .with_namespace("parabel")
        .generate()
        .expect("Unable to generate C++ bindings")
        .write_to_file("target/include/parabel.hpp");
}
