fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/supervisor.proto");
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .extern_path(".hudson.guest.v1", "crate::guest")
        .compile_protos(&["../../proto/supervisor.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/guest.proto");
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .build_client(false)
        .build_server(false)
        .skip_debug([".hudson.guest.v1.Execute", ".hudson.guest.v1.OutputChunk"])
        .compile_protos(&["../../proto/guest.proto"], &["../../proto"])?;
    Ok(())
}
