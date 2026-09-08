#[test]
fn zero_iterations_is_rejected_before_sampling() {
    let error = super::validate_iterations(0).expect_err("zero iterations must fail");
    assert_eq!(
        error,
        "NIX_TOOLS_GRAPH_ITERATIONS must be greater than zero"
    );
}
