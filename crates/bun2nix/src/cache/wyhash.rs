//! Bun's Wyhash 1.1 cache-key implementation.
//!
//! Ported from the MIT-licensed implementation bundled in
//! `nix-community/bun2nix`, itself copied from Bun/Zig. See `UPSTREAM.md`.

const PRIMES: [u64; 5] = [
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
    0x1d8e_4e27_c47d_124f,
];

pub(super) fn hash(input: &[u8]) -> u64 {
    let aligned_length = input.len() - input.len() % 32;
    let mut seed = 0;
    for chunk in input[..aligned_length].chunks_exact(32) {
        seed = mix0(read_u64(&chunk[..8]), read_u64(&chunk[8..16]), seed)
            ^ mix1(read_u64(&chunk[16..24]), read_u64(&chunk[24..32]), seed);
    }

    let remainder = &input[aligned_length..];
    seed = match remainder.len() {
        0 => seed,
        1..=8 => mix0(short_word(remainder), PRIMES[4], seed),
        9..=16 => mix0(
            swapped_eight(&remainder[..8]),
            short_word(&remainder[8..]),
            seed,
        ),
        17..=24 => {
            mix0(
                swapped_eight(&remainder[..8]),
                swapped_eight(&remainder[8..16]),
                seed,
            ) ^ mix1(short_word(&remainder[16..]), PRIMES[4], seed)
        }
        25..=31 => {
            mix0(
                swapped_eight(&remainder[..8]),
                swapped_eight(&remainder[8..16]),
                seed,
            ) ^ mix1(
                swapped_eight(&remainder[16..24]),
                short_word(&remainder[24..]),
                seed,
            )
        }
        _ => unreachable!("remainder is always shorter than 32 bytes"),
    };

    mum(seed ^ input.len() as u64, PRIMES[4])
}

fn short_word(bytes: &[u8]) -> u64 {
    match bytes.len() {
        1 => u64::from(bytes[0]),
        2 => read_u16(bytes),
        3 => (read_u16(bytes) << 8) | u64::from(bytes[2]),
        4 => read_u32(bytes),
        5 => (read_u32(bytes) << 8) | u64::from(bytes[4]),
        6 => (read_u32(bytes) << 16) | read_u16(&bytes[4..]),
        7 => (read_u32(bytes) << 24) | (read_u16(&bytes[4..]) << 8) | u64::from(bytes[6]),
        8 => swapped_eight(bytes),
        _ => unreachable!("short word is between one and eight bytes"),
    }
}

fn read_u16(bytes: &[u8]) -> u64 {
    u64::from(u16::from_le_bytes(
        bytes[..2].try_into().expect("two bytes"),
    ))
}

fn read_u32(bytes: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes(
        bytes[..4].try_into().expect("four bytes"),
    ))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"))
}

fn swapped_eight(bytes: &[u8]) -> u64 {
    (read_u32(bytes) << 32) | read_u32(&bytes[4..])
}

fn mum(left: u64, right: u64) -> u64 {
    let product = u128::from(left) * u128::from(right);
    let bytes = product.to_le_bytes();
    let low = u64::from_le_bytes(bytes[..8].try_into().expect("low word"));
    let high = u64::from_le_bytes(bytes[8..].try_into().expect("high word"));
    low ^ high
}

fn mix0(left: u64, right: u64, seed: u64) -> u64 {
    mum(left ^ seed ^ PRIMES[0], right ^ seed ^ PRIMES[1])
}

fn mix1(left: u64, right: u64, seed: u64) -> u64 {
    mum(left ^ seed ^ PRIMES[2], right ^ seed ^ PRIMES[3])
}

#[cfg(test)]
#[path = "wyhash_test.rs"]
mod tests;
