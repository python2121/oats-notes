//! macOS: this crate's examples link the FluidAudio Swift static library, so they need
//! the Swift runtime dylibs from `/usr/lib/swift` at run time (the app binary gets the same
//! rpath from its own build script).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-examples=-Wl,-rpath,/usr/lib/swift");
    }
}
