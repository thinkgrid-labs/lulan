// Hand-written types over the wasm-pack output, whose generated .d.ts
// erases everything to `any` (serde crosses the boundary untyped). This is
// the surface boarding apps program against.

/** One signing key from `GET /v1/ticket-keys`, cached on the device. */
export interface KeyEntry {
  kid: string;
  /** base64url, 32-byte Ed25519 public key. */
  public_key: string;
}

/** A ticket that passed every check. */
export interface VerifiedTicket {
  ticket_id: string;
  trip_id: string;
  unit_code: string;
  from_index: number;
  to_index: number;
  passenger_name: string;
  fare_class: string | null;
  expires_at_unix: number;
  kid: string;
}

/**
 * The machine-readable reason a verification threw — the prefix of
 * `error.message` (`"expired: …"`). Branch on these rather than the prose.
 */
export type ValidationErrorCode =
  | "malformed"
  | "unsupported_version"
  | "unknown_key"
  | "bad_signature"
  | "expired"
  | "wrong_trip"
  | "revoked"
  | "bad_key_set"
  | "bad_expected_trip"
  | "bad_revocations";

/**
 * Verify a scanned ticket token against cached keys.
 *
 * @param token       the scanned `LT1.…` string
 * @param keys        the cached key set (`GET /v1/ticket-keys`)
 * @param nowUnix     current time in unix seconds, e.g. `Date.now()/1000`
 * @param expectedTrip the trip being boarded, or `null` for inspection
 * @throws Error whose `.message` starts with a {@link ValidationErrorCode}
 */
export function verifyTicket(
  token: string,
  keys: KeyEntry[],
  nowUnix: number,
  expectedTrip?: string | null,
): VerifiedTicket;

/**
 * As {@link verifyTicket}, and additionally refuse anything on the
 * operator's revocation list (`GET /v1/revocations`, cached on the
 * device). A signature proves issuance, never that the ticket is still
 * valid — a refund happens after signing, so this list is the only offline
 * signal of it.
 */
export function verifyTicketWithRevocations(
  token: string,
  keys: KeyEntry[],
  nowUnix: number,
  expectedTrip: string | null | undefined,
  revoked: string[],
): VerifiedTicket;

/**
 * Load and instantiate the WebAssembly module. Call once before verifying.
 * In Node with the `nodejs` build this is unnecessary (the module loads on
 * import); in browsers and React Native, await it at startup.
 */
export default function init(
  moduleOrPath?: RequestInfo | URL | Response | BufferSource | WebAssembly.Module,
): Promise<unknown>;
