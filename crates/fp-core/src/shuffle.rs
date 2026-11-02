//! Stable seeded order for browsing and queue shuffles.

/// Where `location` falls in shuffle `seed`: the same while the seed is.
pub fn key(seed: u64, location: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325 ^ seed;
    for b in location.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    // splitmix64's finish, so nearby seeds give unrelated orders.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}
