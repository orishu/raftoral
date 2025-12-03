fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let descriptor_path = std::path::Path::new(&out_dir).join("descriptor.bin");

    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&["proto/raftoral.proto", "proto/sidecar.proto"], &["proto"])?;

    // Tell cargo to rerun if the proto files change
    println!("cargo:rerun-if-changed=proto/raftoral.proto");
    println!("cargo:rerun-if-changed=proto/sidecar.proto");

    Ok(())
}
