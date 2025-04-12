use std::process::Command;
use std::{fs, path::Path};

fn main() {
    // Auto-generate vmlinux.h
    let vmlinux_path = Path::new("src/ebpf/vmlinux.h");

    if !vmlinux_path.exists() {
        println!("cargo:warning=vmlinux.h not found, generating...");
        if which::which("bpftool").is_ok() {
            let status = Command::new("bpftool")
                .args(&["btf", "dump", "file", "/sys/kernel/btf/vmlinux", "format", "c"])
                .output()
                .expect("failed to run bpftool");

            if status.status.success() {
                fs::write(vmlinux_path, status.stdout).expect("failed to write vmlinux.h");
                println!("cargo:warning=vmlinux.h generated successfully");
            } else {
                panic!("bpftool failed: {}", String::from_utf8_lossy(&status.stderr));
            }
        } else {
            panic!("bpftool not found. Please install it to generate vmlinux.h");
        }
    }

    // Build eBPF C program
    let status = Command::new("clang")
        .args(&[
            "-O2", "-g",
            "-target", "bpf",
            "-D__TARGET_ARCH_x86",
            "-I", "src/ebpf",
            "-c", "src/ebpf/network.c",
            "-o", "src/ebpf/network.o",
            "-Wall",
            "-Wno-unused",
            "-Wno-unused-function",
            "-Wno-address-of-packed-member",
            "-Wno-pointer-sign",
            "-Wno-compare-distinct-pointer-types",
            "-Wno-tautological-compare",
            "-fno-stack-protector",
            "-fno-builtin",
        ])
        .status()
        .expect("failed to compile eBPF program");

    assert!(status.success(), "eBPF program compilation failed");

    println!("cargo:rerun-if-changed=src/ebpf/network.c");
    println!("cargo:rerun-if-changed=src/ebpf/vmlinux.h");
}
