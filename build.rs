fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CARGO_FEATURE_DISTRIBUTED").is_err() {
        return Ok(());
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let proto_dir = std::path::Path::new(&manifest_dir).join("proto");
    let proto_file = proto_dir.join("wsblast.proto");
    let out_dir = std::path::Path::new(&manifest_dir)
        .join("src")
        .join("distributed")
        .join("proto");

    println!("cargo:rerun-if-changed=proto/wsblast.proto");
    println!("cargo:rerun-if-changed=build.rs");

    if !proto_file.exists() {
        return Err(format!(
            "distributed feature requires protobuf definition at {}",
            proto_file.display()
        )
        .into());
    }

    std::fs::create_dir_all(&out_dir)?;
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&out_dir)
        .compile_protos(&[proto_file.as_path()], &[proto_dir.as_path()])?;

    Ok(())
}
