fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=IOKit");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rerun-if-changed=src/platform/macos_socket.c");
        cc::Build::new()
            .file("src/platform/macos_socket.c")
            .compile("ballast_socket");
    }
}
