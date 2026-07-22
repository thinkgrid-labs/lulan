//! Encryption at rest for ticket signing keys.
//!
//! The Ed25519 seed that signs boarding passes used to sit in Postgres in
//! the clear. Anyone holding a database dump — a backup in object storage,
//! a stolen replica, an over-broad read grant — could forge a valid ticket
//! for any trip, indefinitely, and the forgery would verify perfectly at
//! every offline gate. Nothing downstream could tell.
//!
//! Sealing splits that into two secrets that live in different places: the
//! ciphertext stays in the database, and the key that opens it comes from
//! the environment (`LULAN_TICKET_KEY_ENCRYPTION_KEY`, mounted from
//! whatever secret manager the operator already runs). A dump on its own
//! is then inert.
//!
//! **What this does not defend.** An attacker on the running host reads
//! both the environment and process memory. This buys separation of the
//! database from the signing key, which is the realistic breach; it does
//! not buy protection from host compromise, and it is not an HSM.
//!
//! **Losing the encryption key is recoverable.** Public halves are stored
//! unencrypted, so tickets already issued keep verifying; you lose only
//! the ability to sign new ones, which a rotation fixes. That is
//! deliberate — key-at-rest schemes that can strand issued credentials are
//! not worth adopting.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;

/// Env var holding the 32-byte wrapping key, hex-encoded (64 characters).
pub const KEY_ENV: &str = "LULAN_TICKET_KEY_ENCRYPTION_KEY";

/// Recorded in `ticket_keys.encryption` so a future scheme can be added
/// without guessing how existing rows were written.
pub const SCHEME: &str = "xchacha20poly1305";

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error(
        "{KEY_ENV} must be {expected} hex characters ({KEY_LEN} bytes); \
         generate one with: openssl rand -hex 32",
        expected = KEY_LEN * 2
    )]
    MalformedKey,
    #[error("ticket signing key is sealed with {0:?}, which this build does not know")]
    UnknownScheme(String),
    #[error(
        "ticket signing key is sealed but {KEY_ENV} is not set — set it to the same value \
         used when the key was written, or rotate to mint a fresh key"
    )]
    KeyUnavailable,
    #[error(
        "ticket signing key failed to decrypt: {KEY_ENV} does not match the one it was \
         sealed with, or the stored row was altered"
    )]
    Undecryptable,
    #[error("stored ticket key is {0} bytes, expected {KEY_LEN}")]
    BadSeedLength(usize),
}

/// A sealed seed, exactly as the three `ticket_keys` columns hold it.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub scheme: &'static str,
}

/// Wraps and unwraps signing seeds. Cheap to construct — built per call
/// rather than cached, so a test or a re-read always observes the current
/// environment instead of whatever was set first.
pub struct KeyWrapper {
    cipher: XChaCha20Poly1305,
}

impl KeyWrapper {
    /// `Ok(None)` when no wrapping key is configured: seeds are then
    /// stored in the clear, which is the pre-existing behaviour and stays
    /// available for dev and for operators who accept the risk knowingly.
    pub fn from_env() -> Result<Option<Self>, SealError> {
        match std::env::var(KEY_ENV) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(Self::from_hex(value.trim())?)),
            _ => Ok(None),
        }
    }

    pub fn from_hex(hex: &str) -> Result<Self, SealError> {
        if hex.len() != KEY_LEN * 2 {
            return Err(SealError::MalformedKey);
        }
        let mut key = [0u8; KEY_LEN];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| SealError::MalformedKey)?;
        }
        Ok(Self {
            cipher: XChaCha20Poly1305::new(&key.into()),
        })
    }

    /// Seal `seed` for the key identified by `kid`.
    ///
    /// The kid is the associated data, so a ciphertext lifted from one row
    /// cannot be pasted into another: it authenticates as belonging to the
    /// key it was written for, or it does not open at all.
    ///
    /// XChaCha20-Poly1305 over AES-GCM for the 24-byte nonce — random
    /// nonces are safe without any counter or reuse bookkeeping — and
    /// because it is constant-time in software, with no dependence on
    /// AES-NI being present on whatever the operator deploys to.
    pub fn seal(&self, kid: &str, seed: &[u8; KEY_LEN]) -> Result<Sealed, SealError> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: seed.as_slice(),
                    aad: kid.as_bytes(),
                },
            )
            .map_err(|_| SealError::Undecryptable)?;

        Ok(Sealed {
            ciphertext,
            nonce: nonce_bytes.to_vec(),
            scheme: SCHEME,
        })
    }

    pub fn open(
        &self,
        kid: &str,
        scheme: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<[u8; KEY_LEN], SealError> {
        if scheme != SCHEME {
            return Err(SealError::UnknownScheme(scheme.to_string()));
        }
        if nonce.len() != NONCE_LEN {
            return Err(SealError::Undecryptable);
        }
        let plain = self
            .cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: kid.as_bytes(),
                },
            )
            .map_err(|_| SealError::Undecryptable)?;
        seed_from(&plain)
    }
}

/// Interpret a stored `ticket_keys` row's secret, sealed or not.
pub fn unseal_row(
    kid: &str,
    scheme: Option<&str>,
    nonce: Option<&[u8]>,
    secret: &[u8],
) -> Result<[u8; KEY_LEN], SealError> {
    match (scheme, nonce) {
        // Written before sealing existed, or by a deployment that has not
        // configured a wrapping key.
        (None, _) => seed_from(secret),
        (Some(scheme), Some(nonce)) => KeyWrapper::from_env()?
            .ok_or(SealError::KeyUnavailable)?
            .open(kid, scheme, nonce, secret),
        // The CHECK constraint makes this unrepresentable in the database.
        (Some(_), None) => Err(SealError::Undecryptable),
    }
}

fn seed_from(bytes: &[u8]) -> Result<[u8; KEY_LEN], SealError> {
    bytes
        .try_into()
        .map_err(|_| SealError::BadSeedLength(bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const KEY_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    const SEED: [u8; 32] = [7u8; 32];

    #[test]
    fn seals_and_opens_round_trip() {
        let wrapper = KeyWrapper::from_hex(KEY_A).unwrap();
        let sealed = wrapper.seal("lulan-abc", &SEED).unwrap();

        assert_ne!(
            sealed.ciphertext.as_slice(),
            SEED.as_slice(),
            "the seed must not survive into the stored bytes"
        );
        assert_eq!(sealed.nonce.len(), NONCE_LEN);
        assert_eq!(
            wrapper
                .open(
                    "lulan-abc",
                    sealed.scheme,
                    &sealed.nonce,
                    &sealed.ciphertext
                )
                .unwrap(),
            SEED
        );
    }

    /// Two seals of the same seed must differ, or the ciphertext leaks
    /// that the key was reused.
    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let wrapper = KeyWrapper::from_hex(KEY_A).unwrap();
        let one = wrapper.seal("lulan-abc", &SEED).unwrap();
        let two = wrapper.seal("lulan-abc", &SEED).unwrap();
        assert_ne!(one.nonce, two.nonce);
        assert_ne!(one.ciphertext, two.ciphertext);
    }

    /// The whole point: a database dump without the environment key is
    /// inert.
    #[test]
    fn a_different_key_cannot_open_it() {
        let sealed = KeyWrapper::from_hex(KEY_A)
            .unwrap()
            .seal("lulan-abc", &SEED)
            .unwrap();
        let attacker = KeyWrapper::from_hex(KEY_B).unwrap();
        assert!(matches!(
            attacker.open(
                "lulan-abc",
                sealed.scheme,
                &sealed.nonce,
                &sealed.ciphertext
            ),
            Err(SealError::Undecryptable)
        ));
    }

    /// AEAD, not just encryption: altering the stored row is detected
    /// rather than yielding a garbage seed that silently signs junk.
    #[test]
    fn tampering_is_detected() {
        let wrapper = KeyWrapper::from_hex(KEY_A).unwrap();
        let sealed = wrapper.seal("lulan-abc", &SEED).unwrap();

        let mut bad_ct = sealed.ciphertext.clone();
        bad_ct[0] ^= 0x01;
        assert!(
            wrapper
                .open("lulan-abc", sealed.scheme, &sealed.nonce, &bad_ct)
                .is_err()
        );

        let mut bad_nonce = sealed.nonce.clone();
        bad_nonce[0] ^= 0x01;
        assert!(
            wrapper
                .open("lulan-abc", sealed.scheme, &bad_nonce, &sealed.ciphertext)
                .is_err()
        );
    }

    /// The kid is associated data, so a ciphertext cannot be moved to
    /// another key's row and opened there.
    #[test]
    fn ciphertext_is_bound_to_its_kid() {
        let wrapper = KeyWrapper::from_hex(KEY_A).unwrap();
        let sealed = wrapper.seal("lulan-abc", &SEED).unwrap();
        assert!(matches!(
            wrapper.open(
                "lulan-xyz",
                sealed.scheme,
                &sealed.nonce,
                &sealed.ciphertext
            ),
            Err(SealError::Undecryptable)
        ));
    }

    #[test]
    fn malformed_wrapping_keys_are_refused() {
        for bad in [
            "",
            "abc",
            "zz112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        ] {
            assert!(matches!(
                KeyWrapper::from_hex(bad),
                Err(SealError::MalformedKey)
            ));
        }
    }

    #[test]
    fn unknown_scheme_is_refused_rather_than_guessed() {
        let wrapper = KeyWrapper::from_hex(KEY_A).unwrap();
        let sealed = wrapper.seal("lulan-abc", &SEED).unwrap();
        assert!(matches!(
            wrapper.open("lulan-abc", "aes-gcm-siv", &sealed.nonce, &sealed.ciphertext),
            Err(SealError::UnknownScheme(s)) if s == "aes-gcm-siv"
        ));
    }

    /// Rows written before sealing existed stay readable.
    #[test]
    fn unsealed_rows_are_read_as_plaintext() {
        assert_eq!(unseal_row("lulan-abc", None, None, &SEED).unwrap(), SEED);
        assert!(matches!(
            unseal_row("lulan-abc", None, None, b"too short"),
            Err(SealError::BadSeedLength(9))
        ));
    }
}
