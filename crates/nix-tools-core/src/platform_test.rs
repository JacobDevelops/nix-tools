use super::{DefaultPlatformCheck, Platform, PlatformCheck, PlatformRequirement};
use crate::outcome::ExitCode;

#[test]
fn portable_operations_support_linux_and_macos() {
    let check = DefaultPlatformCheck;
    assert!(
        check
            .preflight(Platform::Linux, PlatformRequirement::Portable, "check")
            .is_ok()
    );
    assert!(
        check
            .preflight(Platform::MacOs, PlatformRequirement::Portable, "check")
            .is_ok()
    );
}

#[test]
fn platform_only_operations_fail_with_specific_preflight_error() {
    let error = DefaultPlatformCheck
        .preflight(Platform::MacOs, PlatformRequirement::LinuxOnly, "dev")
        .expect_err("unsupported");
    assert_eq!(error.exit_code, ExitCode::PREFLIGHT);
    assert_eq!(error.message, "dev is not supported on macos");
}
