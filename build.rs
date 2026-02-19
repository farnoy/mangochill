fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.starts_with("wasm32") {
        capnpc::CompilerCommand::new()
            .file("mangochill.capnp")
            .run()?;
    }
    Ok(())
}
