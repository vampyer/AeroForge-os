//! Builds the C++ drivers (freestanding, C++20) into a static library and
//! links it into the kernel, plus passes the linker script.

use std::{env, path::PathBuf, process::Command};

const CXX_DRIVERS: &[&str] = &["ps2kbd/ps2kbd.cpp", "nvme/nvme.cpp", "ahci/ahci.cpp", "xhci/xhci.cpp", "e1000/e1000.cpp", "igc/igc.cpp", "btmtk/btmtk.cpp"];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let drivers = manifest.join("../drivers");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let cxx = env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let ar = env::var("AR").unwrap_or_else(|_| "llvm-ar".into());

    println!("cargo:rustc-link-arg=-T{}", manifest.join("linker.ld").display());
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/isr_stubs.s.in");
    println!("cargo:rerun-if-changed=../drivers");

    let mut objects = Vec::new();
    for src in CXX_DRIVERS {
        let src_path = drivers.join(src);
        let obj = out.join(src.replace('/', "_")).with_extension("o");
        let status = Command::new(&cxx)
            .args([
                "--target=x86_64-unknown-none-elf",
                "-std=c++20",
                "-ffreestanding",
                "-fno-exceptions",
                "-fno-rtti",
                "-fno-pic",
                "-fstack-protector-strong", "-mstack-protector-guard=global",
                "-fno-threadsafe-statics",
                "-mcmodel=kernel",
                "-mno-red-zone",
                "-mgeneral-regs-only",
                "-nostdlib",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg("-I")
            .arg(drivers.join("include"))
            .arg("-c")
            .arg(&src_path)
            .arg("-o")
            .arg(&obj)
            .status()
            .expect("failed to run the C++ compiler (set CXX to a clang++)");
        assert!(status.success(), "C++ driver failed to compile: {src}");
        objects.push(obj);
    }

    let lib = out.join("libaerodrivers.a");
    let _ = std::fs::remove_file(&lib);
    let status = Command::new(&ar).arg("crs").arg(&lib).args(&objects).status()
        .expect("failed to run llvm-ar (set AR)");
    assert!(status.success(), "archiving C++ drivers failed");

    std::fs::write(out.join("isr_stubs.s"), isr_stubs()).unwrap();

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=aerodrivers");
}

/// Interrupt entry stubs for all 256 vectors, plus the common save/restore
/// path. Generated so the vector lists can't get out of step by hand.
fn isr_stubs() -> String {
    const WITH_ERROR_CODE: [u32; 10] = [8, 10, 11, 12, 13, 14, 17, 21, 29, 30];
    let join = |v: Vec<u32>| v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let no_err = join((0..256).filter(|n| !WITH_ERROR_CODE.contains(n)).collect());
    let err = join(WITH_ERROR_CODE.to_vec());
    let all = join((0..256).collect());
    include_str!("src/isr_stubs.s.in")
        .replace("@NOERR@", &no_err)
        .replace("@ERR@", &err)
        .replace("@ALL@", &all)
}
