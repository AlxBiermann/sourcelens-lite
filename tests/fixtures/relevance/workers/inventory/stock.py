"""Stock levels per warehouse and reservations for open orders."""

from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone

RESERVATION_TTL = timedelta(minutes=30)


class OutOfStockError(Exception):
    """Raised when a warehouse cannot cover the requested quantity."""


@dataclass
class StockLevel:
    sku: str
    warehouse: str
    on_hand: int
    reserved: dict[str, tuple[int, datetime]] = field(default_factory=dict)

    @property
    def available(self) -> int:
        """Units that are on hand and not held by a reservation."""
        return self.on_hand - sum(quantity for quantity, _ in self.reserved.values())


def reserve_stock(level: StockLevel, order_id: str, quantity: int) -> datetime:
    """Holds `quantity` units for an order until checkout completes.

    The reservation expires after RESERVATION_TTL so abandoned carts give the units back.
    Returns the expiry time.
    """
    if quantity <= 0:
        raise ValueError("quantity must be positive")
    if level.available < quantity:
        raise OutOfStockError(
            f"{level.sku}: {level.available} available in {level.warehouse}, {quantity} requested"
        )
    expires_at = datetime.now(timezone.utc) + RESERVATION_TTL
    level.reserved[order_id] = (quantity, expires_at)
    return expires_at


def release_expired_reservations(level: StockLevel, now: datetime | None = None) -> list[str]:
    """Drops reservations whose time ran out and returns their order ids."""
    now = now or datetime.now(timezone.utc)
    expired = [order_id for order_id, (_, until) in level.reserved.items() if until <= now]
    for order_id in expired:
        del level.reserved[order_id]
    return expired


def restock_from_shipment(levels: dict[str, StockLevel], shipment: list[dict]) -> int:
    """Adds the units of a supplier shipment to the matching stock levels.

    Unknown SKUs are skipped. Returns the number of units added.
    """
    added = 0
    for item in shipment:
        level = levels.get(item["sku"])
        if level is None:
            continue
        level.on_hand += int(item["quantity"])
        added += int(item["quantity"])
    return added
