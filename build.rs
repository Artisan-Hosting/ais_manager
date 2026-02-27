use std::process::Command;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const VMLINUX_ENV_VARS: [&str; 2] = ["VMLINUX_H_SOURCE", "VMLINUX_H"];

fn copy_if_different(src: &Path, dst: &Path) -> std::io::Result<bool> {
    let source_bytes = fs::read(src)?;
    let destination_bytes = fs::read(dst).ok();

    if destination_bytes.as_deref() == Some(source_bytes.as_slice()) {
        return Ok(false);
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(dst, source_bytes)?;
    Ok(true)
}

fn vmlinux_source_from_env() -> Option<(String, PathBuf)> {
    VMLINUX_ENV_VARS.iter().find_map(|key| {
        env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| (key.to_string(), PathBuf::from(value)))
    })
}

fn generate_vmlinux(vmlinux_path: &Path) {
    println!("cargo:warning=vmlinux.h not found, generating...");
    if which::which("bpftool").is_ok() {
        let status = Command::new("bpftool")
            .args(&[
                "btf",
                "dump",
                "file",
                "/sys/kernel/btf/vmlinux",
                "format",
                "c",
            ])
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

fn ensure_vmlinux(vmlinux_path: &Path) {
    for env_var in VMLINUX_ENV_VARS {
        println!("cargo:rerun-if-env-changed={env_var}");
    }

    if let Some((source_var, source_path)) = vmlinux_source_from_env() {
        if !source_path.exists() {
            panic!(
                "{source_var} was set to '{}', but that file does not exist",
                source_path.display()
            );
        }

        println!("cargo:rerun-if-changed={}", source_path.display());
        let copied = copy_if_different(&source_path, vmlinux_path)
            .expect("failed to copy vmlinux.h from configured source");

        if copied {
            println!(
                "cargo:warning=Copied vmlinux.h from {}",
                source_path.display()
            );
        }
        return;
    }

    if !vmlinux_path.exists() {
        generate_vmlinux(vmlinux_path);
    }
}

fn main() {
    let vmlinux_path = Path::new("src/ebpf/vmlinux.h");
    ensure_vmlinux(vmlinux_path);

    // Build eBPF C program
    let status = Command::new("clang")
        .args(&[
            "-O2",
            "-g",
            "-target",
            "bpf",
            "-D__TARGET_ARCH_x86",
            "-I",
            "src/ebpf",
            "-c",
            "src/ebpf/network.c",
            "-o",
            "src/ebpf/network.o",
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
