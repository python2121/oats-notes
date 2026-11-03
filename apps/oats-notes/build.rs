//! macOS: the FluidAudio engine is a Swift static library, so the binary needs the Swift
//! runtime dylibs (`libswift_Concurrency` and friends) from `/usr/lib/swift` at run time.
//! Without this rpath dyld refuses to start the binary ("no LC_RPATH's found").
//!
//! The bare binary also carries `Info.plist` embedded in its `__TEXT,__info_plist` section:
//! macOS reads it as the executable's bundle information, so the application menu is titled
//! "OatsNotes" (AppKit uses `CFBundleName`; without it the menu says `oats-notes`) and the
//! microphone / system-audio usage descriptions are there for the day it is bundled.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
