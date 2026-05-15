use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto");
    let files = [
        "boi/workspace/v1/workspace.proto",
        "boi/pool/v1/pool.proto",
        "boi/router/v1/router.proto",
        "boi/provisioner/v1/provisioner.proto",
        "boi/hooks/v1/hooks.proto",
        "boi/cluster/v1/cluster.proto",
    ];
    let paths: Vec<PathBuf> = files.iter().map(|f| proto_root.join(f)).collect();
    for p in &paths {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&paths, &[proto_root])?;
    Ok(())
}
