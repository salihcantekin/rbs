//! The check phrase both users compare before media is used (ADR-0139 decision 6,
//! docs/specs/peer-port.md "Check phrase").
//!
//! The adapter hashes [`phrase_input`] with SHA-256 (it owns the crypto backend; this
//! port has none) and turns the digest into words with [`CheckPhrase::from_digest`].

use std::fmt;

use crate::code::SessionId;
use crate::error::PhraseError;
use crate::words::WORDS;

/// Domain tag that starts every phrase input.
pub const PHRASE_DOMAIN: &[u8] = b"rbs-guest-phrase-v1";
/// Words in a phrase: 6 × 11 bits = 66 bits.
pub const PHRASE_WORDS: usize = 6;
/// The shortest digest [`CheckPhrase::from_digest`] takes (66 bits need 9 bytes).
pub const MIN_DIGEST_LEN: usize = 9;
/// The longest certificate fingerprint accepted (SHA-512).
pub const MAX_FINGERPRINT_LEN: usize = 64;

/// The bytes to hash: the domain tag, the session id and both DTLS-authenticated
/// certificate fingerprints, each with a big-endian `u16` length so no byte can move
/// from one fingerprint to the other without changing the input.
pub fn phrase_input(
    session: SessionId,
    offerer_fingerprint: &[u8],
    answerer_fingerprint: &[u8],
) -> Result<Vec<u8>, PhraseError> {
    for fingerprint in [offerer_fingerprint, answerer_fingerprint] {
        if fingerprint.is_empty() || fingerprint.len() > MAX_FINGERPRINT_LEN {
            return Err(PhraseError::BadFingerprint);
        }
    }
    let mut input = Vec::with_capacity(
        PHRASE_DOMAIN.len() + 16 + 4 + offerer_fingerprint.len() + answerer_fingerprint.len(),
    );
    input.extend_from_slice(PHRASE_DOMAIN);
    input.extend_from_slice(session.as_bytes());
    for fingerprint in [offerer_fingerprint, answerer_fingerprint] {
        // At most MAX_FINGERPRINT_LEN (checked above), so the cast cannot truncate.
        input.extend_from_slice(&(fingerprint.len() as u16).to_be_bytes());
        input.extend_from_slice(fingerprint);
    }
    Ok(input)
}

/// Six words from the BIP-39 English list.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CheckPhrase([u16; PHRASE_WORDS]);

impl CheckPhrase {
    /// The phrase for a SHA-256 digest of [`phrase_input`]: its first 66 bits, most
    /// significant bit first, as six 11-bit word indices.
    pub fn from_digest(digest: &[u8]) -> Result<Self, PhraseError> {
        let bytes = digest
            .get(..MIN_DIGEST_LEN)
            .ok_or(PhraseError::ShortDigest)?;
        let mut indices = [0u16; PHRASE_WORDS];
        for (word, index) in indices.iter_mut().enumerate() {
            let mut value = 0u16;
            for bit in word * 11..word * 11 + 11 {
                let set = (bytes[bit / 8] >> (7 - bit % 8)) & 1;
                value = (value << 1) | u16::from(set);
            }
            *index = value;
        }
        Ok(Self(indices))
    }

    /// The six words, in order.
    pub fn words(&self) -> [&'static str; PHRASE_WORDS] {
        self.0.map(|index| WORDS[usize::from(index)])
    }
}

impl fmt::Display for CheckPhrase {
    /// The words separated by single spaces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, word) in self.words().iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(word)?;
        }
        Ok(())
    }
}

impl fmt::Debug for CheckPhrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CheckPhrase({self})")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn id() -> SessionId {
        SessionId::new([3; 16]).unwrap()
    }

    #[test]
    fn the_word_list_has_2048_unique_words() {
        let mut sorted = WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 2048);
        assert_eq!(WORDS[0], "abandon");
        assert_eq!(WORDS[2047], "zoo");
        // Unique by the first four letters, so a phrase read aloud is hard to mishear.
        let mut prefixes: Vec<&str> = WORDS.iter().map(|w| &w[..w.len().min(4)]).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(prefixes.len(), 2048);
    }

    #[test]
    fn fixed_digests_give_fixed_phrases() {
        assert_eq!(
            CheckPhrase::from_digest(&[0; 32]).unwrap().to_string(),
            "abandon abandon abandon abandon abandon abandon"
        );
        assert_eq!(
            CheckPhrase::from_digest(&[0xff; 9]).unwrap().to_string(),
            "zoo zoo zoo zoo zoo zoo"
        );
        // 0x00 0x20 0x04 ... puts index 1 in each 11-bit group.
        // Bits: 00000000001 00000000001 ... -> six times "ability".
        let mut digest = [0u8; 9];
        for word in 0..PHRASE_WORDS {
            let bit = word * 11 + 10;
            digest[bit / 8] |= 1 << (7 - bit % 8);
        }
        assert_eq!(
            CheckPhrase::from_digest(&digest).unwrap().words(),
            ["ability"; PHRASE_WORDS]
        );
        // Only the first 66 bits count: bit 66 onward changes nothing.
        let mut tail = [0u8; 32];
        tail[8] = 0x3f;
        tail[20] = 0xff;
        assert_eq!(
            CheckPhrase::from_digest(&tail).unwrap(),
            CheckPhrase::from_digest(&[0; 9]).unwrap()
        );
    }

    #[test]
    fn a_short_digest_is_refused() {
        assert_eq!(
            CheckPhrase::from_digest(&[0; 8]),
            Err(PhraseError::ShortDigest)
        );
    }

    #[test]
    fn the_input_is_unambiguous() {
        let a = phrase_input(id(), b"abc", b"def").unwrap();
        let b = phrase_input(id(), b"abcd", b"ef").unwrap();
        assert_ne!(
            a, b,
            "moving a byte between fingerprints must change the input"
        );
        let swapped = phrase_input(id(), b"def", b"abc").unwrap();
        assert_ne!(a, swapped, "the roles are part of the input");
        let other = phrase_input(SessionId::new([4; 16]).unwrap(), b"abc", b"def").unwrap();
        assert_ne!(a, other);
        assert!(a.starts_with(PHRASE_DOMAIN));
        assert_eq!(a.len(), PHRASE_DOMAIN.len() + 16 + 2 + 3 + 2 + 3);
    }

    #[test]
    fn bad_fingerprints_are_refused() {
        assert_eq!(
            phrase_input(id(), b"", b"x"),
            Err(PhraseError::BadFingerprint)
        );
        assert_eq!(
            phrase_input(id(), b"x", &[1; 65]),
            Err(PhraseError::BadFingerprint)
        );
        assert!(phrase_input(id(), &[1; 64], &[1; 64]).is_ok());
    }
}
