// Order endpoints: checkout, cancellation and the order history page.
import { httpClient } from "./client";
import { withExponentialBackoff } from "../utils/retry";

export type OrderStatus = "pending" | "paid" | "shipped" | "cancelled" | "refunded";

export interface OrderLine {
  sku: string;
  quantity: number;
  unitPriceCents: number;
}

export interface Order {
  id: string;
  customerId: string;
  status: OrderStatus;
  lines: OrderLine[];
  totalCents: number;
  placedAt: Date;
}

/**
 * Places an order for the items in the cart. The idempotency key makes a retried checkout
 * create the order only once.
 */
export async function createOrder(customerId: string, lines: OrderLine[]): Promise<Order> {
  if (lines.length === 0) {
    throw new Error("An order needs at least one line.");
  }
  const idempotencyKey = crypto.randomUUID();
  const response = await withExponentialBackoff(() =>
    httpClient.post("/v1/orders", {
      headers: { "Idempotency-Key": idempotencyKey },
      body: { customerId, lines },
    }),
  );
  return parseOrder(response.body);
}

/** Cancels an order that has not shipped yet; the payment is refunded by the billing service. */
export async function cancelOrder(orderId: string, reason: string): Promise<Order> {
  const response = await httpClient.post(`/v1/orders/${orderId}/cancel`, {
    body: { reason },
  });
  if (response.status === 409) {
    throw new Error("The order has already shipped and can no longer be cancelled.");
  }
  return parseOrder(response.body);
}

/** All orders of a customer, newest first, following the pagination cursor. */
export async function fetchOrderHistory(customerId: string): Promise<Order[]> {
  const orders: Order[] = [];
  let cursor: string | undefined;
  do {
    const response = await httpClient.get(`/v1/customers/${customerId}/orders`, {
      query: cursor ? { cursor } : {},
    });
    orders.push(...response.body.items.map(parseOrder));
    cursor = response.body.nextCursor;
  } while (cursor);
  return orders;
}

function parseOrder(raw: Record<string, any>): Order {
  return {
    id: raw.id,
    customerId: raw.customer_id,
    status: raw.status,
    lines: raw.lines,
    totalCents: raw.total_cents,
    placedAt: new Date(raw.placed_at),
  };
}
