"""Order handling for the shop backend."""

from dataclasses import dataclass, field
from decimal import Decimal


@dataclass
class LineItem:
    sku: str
    quantity: int
    unit_price: Decimal


@dataclass
class Order:
    order_id: str
    customer: str
    items: list[LineItem] = field(default_factory=list)

    def total(self) -> Decimal:
        return sum((item.unit_price * item.quantity for item in self.items), Decimal("0"))


def create_order(order_id: str, customer: str, items: list[LineItem]) -> Order:
    if not items:
        raise ValueError("An order needs at least one line item.")
    for item in items:
        if item.quantity <= 0:
            raise ValueError(f"Quantity of {item.sku} must be positive.")
        if item.unit_price < 0:
            raise ValueError(f"Price of {item.sku} must not be negative.")
    return Order(order_id=order_id, customer=customer, items=list(items))


def apply_discount(order: Order, percent: int) -> Decimal:
    if percent < 0 or percent > 100:
        raise ValueError("A discount is a percentage between 0 and 100.")
    total = order.total()
    discount = total * Decimal(percent) / Decimal(100)
    return (total - discount).quantize(Decimal("0.01"))
