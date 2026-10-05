fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    for arg in ["-nostartfiles", "-nostdlib", "-static", "-no-pie"] {
        println!("cargo:rustc-link-arg-bins={arg}");
    }
    println!("cargo:rustc-link-arg-bins=-T{dir}/user.ld");
    println!("cargo:rerun-if-changed=user.ld");
}
