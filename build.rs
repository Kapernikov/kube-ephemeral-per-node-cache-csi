fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Include the google protobuf types from our tools directory
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &["proto/csi.proto"],
            &["proto/", "tools/include/"],
        )?;
    Ok(())
}
