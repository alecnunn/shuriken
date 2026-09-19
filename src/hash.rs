//! Command hashing, bit-compatible with ninja's `.ninja_log` entries.
//!
//! ninja >= 1.12 hashes command lines with rapidhash (v1, "fast"/"unrolled"
//! configuration, default seed and secrets). The build log records those
//! hashes, so an implementation that wants to share a `.ninja_log` with ninja
//! has to compute exactly the same values. This is an independent Rust
//! implementation of that algorithm.

const RAPID_SEED: u64 = 0xbdd8_9aa9_8270_4029;
const SECRET: [u64; 3] = [
    0x2d35_8dcc_aa6c_78a5,
    0x8bb8_4b93_962e_acc9,
    0x4b33_a62e_d433_d4a3,
];

#[inline(always)]
fn mum(a: &mut u64, b: &mut u64) {
    let r = (*a as u128).wrapping_mul(*b as u128);
    *a = r as u64;
    *b = (r >> 64) as u64;
}

#[inline(always)]
fn mix(mut a: u64, mut b: u64) -> u64 {
    mum(&mut a, &mut b);
    a ^ b
}

#[inline(always)]
fn read64(p: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(p[i..i + 8].try_into().unwrap())
}

#[inline(always)]
fn read32(p: &[u8], i: usize) -> u64 {
    u32::from_le_bytes(p[i..i + 4].try_into().unwrap()) as u64
}

#[inline(always)]
fn read_small(p: &[u8], k: usize) -> u64 {
    ((p[0] as u64) << 56) | ((p[k >> 1] as u64) << 32) | (p[k - 1] as u64)
}

/// Hash a command line the way ninja's build log does.
pub fn hash_command(command: &str) -> u64 {
    rapidhash(command.as_bytes())
}

/// rapidhash with ninja's default seed and secrets.
pub fn rapidhash(key: &[u8]) -> u64 {
    let len = key.len();
    let mut seed = RAPID_SEED;
    seed ^= mix(seed ^ SECRET[0], SECRET[1]) ^ (len as u64);

    let (a, b);
    if len <= 16 {
        if len >= 4 {
            let plast = len - 4;
            a = (read32(key, 0) << 32) | read32(key, plast);
            let delta = (len & 24) >> (len >> 3);
            b = (read32(key, delta) << 32) | read32(key, plast - delta);
        } else if len > 0 {
            a = read_small(key, len);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = len;
        let mut p = 0usize;
        if i > 48 {
            let mut see1 = seed;
            let mut see2 = seed;
            while i >= 96 {
                seed = mix(read64(key, p) ^ SECRET[0], read64(key, p + 8) ^ seed);
                see1 = mix(read64(key, p + 16) ^ SECRET[1], read64(key, p + 24) ^ see1);
                see2 = mix(read64(key, p + 32) ^ SECRET[2], read64(key, p + 40) ^ see2);
                seed = mix(read64(key, p + 48) ^ SECRET[0], read64(key, p + 56) ^ seed);
                see1 = mix(read64(key, p + 64) ^ SECRET[1], read64(key, p + 72) ^ see1);
                see2 = mix(read64(key, p + 80) ^ SECRET[2], read64(key, p + 88) ^ see2);
                p += 96;
                i -= 96;
            }
            if i >= 48 {
                seed = mix(read64(key, p) ^ SECRET[0], read64(key, p + 8) ^ seed);
                see1 = mix(read64(key, p + 16) ^ SECRET[1], read64(key, p + 24) ^ see1);
                see2 = mix(read64(key, p + 32) ^ SECRET[2], read64(key, p + 40) ^ see2);
                p += 48;
                i -= 48;
            }
            seed ^= see1 ^ see2;
        }
        if i > 16 {
            seed = mix(
                read64(key, p) ^ SECRET[2],
                read64(key, p + 8) ^ seed ^ SECRET[1],
            );
            if i > 32 {
                seed = mix(read64(key, p + 16) ^ SECRET[2], read64(key, p + 24) ^ seed);
            }
        }
        a = read64(key, p + i - 16);
        b = read64(key, p + i - 8);
    }

    let mut a = a ^ SECRET[1];
    let mut b = b ^ seed;
    mum(&mut a, &mut b);
    mix(a ^ SECRET[0] ^ (len as u64), b ^ SECRET[1])
}

/// A small, fast, non-cryptographic hasher for internal hash maps.
///
/// This is the FxHash algorithm (as used by rustc). It is not used for
/// anything persisted to disk, only for in-memory maps, so it is free to
/// change.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add_to_hash(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add_to_hash(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rem.len()].copy_from_slice(rem);
            self.add_to_hash(u64::from_le_bytes(buf));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add_to_hash(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // Finalize before handing the value to a hash map. Without this, the
        // low bits (which hashbrown uses to pick a bucket) carry very little
        // entropy for keys that share a prefix, such as the thousands of
        // similar paths in a large manifest, and probe chains explode.
        let mut h = self.hash;
        h ^= h >> 32;
        h = h.wrapping_mul(0xd6e8_feb8_6659_fd93);
        h ^= h >> 32;
        h
    }
}

/// [`std::hash::BuildHasher`] for [`FxHasher`].
#[derive(Default, Clone, Copy)]
pub struct FxBuildHasher;

impl std::hash::BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    #[inline]
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// A [`std::collections::HashMap`] using [`FxHasher`].
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
/// A [`std::collections::HashSet`] using [`FxHasher`].
pub type FxHashSet<T> = std::collections::HashSet<T, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ninja_recorded_hash() {
        // Captured from ninja 1.13.2's .ninja_log for this exact command.
        let cmd = r#"cp a.txt b.txt && printf "b.txt: a.txt extra.h\n" > b.txt.d"#;
        assert_eq!(format!("{:016x}", hash_command(cmd)), "c5838a4b554d43e9");
    }

    #[test]
    fn fx_hasher_spreads_similar_keys() {
        use std::hash::BuildHasher;
        // Keys that differ only in a suffix must land in different buckets:
        // this is what a manifest full of "out1234"-style paths looks like.
        let build = FxBuildHasher;
        let mut low_bits = std::collections::HashSet::new();
        for i in 0..4096u32 {
            let key = format!("out{i}");
            low_bits.insert(build.hash_one(&key) & 0xfff);
        }
        // A perfect spread would be 4096 distinct values; random hashing gives
        // about 63% of that. Anything much below signals a degenerate hasher.
        assert!(
            low_bits.len() > 2200,
            "only {} distinct buckets",
            low_bits.len()
        );
    }

    #[test]
    fn hashes_all_length_classes() {
        // Just exercise every branch; values are pinned in the integration
        // test that compares against a ninja-written log.
        for n in 0..200 {
            let s: String = std::iter::repeat_n('x', n).collect();
            let _ = hash_command(&s);
        }
    }
}
