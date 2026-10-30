#[test]
fn library_exposes_version() {
    assert_eq!(rustdm::VERSION, env!("CARGO_PKG_VERSION"));
}
