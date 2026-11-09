fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc: users need no protobuf compiler installed.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    let protos = [
        "proto/common.proto",
        "proto/llm.proto",
        "proto/stt.proto",
        "proto/tts.proto",
        "proto/memory.proto",
        "proto/persona.proto",
        "proto/avatar.proto",
        "proto/orchestrator.proto",
    ];
    tonic_build::configure().compile_protos(&protos, &["proto"])?;
    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    Ok(())
}
