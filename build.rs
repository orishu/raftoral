fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::var("OUT_DIR")?;
    let descriptor_path = std::path::Path::new(&out_dir).join("descriptor.bin");

    // Check if we're building for WASM
    let target = std::env::var("TARGET").unwrap_or_default();
    let is_wasm = target.starts_with("wasm32");

    if is_wasm {
        // For WASM: only generate message types (no gRPC services)
        prost_build::Config::new()
            .file_descriptor_set_path(&descriptor_path)
            .compile_protos(&["proto/raftoral.proto"], &["proto"])?;
    } else {
        // For native: generate messages + gRPC services
        tonic_prost_build::configure()
            .file_descriptor_set_path(&descriptor_path)
            .compile_protos(&["proto/raftoral.proto"], &["proto"])?;
    }

    // Tell cargo to rerun if the proto file changes
    println!("cargo:rerun-if-changed=proto/raftoral.proto");

    Ok(())
}
