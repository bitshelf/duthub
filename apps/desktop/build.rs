fn main() {
    // Windows: Embed application icons (Resource ID 1 — gpui load_icon will be loaded from here)
    // along with version information.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI constructs nested settings panels on the main thread. Debug
        // builds retain intermediate layout values and need more stack reserve.
        println!("cargo:rustc-link-arg-bin=duthub=/STACK:8388608");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "duthub");
        res.set("ProductName", "Duthub");
        res.compile()
            .expect("Failed to embed Windows resources. The app.ico file must be located in the assets/ directory.");
    }
}
