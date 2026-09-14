use super::*;

#[test]
fn all_outputs_is_limited_to_realization_requests() {
    let mut request: Request = serde_json::from_str(r#"{"type":"request","version":1,"id":"1","operation":"build_installables","attribute_paths":[["legacyPackages","x86_64-linux","ci"]],"all_outputs":true,"config":{"system":"x86_64-linux"},"flake":{"reference":"."}}"#).unwrap();
    request.validate().unwrap();
    assert!(request.engine_config().unwrap().all_outputs);
    request.operation = Operation::PrepareRun;
    request.attribute_paths.clear();
    request.app = Some("dev".into());
    assert!(request.validate().is_err());
}

#[test]
fn presentation_accepts_existing_modes_and_rejects_unknown_modes() {
    let valid = r#"{"type":"request","version":1,"id":"1","operation":"discover","config":{"system":"x86_64-linux"},"flake":{"reference":"."},"presentation":{"mode":"tui","title":"jfit build"}}"#;
    let request: Request = serde_json::from_str(valid).unwrap();
    assert!(request.presentation.is_some());
    assert!(serde_json::from_str::<Request>(&valid.replace("tui", "custom")).is_err());
}

#[test]
fn shared_go_golden_discovery_contract_matches_rust() {
    let request: Request = serde_json::from_str(include_str!(
        "../../../sdk/go/testdata/discover-request.json"
    ))
    .unwrap();
    request.validate().unwrap();
    assert!(request.operation == Operation::Discover);
    let result = ResultPayload::Discover {
        discovery: nix_tools_engine::DiscoveredTargets {
            packages: vec!["cli".into()],
            checks: vec!["lint".into()],
            apps: vec!["dev".into()],
        },
    };
    let encoded = encode_response(
        &ResultMessage {
            message_type: "result",
            version: VERSION,
            id: "1",
            result: &result,
            signal: None,
        },
        DEFAULT_RESPONSE_BYTES,
    )
    .unwrap();
    let expected: Value = serde_json::from_str(include_str!(
        "../../../sdk/go/testdata/discover-result.json"
    ))
    .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), expected);
}

#[test]
fn full_flake_check_uses_configured_system_and_validated_trust() {
    struct Runner;
    impl ProcessRunner for Runner {
        fn run(
            &self,
            spec: &ProcessSpec,
            _: &Cancellation,
        ) -> Result<nix_tools_core::process::ProcessResult, Error> {
            if spec.args[0] == "flake" {
                assert!(spec.args.contains(&"--no-build".into()));
                assert!(
                    spec.args
                        .windows(3)
                        .any(|args| args == ["--option", "system", "aarch64-darwin"])
                );
                assert!(
                    spec.env[std::ffi::OsStr::new("NIX_CONFIG")]
                        .to_str()
                        .unwrap()
                        .contains("accept-flake-config = false")
                );
                assert_eq!(spec.env.len(), 1);
            }
            Ok(nix_tools_core::process::ProcessResult {
                termination: nix_tools_core::process::ChildTermination::Exited(0),
                stdout: nix_tools_core::process::CapturedStream {
                    bytes: br#"{"exceeded":false,"attempts":[]}"#.to_vec(),
                    truncated: false,
                },
                stderr: nix_tools_core::process::CapturedStream::default(),
                combined: None,
                duration: Duration::ZERO,
            })
        }
    }
    let request = serde_json::from_str(r#"{"type":"request","version":1,"id":"1","operation":"flake_check","config":{"system":"aarch64-darwin"},"flake":{"reference":"."}}"#).unwrap();
    let output = Arc::new(Output {
        writer: Mutex::new(io::stdout()),
        cancellation: Cancellation::default(),
        id: "1".into(),
        max_response_bytes: DEFAULT_RESPONSE_BYTES,
        failure: Mutex::new(None),
        ui: Mutex::new(None),
    });
    assert!(matches!(
        execute(&request, &Runner, &output).unwrap(),
        ResultPayload::FlakeCheck { exit_code: 0, .. }
    ));
}

#[test]
fn response_limit_counts_newline_and_never_emits_partial_json() {
    assert!(encode_response(&"long response", 4).is_err());
    assert_eq!(encode_response(&"x", 4).unwrap(), b"\"x\"\n");
    assert!(encode_response(&"x", 3).is_err());
}

#[test]
fn rejects_oversized_and_unterminated_frames() {
    assert!(read_frame(&mut &b"{}"[..]).is_err());
    assert!(read_frame(&mut vec![b'x'; MAX_REQUEST_BYTES + 1].as_slice()).is_err());
}

#[test]
fn rejects_incompatible_version_and_operation_fields() {
    let mut request: Request = serde_json::from_str(r#"{"type":"request","version":2,"id":"1","operation":"discover","config":{"system":"x86_64-linux"},"flake":{"reference":"."}}"#).unwrap();
    assert!(request.validate().is_err());
    request.version = VERSION;
    assert!(request.validate().is_ok());
    request.targets.push("bad".into());
    assert!(request.validate().is_err());
}

#[test]
fn error_envelope_retains_category_status_and_signal() {
    let value = error_envelope("1", &Error::cancelled(15, "cancelled"), Some(15));
    assert_eq!(value["error"]["category"], "cancelled");
    assert_eq!(value["error"]["exit_code"], 143);
    assert_eq!(value["error"]["signal"], 15);
}

#[test]
fn cancellation_rejects_wrong_ids_and_malformed_messages() {
    assert_eq!(
        read_control(&mut &b""[..], "1").unwrap_err().kind,
        ErrorKind::Cancelled
    );
    assert_eq!(
        read_control(&mut &b"wat\n"[..], "1").unwrap_err().kind,
        ErrorKind::Usage
    );
    assert!(
        read_control(
            &mut &br#"{"type":"cancel","version":1,"id":"wrong","signal":2}
"#[..],
            "1"
        )
        .is_err()
    );
    assert_eq!(
        read_control(
            &mut &br#"{"type":"cancel","version":1,"id":"1","signal":2}
"#[..],
            "1"
        )
        .unwrap(),
        2
    );
}
