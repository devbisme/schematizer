//! The engine's random number generator.
//!
//! Placement and routing are randomized: parts start at random positions,
//! exactly-overlapping parts are separated by a random nudge, and the router
//! picks randomly among equally good choices. Seeding the generator makes a
//! run reproducible.

use sha1::{Digest, Sha1};

/// A xoshiro256** generator.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

impl Rng {
    /// A generator seeded from the clock, for runs that needn't be repeatable.
    pub fn from_entropy() -> Rng {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let addr = &t as *const u64 as u64;
        Rng::from_u64(t ^ addr.rotate_left(32) ^ (std::process::id() as u64) << 20)
    }

    pub fn from_u64(seed: u64) -> Rng {
        let mut st = seed;
        Rng { s: [splitmix(&mut st), splitmix(&mut st), splitmix(&mut st), splitmix(&mut st)] }
    }

    /// A generator seeded from a string.
    pub fn from_str(seed: &str) -> Rng {
        let digest = Sha1::digest(seed.as_bytes());
        let mut b = [0u8; 8];
        b.copy_from_slice(&digest[..8]);
        Rng::from_u64(u64::from_le_bytes(b))
    }

    pub fn seed(&mut self, seed: &str) {
        *self = Rng::from_str(seed);
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// A float in [0, 1).
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A float in [a, b].
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.random()
    }

    /// An integer in [a, b].
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        a + (self.next_u64() % ((b - a + 1) as u64)) as i64
    }

    /// An index below n.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    pub fn getrandbits64(&mut self) -> u64 {
        self.next_u64()
    }
}
