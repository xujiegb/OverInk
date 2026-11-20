fn main() {
    println!("cargo:rerun-if-changed=src/native/macos/direct_touch.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // Only the Cocoa adapter needs Objective-C. The application remains Rust;
    // the bridge passes scalar finger samples, never native object pointers.
    cc::Build::new()
        .file("src/native/macos/direct_touch.m")
        .flag("-fobjc-arc")
        .compile("overink_direct_touch");
    println!("cargo:rustc-link-lib=framework=AppKit");
}
