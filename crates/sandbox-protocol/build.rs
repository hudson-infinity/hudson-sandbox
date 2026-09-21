fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/supervisor.proto");
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/supervisor.proto"], &["../../proto"])?;
    Ok(())
}
