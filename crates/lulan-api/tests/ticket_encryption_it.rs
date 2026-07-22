//! Encryption at rest for ticket signing keys, at the database level.
//!
//! The cryptography itself is unit-tested in `lulan_engine::ticket::sealing`
//! (round-trip, tampering, wrong key, kid binding). What is left to prove
//! here is the part that only exists once Postgres is involved: that a
//! sealed row decodes back to the right signing key, and that the same row
//! is useless to someone holding only the database.
//!
//! Deliberately non-destructive. Every suite shares one database, so
//! sealing the *active* key — which is what `seal_stored_keys` does at
//! boot — would leave every other suite unable to decrypt it, since those
//! processes have no wrapping key set. This test therefore writes and
//! removes its own inactive row and never touches the active one.

use lulan_engine::ticket::TicketSigner;
use lulan_engine::ticket::sealing::{KeyWrapper, SCHEME};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const WRAPPING_KEY: &str = "4d1e9b0c7a52f38641d0c9be27a5f30188cc47ea9b6d25f0713ae84cb92d6f5a";
const WRONG_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[tokio::test]
async fn sealed_keys_round_trip_and_a_bare_dump_is_useless() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set — skipping");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    lulan_api::MIGRATOR.run(&pool).await.unwrap();

    // A known seed, so we can assert the decoded key is byte-identical
    // rather than merely "some key".
    let seed = [0x5au8; 32];
    let expected_public = TicketSigner::from_seed("expected", seed).public_key_b64();
    let kid = format!("seal-test-{}", Uuid::new_v4().simple());

    let sealed = KeyWrapper::from_hex(WRAPPING_KEY)
        .unwrap()
        .seal(&kid, &seed)
        .unwrap();
    assert_ne!(
        sealed.ciphertext.as_slice(),
        seed.as_slice(),
        "the seed must not reach the database in the clear"
    );

    // Written inactive on purpose: this row must never become the key the
    // rest of the suite signs with.
    sqlx::query(
        "INSERT INTO ticket_keys (kid, secret, public, active, encryption, nonce)
         VALUES ($1, $2, $3, false, $4, $5)",
    )
    .bind(&kid)
    .bind(&sealed.ciphertext)
    .bind(
        base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            &expected_public,
        )
        .unwrap(),
    )
    .bind(SCHEME)
    .bind(&sealed.nonce)
    .execute(&pool)
    .await
    .unwrap();

    // ---- With the wrapping key: the signer comes back intact ----------
    unsafe { std::env::set_var(lulan_engine::ticket::sealing::KEY_ENV, WRAPPING_KEY) };
    let loaded = TicketSigner::load(&pool, &kid)
        .await
        .expect("a sealed row opens with the right key")
        .expect("the row exists");
    assert_eq!(loaded.kid, kid);
    assert_eq!(
        loaded.public_key_b64(),
        expected_public,
        "the decrypted seed must be the exact key that was sealed"
    );

    // ---- Without it: the row is inert ---------------------------------
    // This is the property the whole change exists for. A leaked dump no
    // longer contains anything that can forge a boarding pass.
    unsafe { std::env::remove_var(lulan_engine::ticket::sealing::KEY_ENV) };
    let err = match TicketSigner::load(&pool, &kid).await {
        Err(err) => err,
        Ok(_) => panic!("a sealed row must not open without the wrapping key"),
    };
    assert!(
        err.to_string()
            .contains(lulan_engine::ticket::sealing::KEY_ENV),
        "the error must name the missing variable so an operator can act: {err}"
    );

    // ---- With the wrong key: refused, not silently wrong --------------
    unsafe { std::env::set_var(lulan_engine::ticket::sealing::KEY_ENV, WRONG_KEY) };
    assert!(
        TicketSigner::load(&pool, &kid).await.is_err(),
        "AEAD must reject a mismatched key rather than yield a garbage seed"
    );

    unsafe { std::env::remove_var(lulan_engine::ticket::sealing::KEY_ENV) };
    sqlx::query("DELETE FROM ticket_keys WHERE kid = $1")
        .bind(&kid)
        .execute(&pool)
        .await
        .unwrap();

    // The active key is untouched and still readable by everyone else.
    assert!(
        TicketSigner::active(&pool)
            .await
            .expect("the shared active key must remain plaintext-readable")
            .is_some()
    );
}
