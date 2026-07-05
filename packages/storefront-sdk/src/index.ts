/**
 * @lulan/storefront-sdk — typed client for the Lulan reservation engine.
 *
 * Types mirror docs/openapi.json (served live at GET /openapi.json).
 * Zero dependencies; works in Node 18+, browsers, and edge runtimes.
 *
 * ```ts
 * const lulan = new LulanClient({ baseUrl: "https://api.operator.example" });
 * const { trips } = await lulan.searchTrips({ origin: "BTG", destination: "CEB", date: "2026-07-11" });
 * const quote = await lulan.createQuote({ trip_id: trips[0].trip_id, items: [...] });
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

export interface TripSearchParams {
  origin: string;
  destination: string;
  /** ISO date (YYYY-MM-DD). */
  date: string;
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

export interface TripHit {
  trip_id: string;
  route: string;
  vessel: string;
  departs_at: string;
  from_index: number;
  to_index: number;
  seats: FareClassAvailability[];
  pools: PoolAvailability[];
}

export interface SeatAvailability {
  code: string;
  fare_class: string;
  available: boolean;
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
  /** Seat items: drives mandated discounts and the token's match key. */
  passenger_type?: PassengerType;
}

export interface Adjustment {
  /** e.g. "peak_weekday", "passenger:senior", "promo:BAGONGBYAHE" */
  label: string;
  amount_minor: number;
}

export interface QuotedItem extends Required<Pick<QuoteItemRequest, "unit_code" | "origin" | "destination" | "quantity">> {
  passenger_type: PassengerType | null;
  base_minor: number;
  total_minor: number;
  adjustments: Adjustment[];
}

export interface Quote {
  trip_id: string;
  currency: string;
  total_minor: number;
  expires_at: string;
  /** Locks these prices at checkout for ~5 minutes. */
  quote_token: string;
  items: QuotedItem[];
}

export interface OrderItemRequest {
  unit_code: string;
  origin: string;
  destination: string;
  /** Pool items only. */
  quantity?: number;
  /** Index into passengers[]; required for seats unless there is exactly one passenger. */
  passenger?: number;
  /** Consumes a soft hold on success. */
  hold_id?: string;
}

export interface CreateOrderRequest {
  trip_id: string;
  passengers: Passenger[];
  /** Email or phone. Required unless authenticated with a customer JWT. */
  guest_contact?: string;
  quote_token?: string;
  promo_code?: string;
  items: OrderItemRequest[];
}

export interface OrderItem {
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
  trip_id: string;
  passenger_name: string;
  status: OrderStatus;
  total_minor: number;
  currency: string;
  payment_intent_id: string | null;
  expires_at: string | null;
  passengers: PassengerRecord[];
  items: OrderItem[];
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
  trip_id: string;
  status: string;
  total_minor: number;
  currency: string;
  created_at: string;
}

export interface HoldResponse {
  hold_id: string;
  expires_at: string;
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

  searchTrips(params: TripSearchParams, options?: RequestOptions): Promise<{ trips: TripHit[] }> {
    const query = new URLSearchParams(params as unknown as Record<string, string>);
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

  createHold(
    tripId: string,
    item: { unit_code: string; origin: string; destination: string },
    options?: RequestOptions,
  ): Promise<HoldResponse> {
    return this.request("POST", `/v1/trips/${tripId}/holds`, item, options);
  }

  releaseHold(holdId: string, options?: RequestOptions): Promise<void> {
    return this.request("DELETE", `/v1/holds/${holdId}`, undefined, options);
  }

  // ---- quotes & orders -------------------------------------------------

  createQuote(
    request: { trip_id: string; items: QuoteItemRequest[]; promo_code?: string },
    options?: RequestOptions,
  ): Promise<Quote> {
    return this.request("POST", "/v1/quotes", request, options);
  }

  createOrder(request: CreateOrderRequest, options?: RequestOptions): Promise<CreatedOrder> {
    return this.request("POST", "/v1/orders", request, options);
  }

  /** Reads need a credential: the retrieval token, a customer JWT, or an API key. */
  getOrder(orderId: string, retrievalToken?: string, options?: RequestOptions): Promise<OrderDetails> {
    const query = retrievalToken ? `?token=${encodeURIComponent(retrievalToken)}` : "";
    return this.request("GET", `/v1/orders/${orderId}${query}`, undefined, options);
  }

  requestPayment(orderId: string, options?: RequestOptions): Promise<PaymentResponse> {
    return this.request("POST", `/v1/orders/${orderId}/payment`, {}, options);
  }

  cancelOrder(orderId: string, options?: RequestOptions): Promise<{ order_id: string; status: OrderStatus }> {
    return this.request("POST", `/v1/orders/${orderId}/cancel`, {}, options);
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

  /** Public keys for offline validation — cache these on conductor devices. */
  ticketKeys(options?: RequestOptions): Promise<{ keys: TicketKey[] }> {
    return this.request("GET", "/v1/ticket-keys", undefined, options);
  }

  /** Conductor devices: sync the boarding journal (requires a conductor API key). */
  syncScans(deviceId: string, scans: ScanRequest[], options?: RequestOptions): Promise<{ outcomes: ScanOutcome[] }> {
    return this.request("POST", "/v1/scans", { device_id: deviceId, scans }, options);
  }
}
