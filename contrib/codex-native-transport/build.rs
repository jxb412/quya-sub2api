fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    tonic_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_protos(
            &[
                "proto/sub2api/plugin.proto",
                "proto/goplugin/grpc_controller.proto",
                "proto/goplugin/grpc_stdio.proto",
                "proto/goplugin/grpc_broker.proto",
            ],
            &["proto/sub2api", "proto/goplugin"],
        )?;
    println!("cargo:rerun-if-changed=proto/sub2api/plugin.proto");
    println!("cargo:rerun-if-changed=proto/goplugin/grpc_controller.proto");
    println!("cargo:rerun-if-changed=proto/goplugin/grpc_stdio.proto");
    println!("cargo:rerun-if-changed=proto/goplugin/grpc_broker.proto");
    Ok(())
}
