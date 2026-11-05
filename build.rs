use std::env;

fn main() {
    println!("cargo:rerun-if-changed=assets/MATE.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // CARGO_CFG_TARGET_OS names the target, not the host, so cross builds embed it too.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/MATE.ico");
        resource.set(
            "FileDescription",
            "MATE - MQTT Administration & Terminal Environment",
        );
        resource.set("ProductName", "MATE");
        if let Err(error) = resource.compile() {
            panic!("could not embed the Windows icon: {error}");
        }
    }
}
