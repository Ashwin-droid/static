// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

fn main() {
    let libs = system_deps::Config::new()
        .probe()
        .expect("Cannot find libraries");

    println!("cargo:rerun-if-changed=src/bindings.rs");
    println!("cargo:rerun-if-changed=src/static_fns.c");
    println!("cargo:rerun-if-changed=src/type-info.c");
    println!("cargo:rerun-if-changed=wrapper.h");

    let mut cc = cc::Build::new();
    cc.files(["src/type-info.c", "src/static_fns.c"]);
    cc.include(env!("CARGO_MANIFEST_DIR"));
    cc.includes(libs.all_include_paths());
    cc.flag_if_supported("-Wno-unused-parameter");

    #[cfg(feature = "v0_3_65")]
    cc.define("FEATURE_0_3_65", "1");

    cc.compile("libspa-rs-reexports");
}
