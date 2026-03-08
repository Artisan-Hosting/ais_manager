fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/watchdog.proto");

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/watchdog.proto"], &["proto"])?;

    Ok(())
}
