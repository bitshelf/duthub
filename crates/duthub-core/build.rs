fn main() {
    println!("cargo::rerun-if-changed=Cargo.toml");
    let manifest = include_str!("Cargo.toml")
        .parse::<toml::Table>()
        .expect("Failed to parse the duthub-core Cargo manifest");
    let similarity = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("duthub"))
        .and_then(|duthub| duthub.get("default-boot-similarity"))
        .and_then(toml::Value::as_integer)
        .expect("Set package.metadata.duthub.default-boot-similarity to an integer");
    println!("cargo::rustc-env=DUTHUB_DEFAULT_BOOT_SIMILARITY={similarity}");
}
