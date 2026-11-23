fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // Only Windows builds get a Win32 resource block. Keeping the check here
    // lets Linux/macOS development builds and tests run without windres.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winres::WindowsResource::new();
    resource.set_icon("icons/icon.ico");
    resource.set("ProductName", "CRIS Tools Proxy");
    resource.set("FileDescription", "CRIS Tools Proxy");
    resource.set("CompanyName", "CRIS Tools");
    resource.set("OriginalFilename", "cris_tools_proxy.exe");
    resource.set("LegalCopyright", "Copyright (c) CRIS Tools contributors");
    resource.set("FileVersion", env!("CARGO_PKG_VERSION"));
    resource.set("ProductVersion", env!("CARGO_PKG_VERSION"));
    if let Err(error) = resource.compile() {
        panic!("failed to compile the Windows resource block: {error}");
    }
}
