use crate::{CheckSelector, ServiceCheckSelector, ServiceTarget};
use nix_tools_core::outcome::ErrorKind;

#[test]
fn targets_preserve_unambiguous_output_names() {
    for (input, service, job, output) in [
        ("web:dev", "web", "dev", "web:dev"),
        ("api:gen", "api", "gen", "api:gen"),
        (
            "public-api:type-check",
            "public-api",
            "type-check",
            "public-api:type-check",
        ),
        ("Web_2:lint.v2", "Web_2", "lint.v2", "Web_2:lint.v2"),
    ] {
        let target: ServiceTarget = input.parse().unwrap();
        assert_eq!(target.service(), service);
        assert_eq!(target.job(), job);
        assert_eq!(target.output_name(), output);
        assert_eq!(target.to_string(), input);
    }
}

#[test]
fn run_requires_exactly_two_valid_components() {
    for input in [
        "",
        "web",
        ":dev",
        "web:",
        "web:dev:extra",
        " web:dev",
        "web:dev ",
        "../web:dev",
        "web:a/b",
        "web:é",
        "web:-dev",
    ] {
        assert_eq!(
            input.parse::<ServiceTarget>().unwrap_err().kind,
            ErrorKind::Usage,
            "{input}"
        );
    }
}

#[test]
fn checks_select_all_service_jobs_or_one_exact_job() {
    let checks = [
        "web:type-check",
        "api:lint",
        "web:lint",
        "web:lint",
        "web-worker:lint",
        "web-lint",
    ]
    .map(str::to_owned);
    assert_eq!(
        ServiceCheckSelector.select("web", &checks).unwrap(),
        ["web:lint", "web:type-check"]
    );
    assert_eq!(
        ServiceCheckSelector
            .select("web:type-check", &checks)
            .unwrap(),
        ["web:type-check"]
    );
    for input in ["missing", "web:missing"] {
        assert_eq!(
            ServiceCheckSelector
                .select(input, &checks)
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
    }
    for input in ["", ":lint", "web:", "web:lint:extra", "web/", " web"] {
        assert_eq!(
            ServiceCheckSelector
                .select(input, &checks)
                .unwrap_err()
                .kind,
            ErrorKind::Usage
        );
    }
    assert_eq!(
        ServiceCheckSelector.select("web", &[]).unwrap_err().kind,
        ErrorKind::NotFound
    );
}
