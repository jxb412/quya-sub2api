fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(false)
        .build_server(true)
        .compile(
            &[
                "proto/sub2api/plugin.proto",
                "proto/goplugin/grpc_controller.proto",
                "proto/goplugin/grpc_stdio.proto",
                "proto/goplugin/grpc_broker.proto",
            ],
            &["proto/sub2api", "proto/goplugin"],
        )?;
    Ok(())
}
