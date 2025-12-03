fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::compile_protos("../proto/sidecar.proto")?;

    // Tell cargo to rerun if the proto file changes
    println!("cargo:rerun-if-changed=../proto/sidecar.proto");

    Ok(())
}
