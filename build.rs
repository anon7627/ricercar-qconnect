fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut prost_build = prost_build::Config::new();
    prost_build.compile_protos(
        &[
            "protos/qconnect_envelope.proto",
            "protos/qconnect_common.proto",
            "protos/qconnect_payload.proto",
        ],
        &["protos"],
    )?;
    Ok(())
}
