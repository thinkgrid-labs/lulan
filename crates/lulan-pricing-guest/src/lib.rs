//! Reference pricing module for the `lulan:pricing` interface
//! (see lulan-pricing/wit/pricing.wit). Copy this crate as
//! the starting point for a custom fare engine.
//!
//! Build: `cargo build -p lulan-pricing-guest --target wasm32-unknown-unknown --release`
//! Run:   `LULAN_PRICING_WASM=…/lulan_pricing_guest.wasm lulan-api`
//!
//! This reference delegates to the shared rules core, so it prices
//! identically to the native engine — the differential test suite holds
//! any replacement module to the same determinism contract.

use lulan_pricing::rules::{PriceRequest, PriceResponse, evaluate};

/// Host requests a buffer to write the JSON request into.
///
/// # Safety
/// Called only by the host with a non-negative length; the returned
/// buffer is owned by the host until passed back via `price`.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut buf = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

/// Evaluate one price request. Returns `(response_ptr << 32) | response_len`.
///
/// # Safety
/// `ptr`/`len` must describe the buffer the host just wrote via `alloc`.
#[unsafe(no_mangle)]
pub extern "C" fn price(ptr: i32, len: i32) -> i64 {
    let request_bytes =
        unsafe { std::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };

    let response = match serde_json::from_slice::<PriceRequest>(request_bytes) {
        Ok(request) => match evaluate(&request.rules, &request.input) {
            Ok(quote) => PriceResponse {
                ok: Some(quote),
                err: None,
            },
            Err(err) => PriceResponse {
                ok: None,
                err: Some(err.to_string()),
            },
        },
        Err(err) => PriceResponse {
            ok: None,
            err: Some(format!("malformed request: {err}")),
        },
    };

    let out = serde_json::to_vec(&response).expect("PriceResponse always serializes");
    let out_ptr = out.as_ptr() as u64;
    let out_len = out.len() as u64;
    std::mem::forget(out);
    ((out_ptr << 32) | out_len) as i64
}
