fn main() {
    // rust-embed needs the folder to exist even before the UI is built.
    if let Err(err) = std::fs::create_dir_all("ui/dist") {
        println!("cargo:warning=could not create ui/dist: {err}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/dist");
}
