//! Cross-crate wire-contract test: a ticket signed by the AGPL engine must
//! verify with the MIT `lulan-validate` crate — the exact code conductor
//! apps embed. If the CBOR shapes ever drift apart, this breaks first.

use lulan_engine::ticket::{TicketClaims, TicketSigner};
use lulan_validate::{KeyEntry, ValidationError, verify_ticket};
use uuid::Uuid;

fn signer() -> (TicketSigner, Vec<KeyEntry>) {
    let signer = TicketSigner::from_seed("wire-test", [42u8; 32]);
    let keys = vec![KeyEntry {
        kid: "wire-test".into(),
        public_key: signer.public_key_b64(),
    }];
    (signer, keys)
}

#[test]
fn engine_signed_tickets_verify_offline() {
    let (signer, keys) = signer();
    let trip = Uuid::new_v4();
    let claims = TicketClaims {
        v: 1,
        tid: Uuid::new_v4(),
        trp: trip,
        unt: "12A".into(),
        f: 0,
        t: 3,
        pax: "Juan dela Cruz".into(),
        fc: Some("economy".into()),
        exp: 4_000_000_000,
        kid: "wire-test".into(),
    };
    let token = signer.sign_token(&claims);
    assert!(
        token.len() < 400,
        "QR budget: token is {} bytes",
        token.len()
    );

    let verified = verify_ticket(&token, &keys, 1_000_000, Some(trip)).unwrap();
    assert_eq!(verified.ticket_id, claims.tid);
    assert_eq!(verified.unit_code, "12A");
    assert_eq!(verified.passenger_name, "Juan dela Cruz");
    assert_eq!((verified.from_index, verified.to_index), (0, 3));

    // Pinning to another trip rejects; foreign key set rejects.
    assert_eq!(
        verify_ticket(&token, &keys, 1_000_000, Some(Uuid::new_v4())),
        Err(ValidationError::WrongTrip)
    );
    assert!(matches!(
        verify_ticket(&token, &[], 1_000_000, None),
        Err(ValidationError::UnknownKey { .. })
    ));
}
