//! Gives the Windows programs (rdm, rdmd, rdm-nmhost) the rustdm icon and the
//! name Explorer and Task Manager show. Other systems build without it.

const ICON: &str = "assets/icon/rustdm.ico";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(ICON)
        .set("ProductName", "rustdm")
        .set("FileDescription", "rustdm download manager");
    if let Err(e) = resource.compile() {
        panic!("cannot embed the rustdm icon from {ICON}: {e}");
    }
}
