use std::str::FromStr;

use super::NixSystem;

#[test]
fn supported_systems_have_canonical_nix_names() {
    for (name, system) in [
        ("x86_64-linux", NixSystem::X86_64Linux),
        ("aarch64-linux", NixSystem::Aarch64Linux),
        ("x86_64-darwin", NixSystem::X86_64Darwin),
        ("aarch64-darwin", NixSystem::Aarch64Darwin),
    ] {
        assert_eq!(system.as_str(), name);
        assert_eq!(system.to_string(), name);
        assert_eq!(NixSystem::from_str(name).expect("parse"), system);
    }
}

#[test]
fn unsupported_systems_are_preflight_errors() {
    let error = NixSystem::from_parts("riscv64", "linux").expect_err("unsupported");

    assert_eq!(error.message, "unsupported Nix system riscv64-linux");
}
