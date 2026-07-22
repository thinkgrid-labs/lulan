/**
 * @lulan/admin-sdk — typed client for the Lulan admin operations API
 * (`/v1/admin/*`). Deliberately separate from @lulan/storefront-sdk:
 * different audience, different auth, and admin surface never ships in
 * customer bundles.
 *
 * Auth: a staff JWT from the operator's IdP (`admin` / `ops` / `support`
 * role), or an `operator_admin` API key for bootstrap automation.
 */

export type StaffRole = "admin" | "ops" | "support";

export interface Staff {
  id: string;
  issuer: string;
  subject: string;
  display_name: string;
  email: string | null;
  role: StaffRole;
  active: boolean;
}

export interface RouteStop {
  location_code: string;
  /** Minutes from the route's origin departure. */
  arrive_offset_min?: number;
  depart_offset_min?: number;
}

export interface SeatSpec {
  code: string;
  fare_class: string;
}

export interface PoolSpec {
  code: string;
  capacity: number;
}

export interface ManifestSeat {
  seat: string;
  passenger: string | null;
  passenger_type: string | null;
  from_index: number;
  to_index: number;
  order_id: string;
  order_status: string;
  ticket_status: string | null;
}

export interface OrderSummary {
  order_id: string;
  passenger_name: string;
  status: string;
  total_minor: number;
  currency: string;
  guest_contact: string | null;
  created_at: string;
}

export class LulanAdminError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(status: number, message: string, body: unknown) {
    super(`Lulan admin API ${status}: ${message}`);
    this.name = "LulanAdminError";
    this.status = status;
    this.body = body;
  }
}

export interface LulanAdminOptions {
  baseUrl: string;
  /** Staff JWT from the operator's IdP. */
  staffToken?: string;
  /** operator_admin API key (bootstrap automation). */
  apiKey?: string;
  fetch?: typeof globalThis.fetch;
}

export class LulanAdmin {
  private readonly baseUrl: string;
  private staffToken: string | undefined;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: typeof globalThis.fetch;

  constructor(options: LulanAdminOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.staffToken = options.staffToken;
    this.apiKey = options.apiKey;
    this.fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  setStaffToken(token: string | undefined): void {
    this.staffToken = token;
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.staffToken) headers["authorization"] = `Bearer ${this.staffToken}`;
    else if (this.apiKey) headers["x-api-key"] = this.apiKey;
    const init: RequestInit = { method, headers };
    if (body !== undefined) init.body = JSON.stringify(body);
    const response = await this.fetchImpl(`${this.baseUrl}${path}`, init);
    const text = await response.text();
    const json: unknown = text ? JSON.parse(text) : null;
    if (!response.ok) {
      const message =
        typeof json === "object" && json !== null && "error" in json
          ? String((json as { error: unknown }).error)
          : response.statusText;
      throw new LulanAdminError(response.status, message, json);
    }
    return json as T;
  }

  // ---- staff (admin) ---------------------------------------------------
  enrollStaff(req: {
    subject: string;
    display_name: string;
    role: StaffRole;
    issuer?: string;
    email?: string;
  }): Promise<Staff> {
    return this.request("POST", "/v1/admin/staff", req);
  }

  listStaff(): Promise<Staff[]> {
    return this.request("GET", "/v1/admin/staff");
  }

  revokeStaff(id: string): Promise<{ revoked: string }> {
    return this.request("DELETE", `/v1/admin/staff/${id}`);
  }

  // ---- fare rules (ops) ------------------------------------------------
  listFareRules(): Promise<{ rulesets: Array<{ id: string; active: boolean; created_at: string }> }> {
    return this.request("GET", "/v1/admin/fare-rules");
  }

  /** Validate + publish + activate a ruleset; the previous stays for rollback. */
  publishFareRules(rules: unknown): Promise<{ id: string; active: boolean }> {
    return this.request("POST", "/v1/admin/fare-rules", rules);
  }

  /** Rollback/switch the active ruleset. */
  activateFareRules(id: string): Promise<{ id: string; active: boolean }> {
    return this.request("POST", `/v1/admin/fare-rules/${id}/activate`, {});
  }

  // ---- network & schedule (ops) -----------------------------------------
  createLocation(req: { code: string; name: string; timezone?: string }): Promise<{ id: string }> {
    return this.request("POST", "/v1/admin/locations", req);
  }

  createRoute(req: { code: string; name: string; stops: RouteStop[] }): Promise<{ id: string }> {
    return this.request("POST", "/v1/admin/routes", req);
  }

  createVessel(req: {
    code: string;
    name: string;
    kind: "bus" | "ferry" | "aircraft" | "other";
    seats?: SeatSpec[];
    pools?: PoolSpec[];
  }): Promise<{ id: string }> {
    return this.request("POST", "/v1/admin/vessels", req);
  }

  /** Schedule departures (ISO timestamps, UTC). */
  createTrips(req: {
    route_code: string;
    vessel_code: string;
    operator_code?: string;
    service_number?: string;
    departures: string[];
  }): Promise<{ trip_ids: string[] }> {
    return this.request("POST", "/v1/admin/trips", req);
  }

  /** Cancel a departure; cascades (unpaid cancelled, paid refunded). */
  /**
   * Mint a new ticket signing key. Live on every replica immediately;
   * retired keys stay published so issued tickets keep verifying.
   */
  rotateTicketKey(): Promise<{ kid: string; public_key: string }> {
    return this.request("POST", "/v1/admin/ticket-keys/rotate", {});
  }

  cancelTrip(tripId: string): Promise<{
    trip_id: string;
    orders_cancelled: number;
    orders_refunded: number;
    failures: number;
  }> {
    return this.request("POST", `/v1/admin/trips/${tripId}/cancel`, {});
  }

  // ---- orders & manifests (support) ---------------------------------------
  searchOrders(params: { contact?: string; name?: string; trip_id?: string }): Promise<{ orders: OrderSummary[] }> {
    const query = new URLSearchParams(
      Object.fromEntries(Object.entries(params).filter(([, v]) => v !== undefined)) as Record<string, string>,
    );
    return this.request("GET", `/v1/admin/orders?${query}`);
  }

  /** Full refund through the payment port; frees seats, voids tickets. */
  refundOrder(orderId: string): Promise<{ order_id: string; status: string }> {
    return this.request("POST", `/v1/admin/orders/${orderId}/refund`, {});
  }

  tripManifest(tripId: string): Promise<{ trip_id: string; seats: ManifestSeat[] }> {
    return this.request("GET", `/v1/admin/trips/${tripId}/manifest`);
  }
}
