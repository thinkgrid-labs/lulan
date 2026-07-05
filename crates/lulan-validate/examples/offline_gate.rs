//! A conductor device at the gate with NO connectivity: verifies a scanned
//! QR token against a key set cached earlier from `GET /v1/ticket-keys`.
//!
//! Usage: offline_gate <keys.json> <token> [expected_trip_uuid]

use lulan_validate::{KeyEntry, verify_ticket};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(keys_path), Some(token)) = (args.next(), args.next()) else {
        eprintln!("usage: offline_gate <keys.json> <token> [expected_trip_uuid]");
        std::process::exit(2);
    };
    let expected_trip = args.next().map(|s| s.parse().expect("valid trip uuid"));

    let keys_json = std::fs::read_to_string(&keys_path).expect("read cached key set");
    let keys: Vec<KeyEntry> = serde_json::from_str(&keys_json).expect("parse key set");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    match verify_ticket(&token, &keys, now, expected_trip) {
        Ok(ticket) => {
            println!(
                "✔ VALID  seat {}  span {}→{}",
                ticket.unit_code, ticket.from_index, ticket.to_index
            );
            println!("  passenger: {}", ticket.passenger_name);
            println!("  ticket:    {}", ticket.ticket_id);
            println!("  signed by: {}", ticket.kid);
        }
        Err(err) => {
            println!("✘ REJECTED: {err}");
            std::process::exit(1);
        }
    }
}
