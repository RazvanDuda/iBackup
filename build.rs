// These values change between builds; make sure the resource is rebuilt too.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/ibackup.ico");
    embed_windows_icon();
}

// Windows: ibackup.rs shows in the Explorer with the project icon.
// build.rs is compiled for the *host*, so `#[cfg(windows)]` here fires exactly
// when building *on* Windows - cross-compile hosts just skip it.
#[cfg(windows)]
fn embed_windows_icon() {
    winresource::WindowsResource::new()
        // 1048 = a random smallish number; nothing else uses it in this project.
        .set_icon("assets/ibackup.ico")
        .set("FileDescription", "iPhone backup to this computer")
        .set("ProductName", "ibackup")
        .compile()
        .expect("embedding Windows icon/resource");
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
