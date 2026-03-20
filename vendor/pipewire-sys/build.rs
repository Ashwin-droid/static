// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

fn main() {
    system_deps::Config::new()
        .probe()
        .expect("Cannot find libpipewire");

    println!("cargo:rerun-if-changed=src/bindings.rs");
    println!("cargo:rerun-if-changed=wrapper.h");
}
