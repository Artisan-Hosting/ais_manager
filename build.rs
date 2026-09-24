fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/watchdog.proto");
    println!("cargo:rerun-if-changed=proto/secret.proto");
    println!("cargo:rerun-if-changed=../ais_proto/accounts.proto");

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &["proto/watchdog.proto", "../ais_proto/secret.proto", "../ais_proto/accounts.proto"],
            &["proto", "../ais_proto"],
        )?;

    Ok(())
}
