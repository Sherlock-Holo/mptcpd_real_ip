use std::env;
use std::path::PathBuf;

use bindgen::EnumVariation;

fn main() {
    println!("cargo:rerun-if-changed=ffi.h");

    generate_require("ffi");
}

fn generate_require(file: &str) {
    let bindings = bindgen::builder()
        .layout_tests(false)
        .generate_comments(false)
        .clang_arg("-Wno-unknown-attributes")
        .default_enum_style(EnumVariation::ModuleConsts)
        .prepend_enum_name(false)
        .derive_debug(false)
        .header(format!("{file}.h"))
        .allowlist_type(".*")
        .allowlist_var(".*")
        .allowlist_function(".*")
        .size_t_is_usize(false)
        .generate_comments(true)
        .clang_arg("-fretain-comments-from-system-headers")
        .generate()
        .unwrap();

    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(format!("{file}.rs"));

    bindings.write_to_file(path).unwrap();
}
