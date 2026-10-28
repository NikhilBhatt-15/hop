fn main() {
    println!("cargo:rerun-if-changed=hop.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("hop.manifest");
        println!("cargo:rustc-link-arg-bin=hop=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin=hop=/MANIFESTINPUT:{}", manifest.display());
    }
}
