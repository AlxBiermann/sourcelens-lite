// Cart sidebar shown on every product page and at checkout.
import { useMemo } from "react";
import type { OrderLine } from "../api/orders";

export interface CartSummaryProps {
  lines: OrderLine[];
  currency: string;
  discountCents: number;
  onCheckout: () => void;
}

/** Formats an amount in cents for the user's locale, e.g. 1999 → "$19.99". */
export function formatCurrency(cents: number, currency: string): string {
  return new Intl.NumberFormat(navigator.language, {
    style: "currency",
    currency,
  }).format(cents / 100);
}

/** Subtotal, discount and total of the cart, with the checkout button. */
export function CartSummary({ lines, currency, discountCents, onCheckout }: CartSummaryProps) {
  const subtotal = useMemo(
    () => lines.reduce((sum, line) => sum + line.quantity * line.unitPriceCents, 0),
    [lines],
  );
  const total = Math.max(0, subtotal - discountCents);
  const itemCount = lines.reduce((sum, line) => sum + line.quantity, 0);

  return (
    <aside className="cart-summary" aria-label="Cart summary">
      <h2>Your cart ({itemCount} items)</h2>
      <dl>
        <dt>Subtotal</dt>
        <dd>{formatCurrency(subtotal, currency)}</dd>
        {discountCents > 0 && (
          <>
            <dt>Discount</dt>
            <dd>-{formatCurrency(discountCents, currency)}</dd>
          </>
        )}
        <dt>Total</dt>
        <dd className="cart-total">{formatCurrency(total, currency)}</dd>
      </dl>
      <button type="button" disabled={itemCount === 0} onClick={onCheckout}>
        Proceed to checkout
      </button>
    </aside>
  );
}
