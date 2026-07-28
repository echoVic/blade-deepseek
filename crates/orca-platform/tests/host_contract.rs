use orca_platform::host::{Architecture, HostPlatform, OperatingSystem};

#[test]
fn current_platform_is_closed_and_serializable() {
    let host = HostPlatform::current();
    let encoded = serde_json::to_string(&host).expect("serialize host");
    let decoded: HostPlatform = serde_json::from_str(&encoded).expect("deserialize host");
    assert_eq!(decoded, host);
}

#[test]
fn supported_windows_matrix_is_explicit() {
    assert!(HostPlatform::new(OperatingSystem::Windows, Architecture::X86_64).is_supported());
    assert!(HostPlatform::new(OperatingSystem::Windows, Architecture::Aarch64).is_supported());
    assert!(
        !HostPlatform::new(
            OperatingSystem::Windows,
            Architecture::Other("x86".to_string()),
        )
        .is_supported()
    );
}

#[test]
fn unknown_hosts_remain_representable_but_unsupported() {
    let host = HostPlatform::new(
        OperatingSystem::Other("plan9".to_string()),
        Architecture::Other("mips".to_string()),
    );
    assert!(!host.is_supported());
}
