//! Throwaway: emit a deterministic (token, key set, claims) vector so the
//! JS package can be tested against real engine output. Not shipped.
use lulan_engine::ticket::{TicketClaims, TicketSigner};
use uuid::Uuid;

fn main() {
    let signer = TicketSigner::from_seed("wire-test", [42u8; 32]);
    let tid = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    let trp = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
    let claims = TicketClaims {
        v: 1,
        tid,
        trp,
        unt: "12A".into(),
        f: 0,
        t: 3,
        pax: "Ana Reyes".into(),
        fc: Some("economy".into()),
        exp: 4_102_444_800, // 2100-01-01, comfortably in the future
        kid: signer.kid.clone(),
    };
    let token = signer.sign_token(&claims);
    println!("TOKEN={token}");
    println!("KID={}", signer.kid);
    println!("PUBKEY={}", signer.public_key_b64());
    println!("TID={tid}");
    println!("TRP={trp}");
}
