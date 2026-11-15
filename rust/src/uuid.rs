//! Deterministic (version 5) UUIDs, so the same drawing gets the same IDs.

use sha1::{Digest, Sha1};

// The namespace SKiDL uses for its netlists, so the UUIDs can be cross-referenced.
const NAMESPACE: [u8; 16] = [
    0x70, 0x26, 0xfc, 0xc6, 0xe1, 0xa0, 0x40, 0x9e, 0xaa, 0xf4, 0x6a, 0x17, 0xea, 0x82, 0x65, 0x4f,
];

fn format(b: &[u8; 16]) -> String {
    let h: Vec<String> = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

/// A UUID that depends only on `name`.
pub fn gen_uuid(name: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(NAMESPACE);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    format(&b)
}
