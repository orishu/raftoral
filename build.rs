fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .file_descriptor_set_path("target/descriptor.bin")
        .compile_protos(&["proto/raftoral.proto"], &["proto"])?;
    Ok(())
}
