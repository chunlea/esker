//! SHA-256 (FIPS 180-4), the hash AWS Signature Version 4 is built out of.
//!
//! `CLAUDE.md` bans the crates that would otherwise supply this — anything reaching `ring`,
//! `aws-lc-rs` or an `openssl-sys` — and the algorithm is two hundred lines of arithmetic, so
//! it lives here beside [`crate::crc32c`] under the same rule: **published vectors, not
//! self-agreement**. A hash that only matches itself will sign every request wrongly and
//! consistently, and the first thing to notice would be S3 rejecting all of them.
//!
//! This is not a general-purpose cryptographic library and does not pretend to be one. It is
//! constant-shaped rather than constant-time, it has no side-channel hardening, and the only
//! secret it ever touches is an S3 secret access key inside this process. Do not reach for it
//! to check a password.
//!
//! ```
//! # use esker_base::sha256;
//! assert_eq!(
//!     sha256::hex(&sha256::digest(b"abc")),
//!     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
//! );
//! ```

/// Bytes in a SHA-256 digest.
pub const DIGEST_LEN: usize = 32;

/// Bytes in the compression function's input block. HMAC's padding is defined in terms of it.
pub const BLOCK_LEN: usize = 64;

/// The first thirty-two bits of the fractional parts of the cube roots of the first sixty-four
/// primes. Part of the algorithm, so frozen.
#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4, 0xab1c_5ed5,
    0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174,
    0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
    0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967,
    0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85,
    0xa2bf_e8a1, 0xa81a_664b, 0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
    0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
];

/// The first thirty-two bits of the fractional parts of the square roots of the first eight
/// primes.
const INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// An incremental SHA-256.
///
/// Feed it with [`update`](Self::update) as many times as convenient — the digest depends only
/// on the concatenation, never on where the calls fell — and take the result with
/// [`finish`](Self::finish). [`digest`] is the one-shot form.
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// Bytes of the current block that are filled. Always `< BLOCK_LEN` between calls.
    buffered: usize,
    block: [u8; BLOCK_LEN],
    /// Total bytes consumed. The padding encodes this times eight, so a message longer than
    /// 2^61 bytes would wrap — a limit the type system cannot express and no caller can reach.
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// A hasher over the empty message.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: INITIAL,
            buffered: 0,
            block: [0u8; BLOCK_LEN],
            len: 0,
        }
    }

    /// Appends `data` to the message being hashed.
    pub fn update(&mut self, data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        let mut rest = data;

        // Top up a partially filled block first, so the fast path below can work on whole
        // blocks straight out of the caller's slice with no copying.
        if self.buffered > 0 {
            let want = BLOCK_LEN - self.buffered;
            let take = want.min(rest.len());
            self.block[self.buffered..self.buffered + take].copy_from_slice(&rest[..take]);
            self.buffered += take;
            rest = &rest[take..];
            if self.buffered < BLOCK_LEN {
                // `take` was limited by `rest`, so `rest` is now empty and the block is still
                // partial. Returning here is not an optimisation: falling through would set
                // `buffered` from an empty remainder and forget what was just buffered.
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.buffered = 0;
        }

        let mut chunks = rest.chunks_exact(BLOCK_LEN);
        for chunk in &mut chunks {
            // `chunks_exact` yields exactly BLOCK_LEN bytes, so the conversion cannot fail.
            let block: &[u8; BLOCK_LEN] = match chunk.try_into() {
                Ok(block) => block,
                // Unreachable; written as a branch rather than an `expect` so that this
                // module contains no panic path at all (CLAUDE.md invariant 9).
                Err(_) => continue,
            };
            compress(&mut self.state, block);
        }

        let tail = chunks.remainder();
        self.block[..tail.len()].copy_from_slice(tail);
        self.buffered = tail.len();
    }

    /// Finishes the message and returns its digest.
    #[must_use]
    pub fn finish(mut self) -> [u8; DIGEST_LEN] {
        // FIPS 180-4 §5.1.1: a single 1 bit, then zeros, then the length in bits as a 64-bit
        // big-endian integer, chosen so the whole message is a multiple of the block.
        let bit_len = self.len.wrapping_mul(8);
        self.block[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > BLOCK_LEN - 8 {
            self.block[self.buffered..].fill(0);
            let block = self.block;
            compress(&mut self.state, &block);
            self.buffered = 0;
        }
        self.block[self.buffered..BLOCK_LEN - 8].fill(0);
        self.block[BLOCK_LEN - 8..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        compress(&mut self.state, &block);

        let mut out = [0u8; DIGEST_LEN];
        for (word, chunk) in self.state.iter().zip(out.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The SHA-256 of `data`.
#[must_use]
pub fn digest(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finish()
}

/// The digest of the empty message, which `SigV4` uses as the payload hash of a bodyless request.
///
/// Precomputed because it appears in every signed GET and DELETE, and because a constant is
/// easier to recognise in a canonical request than a call.
pub const EMPTY_DIGEST_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Lowercase hexadecimal, the encoding every `SigV4` field uses.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// One application of the compression function to one 64-byte block.
fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_LEN]) {
    let mut w = [0u32; 64];
    for (word, chunk) in w[..16].iter_mut().zip(block.chunks_exact(4)) {
        // The chunk is four bytes; the fallback keeps the function panic-free.
        *word = match <[u8; 4]>::try_from(chunk) {
            Ok(bytes) => u32::from_be_bytes(bytes),
            Err(_) => 0,
        };
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let mut v = *state;
    for i in 0..64 {
        let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let temp1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let temp2 = s0.wrapping_add(maj);

        v[7] = v[6];
        v[6] = v[5];
        v[5] = v[4];
        v[4] = v[3].wrapping_add(temp1);
        v[3] = v[2];
        v[2] = v[1];
        v[1] = v[0];
        v[0] = temp1.wrapping_add(temp2);
    }

    for (slot, value) in state.iter_mut().zip(v) {
        *slot = slot.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use super::{EMPTY_DIGEST_HEX, Sha256, digest, hex};
    use crate::rng::Pcg32;

    /// FIPS 180-4's own examples plus the standard long-message case. These are why this
    /// module can be trusted: every other test here would pass just as happily against a
    /// subtly wrong round constant.
    ///
    /// Provenance: NIST FIPS 180-4 appendix B.1/B.2 ("abc" and the 56-byte message), and the
    /// one-million-`a` vector from the same document's §B.3.
    #[test]
    fn matches_published_vectors() {
        assert_eq!(hex(&digest(b"")), EMPTY_DIGEST_HEX);
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // The two-block case: 896 bits, so the length does not fit in the first block's
        // padding and a second block is generated. `\` at end of line elides the newline and
        // the indentation, so this is one 112-byte message.
        assert_eq!(
            hex(&digest(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
                  hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            )),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );

        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&digest(&million)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The digest depends on the concatenation, not on where `update` was called. The padding
    /// is the part that gets this wrong, and it gets it wrong only at block boundaries.
    #[test]
    fn splitting_does_not_change_the_digest() {
        let mut rng = Pcg32::new(0xba78_16bf, 1);
        let mut data = vec![0u8; 600];
        rng.fill_bytes(&mut data);
        let whole = digest(&data);
        for split in 0..data.len() {
            let mut hasher = Sha256::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finish(), whole, "split at {split}");
        }
    }

    /// Every length around a block boundary, because the padding branches at exactly
    /// `BLOCK_LEN - 8` and a message that lands there needs a second block of nothing but
    /// padding.
    #[test]
    fn every_length_class_through_two_blocks() {
        let mut rng = Pcg32::new(0x2222, 7);
        let mut data = vec![0u8; 200];
        rng.fill_bytes(&mut data);
        for len in 0..data.len() {
            let one_shot = digest(&data[..len]);
            let mut hasher = Sha256::new();
            for byte in &data[..len] {
                hasher.update(std::slice::from_ref(byte));
            }
            assert_eq!(hasher.finish(), one_shot, "byte-at-a-time at len {len}");
        }
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        assert_eq!(hex(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&digest(b"abc")).len(), 64);
    }
}
