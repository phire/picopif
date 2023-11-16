
use std::env;

fn main() {
    println!("cargo:rustc-link-arg-bins=--nmagic");

    if cfg!(feature = "run-from-ram") {
        if cfg!(feature = "copy-to-ram") {
            println!("cargo:rustc-link-arg-bins=-Tlink-ram-at-flash.x");
            println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
        } else {
            println!("cargo:rustc-link-arg-bins=-Tlink-ram.x");
        }
    } else {
        println!("cargo:rustc-link-arg-bins=-Tlink.x");
        println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    }
    if env::var("CARGO_FEATURE_DEFMT").is_ok() {
        println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    }
    println!("cargo:rustc-link-arg-bins=--build-id");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Tlink-ram.x");
    println!("cargo:rerun-if-changed=memory.x");
}
