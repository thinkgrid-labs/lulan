/**
 * @lulan/storefront-sdk — typed client for the Lulan reservation engine.
 *
 * Types mirror the engine's OpenAPI spec (served live at GET /openapi.json).
 * Zero dependencies; works in Node 18+, browsers, and edge runtimes.
 *
 * ```ts
 * const lulan = new LulanClient({ baseUrl: "https://api.operator.example" });
 * const { legs } = await lulan.searchTrips({ origin: "BTG", destination: "CEB", departure_date: "2026-07-11" });
 * const outbound = legs[0].trips[0];
 * const quote = await lulan.createQuote({ trip_id: outbound.trip_id, items: [...] });
 * const order = await lulan.createOrder({ ... , quote_token: quote.quote_token },
 *                                        { idempotencyKey: crypto.randomUUID() });
 * ```
 */

// ---------------------------------------------------------------- types

export type PassengerType = "adult" | "child" | "senior" | "pwd" | "infant";

export type OrderStatus =
  | "locked"
  | "pending_payment"
  | "paid"
  | "ticketed"
  | "boarded"
  | "cancelled"
  | "expired";

export interface Passenger {
  full_name: string;
  type: PassengerType;
  /** ISO date; lets staff verify age-based fares at boarding. */
  birthdate?: string;
}

export type TripType = "one_way" | "round_trip";

export interface TripSearchParams {
  origin: string;
  destination: string;
  /** Outbound service date (YYYY-MM-DD). */
  departure_date: string;
  trip_type?: TripType;
  /** Required for round_trip, forbidden for one_way. */
  return_date?: string;
}

export interface FareClassAvailability {
  fare_class: string;
  total: number;
  available: number;
}

export interface PoolAvailability {
  code: string;
  remaining: number;
}

export interface Operator {
  code: string;
  name: string;
}

export interface Vehicle {
  code: string;
  name: string;
  kind: "bus" | "ferry" | "aircraft" | "other";
}

export interface TripCandidate {
  trip_id: string;
  route_code: string;
  /** Carrier; absent if unassigned. */
  operator?: Operator;
  /** Passenger-facing service designator (flight/service number). */
  service_number?: string;
  vehicle: Vehicle;
  origin: string;
  destination: string;
  /** Departure from the requested origin (UTC ISO). */
  departs_at: string;
  /** Arrival at the requested destination (UTC ISO); absent if no schedule. */
  arrives_at?: string;
  duration_minutes?: number;
  from_index: number;
  to_index: number;
  seats: FareClassAvailability[];
  pools: PoolAvailability[];
}

export interface SearchLeg {
  leg: "outbound" | "return";
  origin: string;
  destination: string;
  date: string;
  trips: TripCandidate[];
}

export interface TripSearchResult {
  trip_type: TripType;
  /** One-way: 1 leg. Round-trip: outbound + return. */
  legs: SearchLeg[];
}

export interface SeatAvailability {
  code: string;
  fare_class: string;
  /** Free to claim (sold state — the source of truth). */
  available: boolean;
  /** Another session soft-holds an overlapping span right now (advisory). */
  held: boolean;
}

export interface AvailabilityResponse {
  trip_id: string;
  from_index: number;
  to_index: number;
  seats: SeatAvailability[];
  pools: PoolAvailability[];
}

export interface QuoteItemRequest {
  unit_code: string;
  origin: string;
  destination: string;
  quantity?: number;
  /** Seat items: drives concession fares and the token's match key. */
  passenger_type?: PassengerType;
}

export interface Adjustment {
  /** e.g. "peak_weekday", "passenger:senior", "promo:BAGONGBYAHE" */
  label: string;
  amount_minor: number;
}

export interface QuotedItem extends Required<Pick<QuoteItemRequest, "unit_code" | "origin" | "destination" | "quantity">> {
  trip_id: string;
  passenger_type: PassengerType | null;
  base_minor: number;
  total_minor: number;
  adjustments: Adjustment[];
}

export interface Journey<Item> {
  trip_id: string;
  items: Item[];
}

export interface Quote {
  currency: string;
  /** 1 = one-way; 2 with reversed O&D = round-trip; otherwise multi-city. */
  journey_count: number;
  is_round_trip: boolean;
  total_minor: number;
  expires_at: string;
  /** Locks these prices at checkout for ~5 minutes. */
  quote_token: string;
  items: QuotedItem[];
  ancillaries: Array<{
    code: string; name: string; trip_id?: string; passenger?: number;
    quantity: number; total_minor: number;
  }>;
}

export interface OrderItemRequest {
  unit_code: string;
  origin: string;
  destination: string;
  /** Pool items only. */
  quantity?: number;
  /** Index into passengers[]; required for seats unless there is exactly one passenger. */
  passenger?: number;
}

/**
 * An itinerary: either `journeys` (round-trip / multi-city) or the
 * single-trip shape `trip_id` + `items` — sugar for a 1-journey itinerary.
 * Trip type is derived: 2 journeys with reversed O&D = round-trip.
 */
export interface CreateOrderRequest {
  trip_id?: string;
  items?: OrderItemRequest[];
  journeys?: Journey<OrderItemRequest>[];
  passengers: Passenger[];
  /** Email or phone. Required unless authenticated with a customer JWT. */
  guest_contact?: string;
  quote_token?: string;
  promo_code?: string;
  /** Itinerary hold from createHold, released once the order's claims succeed. */
  hold_id?: string;
  /** Add-ons; must match the quote token's lines when a token is presented. */
  ancillaries?: AncillaryLine[];
}

export interface OrderItem {
  trip_id: string;
  unit_code: string;
  kind: "seat" | "pool";
  from_index: number;
  to_index: number;
  quantity: number;
  price_minor: number;
  passenger_id: string | null;
}

export interface PassengerRecord {
  id: string;
  full_name: string;
  passenger_type: PassengerType;
  birthdate: string | null;
}

export interface Order {
  order_id: string;
  /** Distinct trips this itinerary touches, in item order. */
  trip_ids: string[];
  passenger_name: string;
  status: OrderStatus;
  total_minor: number;
  currency: string;
  payment_intent_id: string | null;
  expires_at: string | null;
  passengers: PassengerRecord[];
  items: OrderItem[];
  ancillaries: OrderAncillary[];
}

export interface CreatedOrder extends Order {
  /** The order's read credential — deliver to the guest (magic link). */
  retrieval_token: string;
  customer_id: string | null;
}

export interface OrderEvent {
  stream_seq: number;
  event_type: string;
  occurred_at: string;
}

export interface OrderDetails extends Order {
  events: OrderEvent[];
}

export interface PaymentResponse {
  order_id: string;
  status: OrderStatus;
  payment_intent_id: string;
}

export interface Ticket {
  ticket_id: string;
  passenger_id: string;
  passenger_name: string;
  unit_code: string;
  status: "issued" | "boarded" | "void";
  /** Signed QR payload (LT1.<payload>.<sig>) — render as a QR code. */
  token: string;
}

export interface TicketKey {
  kid: string;
  alg: string;
  /** base64url, 32 bytes (Ed25519 public key). */
  public_key: string;
}

export interface ScanRequest {
  ticket_id: string;
  scanned_at: string;
  result?: string;
}

export interface ScanOutcome {
  ticket_id: string;
  status: "boarded" | "already_boarded" | "duplicate_scan" | "unknown_ticket";
  order_status: string | null;
}

export interface CustomerOrderSummary {
  order_id: string;
  trip_ids: string[];
  status: string;
  total_minor: number;
  currency: string;
  created_at: string;
}

export interface Ancillary {
  id: string;
  code: string;
  name: string;
  description: string;
  /** Grouping for display: baggage, meal, insurance, service… */
  kind: string;
  price_minor: number;
  per: "passenger" | "order";
  scope: "journey" | "itinerary";
}

/** Same shape at quote and order — the token match is verbatim. */
export interface AncillaryLine {
  code: string;
  /** Journey-scoped add-ons: which leg. */
  trip_id?: string;
  /** Per-passenger add-ons: index into passengers[]. */
  passenger?: number;
  quantity?: number;
}

export interface OrderAncillary {
  code: string;
  name: string;
  trip_id: string | null;
  passenger_id: string | null;
  quantity: number;
  total_minor: number;
}

export interface HoldItem {
  unit_code: string;
  origin: string;
  destination: string;
}

export interface HoldResponse {
  /** One id for the whole itinerary hold — pass to createOrder or releaseHold. */
  hold_id: string;
  expires_at: string;
  items: Array<{ trip_id: string; unit_code: string; origin: string; destination: string }>;
}

// ---------------------------------------------------------------- errors

export class LulanApiError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(status: number, message: string, body: unknown) {
    super(`Lulan API ${status}: ${message}`);
    this.name = "LulanApiError";
    this.status = status;
    this.body = body;
  }
}

// ---------------------------------------------------------------- client

export interface LulanClientOptions {
  /** e.g. "https://api.operator.example" (no trailing slash). */
  baseUrl: string;
  /** Server-to-server API key (llk_…) — sets X-Api-Key. */
  apiKey?: string;
  /** Customer JWT from the operator's IdP — sets Authorization: Bearer. */
  customerToken?: string;
  /** Custom fetch (tests, polyfills). Defaults to globalThis.fetch. */
  fetch?: typeof globalThis.fetch;
}

export interface RequestOptions {
  /** Replay-safe booking retries: same key → same order, no double charge. */
  idempotencyKey?: string;
  /** Per-call override of the customer JWT. */
  customerToken?: string;
  signal?: AbortSignal;
}

export class LulanClient {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private customerToken: string | undefined;
  private readonly fetchImpl: typeof globalThis.fetch;

  constructor(options: LulanClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.customerToken = options.customerToken;
    this.fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** Attach/replace the signed-in customer's JWT for subsequent calls. */
  setCustomerToken(token: string | undefined): void {
    this.customerToken = token;
  }

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
    options: RequestOptions = {},
  ): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.apiKey) headers["x-api-key"] = this.apiKey;
    const bearer = options.customerToken ?? this.customerToken;
    if (bearer) headers["authorization"] = `Bearer ${bearer}`;
    if (options.idempotencyKey) headers["idempotency-key"] = options.idempotencyKey;

    const init: RequestInit = { method, headers };
    if (body !== undefined) init.body = JSON.stringify(body);
    if (options.signal) init.signal = options.signal;
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, init);

    const text = await response.text();
    const json: unknown = text ? JSON.parse(text) : null;
    if (!response.ok) {
      const message =
        typeof json === "object" && json !== null && "error" in json
          ? String((json as { error: unknown }).error)
          : response.statusText;
      throw new LulanApiError(response.status, message, json);
    }
    return json as T;
  }

  // ---- trips ----------------------------------------------------------

  searchTrips(params: TripSearchParams, options?: RequestOptions): Promise<TripSearchResult> {
    const query = new URLSearchParams(
      Object.fromEntries(Object.entries(params).filter(([, v]) => v !== undefined)),
    );
    return this.request("GET", `/v1/trips/search?${query}`, undefined, options);
  }

  availability(
    tripId: string,
    span: { origin: string; destination: string },
    options?: RequestOptions,
  ): Promise<AvailabilityResponse> {
    const query = new URLSearchParams(span);
    return this.request("GET", `/v1/trips/${tripId}/availability?${query}`, undefined, options);
  }

  // ---- holds ----------------------------------------------------------

  /**
   * Hold a one-way or round-trip seat selection as ONE itinerary hold
   * (all-or-nothing). Same journeys[] shape as quotes/orders; a round trip
   * passes both legs in one call. Pass the returned hold_id to createOrder.
   */
  createHold(
    request:
      | { trip_id: string; items: HoldItem[]; ttl_seconds?: number }
      | { journeys: Journey<HoldItem>[]; ttl_seconds?: number },
    options?: RequestOptions,
  ): Promise<HoldResponse> {
    return this.request("POST", "/v1/holds", request, options);
  }

  releaseHold(holdId: string, options?: RequestOptions): Promise<void> {
    return this.request("DELETE", `/v1/holds/${holdId}`, undefined, options);
  }

  // ---- quotes & orders -------------------------------------------------

  createQuote(
    request:
      | { trip_id: string; items: QuoteItemRequest[]; promo_code?: string; ancillaries?: AncillaryLine[] }
      | { journeys: Journey<QuoteItemRequest>[]; promo_code?: string; ancillaries?: AncillaryLine[] },
    options?: RequestOptions,
  ): Promise<Quote> {
    return this.request("POST", "/v1/quotes", request, options);
  }

  /** The operator's add-on catalog (baggage, meals, insurance, …). */
  ancillaries(options?: RequestOptions): Promise<Ancillary[]> {
    return this.request("GET", "/v1/ancillaries", undefined, options);
  }

  createOrder(request: CreateOrderRequest, options?: RequestOptions): Promise<CreatedOrder> {
    return this.request("POST", "/v1/orders", request, options);
  }

  /** Reads need a credential: the retrieval token, a customer JWT, or an API key. */
  getOrder(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<OrderDetails> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("GET", `/v1/orders/${orderId}${query}`, undefined, options);
  }

  /**
   * Needs a credential, like every other order operation: the retrieval
   * token, a customer JWT, or an API key. The intent id this returns is
   * what captures the payment.
   */
  requestPayment(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<PaymentResponse> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("POST", `/v1/orders/${orderId}/payment${query}`, {}, options);
  }

  /** Needs a credential: the retrieval token, a customer JWT, or an API key. */
  cancelOrder(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<{ order_id: string; status: OrderStatus }> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("POST", `/v1/orders/${orderId}/cancel${query}`, {}, options);
  }

  // ---- customers -------------------------------------------------------

  myOrders(options?: RequestOptions): Promise<CustomerOrderSummary[]> {
    return this.request("GET", "/v1/customers/me/orders", undefined, options);
  }

  claimOrder(orderId: string, retrievalToken: string, options?: RequestOptions): Promise<{ order_id: string; customer_id: string }> {
    return this.request("POST", `/v1/orders/${orderId}/claim`, { retrieval_token: retrievalToken }, options);
  }

  // ---- tickets ---------------------------------------------------------

  getTickets(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<{ order_id: string; tickets: Ticket[] }> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("GET", `/v1/orders/${orderId}/tickets${query}`, undefined, options);
  }

  issueTickets(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<{ order_id: string; tickets: Ticket[] }> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("POST", `/v1/orders/${orderId}/tickets${query}`, {}, options);
  }

  /** Public keys for offline validation — cache these on crew devices. */
  ticketKeys(options?: RequestOptions): Promise<{ keys: TicketKey[] }> {
    return this.request("GET", "/v1/ticket-keys", undefined, options);
  }

  /** Boarding devices: sync the scan journal (requires a validator API key). */
  syncScans(deviceId: string, scans: ScanRequest[], options?: RequestOptions): Promise<{ outcomes: ScanOutcome[] }> {
    return this.request("POST", "/v1/scans", { device_id: deviceId, scans }, options);
  }
}
