fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/supervisor.proto");
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .skip_debug([
            ".hudson.supervisor.v1.FileDownloadHandle",
            ".hudson.supervisor.v1.FileCaptureRequest",
            ".hudson.supervisor.v1.FileDownloadRequest",
            ".hudson.supervisor.v1.FileReleaseRequest",
            ".hudson.supervisor.v1.FileAccessObservation",
            ".hudson.supervisor.v1.LiveOutputRequest",
            ".hudson.supervisor.v1.FileWriteRequest",
            ".hudson.supervisor.v1.LiveOutputObservation",
        ])
        .extern_path(".hudson.guest.v1", "crate::guest")
        .compile_protos(&["../../proto/supervisor.proto"], &["../../proto"])?;
    println!("cargo:rerun-if-changed=../../proto/guest.proto");
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .build_client(false)
        .build_server(false)
        .skip_debug([
            ".hudson.guest.v1.Execute",
            ".hudson.guest.v1.OutputChunk",
            ".hudson.guest.v1.WriteFile",
            ".hudson.guest.v1.FileChunk",
        ])
        .compile_protos(&["../../proto/guest.proto"], &["../../proto"])?;
    Ok(())
}
