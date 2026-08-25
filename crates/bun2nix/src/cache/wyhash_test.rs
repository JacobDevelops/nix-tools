use super::hash;

#[test]
fn matches_buns_short_and_multi_block_hash_vectors() {
    let cases = [
        ("beta.1", 0xc073_4e93_69ab_610d),
        ("build.123", 0xf48f_05ed_5aab_c3a0),
        ("beta.9", 0x73c5_c463_24e7_8b9b),
        (
            "https://registry.npmjs.org/zod/-/zod-3.21.4.tgz",
            0x3be0_2e19_198e_30ee,
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(hash(input.as_bytes()), expected);
    }
}
