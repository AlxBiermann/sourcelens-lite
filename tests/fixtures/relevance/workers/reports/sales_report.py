"""Nightly sales report for the finance team."""

import csv
import io
from collections import defaultdict
from datetime import date


def aggregate_daily_sales(orders: list[dict]) -> dict[date, dict[str, int]]:
    """Sums paid orders per day: order count, units sold and revenue in cents.

    Cancelled and refunded orders are left out.
    """
    totals: dict[date, dict[str, int]] = defaultdict(
        lambda: {"orders": 0, "units": 0, "revenue_cents": 0}
    )
    for order in orders:
        if order["status"] in ("cancelled", "refunded"):
            continue
        day = order["placed_at"].date()
        totals[day]["orders"] += 1
        totals[day]["units"] += sum(line["quantity"] for line in order["lines"])
        totals[day]["revenue_cents"] += order["total_cents"]
    return dict(totals)


def top_selling_products(orders: list[dict], limit: int = 10) -> list[tuple[str, int]]:
    """The SKUs with the most units sold, best seller first."""
    units: dict[str, int] = defaultdict(int)
    for order in orders:
        for line in order["lines"]:
            units[line["sku"]] += line["quantity"]
    return sorted(units.items(), key=lambda item: item[1], reverse=True)[:limit]


def export_report_csv(totals: dict[date, dict[str, int]]) -> str:
    """Writes the daily totals as CSV with one row per day, oldest first."""
    buffer = io.StringIO()
    writer = csv.writer(buffer)
    writer.writerow(["day", "orders", "units", "revenue"])
    for day in sorted(totals):
        row = totals[day]
        writer.writerow(
            [day.isoformat(), row["orders"], row["units"], f"{row['revenue_cents'] / 100:.2f}"]
        )
    return buffer.getvalue()
