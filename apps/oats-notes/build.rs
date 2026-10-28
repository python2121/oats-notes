//! macOS: the FluidAudio engine is a Swift static library, so the binary needs the Swift
//! runtime dylibs (`libswift_Concurrency` and friends) from `/usr/lib/swift` at run time.
//! Without this rpath dyld refuses to start the binary ("no LC_RPATH's found").
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
}
