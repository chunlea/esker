//! HMAC-SHA-256 (RFC 2104), the keyed hash `SigV4` derives its signing key with.
//!
//! HMAC is two hashes and some XOR:
//!
//! ```text
//! HMAC(K, m) = H( (K' ^ opad) ++ H( (K' ^ ipad) ++ m ) )
//! ```
//!
//! where `K'` is the key padded with zeros to one block, or the key's own digest — also padded
//! — when the key is longer than a block. That last clause is the part implementations get
//! wrong, so RFC 4231's cases 6 and 7 (a 131-byte key) are in the test module below and are
//! the reason this file can be trusted.
//!
//! The caveats on [`crate::sha256`] apply here too: this is not constant-time and is not for
//! authenticating anything a hostile party gets to time.

use crate::sha256::{self, BLOCK_LEN, DIGEST_LEN, Sha256};

/// The HMAC-SHA-256 of `message` under `key`.
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; DIGEST_LEN] {
    // RFC 2104 §2: a key longer than a block is replaced by its digest, and a key shorter than
    // one is padded with zeros. Both end up exactly one block wide.
    let mut padded = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        padded[..DIGEST_LEN].copy_from_slice(&sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_LEN];
    let mut outer_pad = [0x5cu8; BLOCK_LEN];
    for (index, byte) in padded.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(message);
    let inner = inner.finish();

    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(&inner);
    outer.finish()
}

#[cfg(test)]
mod tests {
    use super::hmac_sha256;
    use crate::sha256::hex;

    /// RFC 4231 §4, the HMAC-SHA-256 test cases, in order. Cases 6 and 7 are the ones that
    /// matter most: their key is 131 bytes, so an implementation that forgets to hash an
    /// over-long key passes everything above and fails here.
    ///
    /// Provenance: RFC 4231, "Identifiers and Test Vectors for HMAC-SHA-224, HMAC-SHA-256,
    /// HMAC-SHA-384, and HMAC-SHA-512", section 4.2 through 4.8.
    #[test]
    fn matches_rfc_4231() {
        // Case 1
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Case 2: a key shorter than the digest, and a printable one.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 3
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // Case 4: a 25-byte key of ascending bytes.
        let key: Vec<u8> = (1..=25u8).collect();
        assert_eq!(
            hex(&hmac_sha256(&key, &[0xcd; 50])),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
        // Case 5, whose published value is truncated to 128 bits.
        let case5 = hmac_sha256(&[0x0c; 20], b"Test With Truncation");
        assert_eq!(hex(&case5[..16]), "a3b6167473100ee06e0c796c2955552b");
        // Case 6: a 131-byte key, hashed before use.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        // Case 7: the same over-long key with a message longer than a block.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than \
                  block-size data. The key needs to be hashed before being used by the HMAC \
                  algorithm."
            )),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    /// A key of exactly one block is padded with nothing and must not be hashed. The boundary
    /// is `> BLOCK_LEN`, not `>=`, and getting that wrong is invisible without this test.
    #[test]
    fn a_key_of_exactly_one_block_is_not_hashed() {
        let exactly = hmac_sha256(&[0xaa; 64], b"boundary");
        let one_more = hmac_sha256(&[0xaa; 65], b"boundary");
        let hashed_form = hmac_sha256(&crate::sha256::digest(&[0xaa; 64]), b"boundary");
        assert_ne!(exactly, one_more);
        assert_ne!(
            exactly, hashed_form,
            "a 64-byte key was hashed when it should have been used as-is"
        );
    }

    /// An empty key and an empty message are both legal and must not panic or short-circuit.
    #[test]
    fn empty_key_and_message() {
        assert_eq!(
            hex(&hmac_sha256(b"", b"")),
            "b613679a0814d9ec772f95d778c35fc5ff1697c493715653c6c712144292c5ad"
        );
    }
}
