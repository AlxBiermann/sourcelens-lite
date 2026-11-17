//! Invoices: line totals, tax and discount codes.

use std::collections::HashMap;

/// One billable line of an invoice, in cents.
#[derive(Debug, Clone)]
pub struct InvoiceLine {
    pub description: String,
    pub quantity: u32,
    pub unit_price_cents: i64,
}

/// An invoice sent to a customer after an order was paid.
#[derive(Debug, Clone)]
pub struct Invoice {
    pub number: String,
    pub customer_id: String,
    pub country_code: String,
    pub lines: Vec<InvoiceLine>,
    pub discount_cents: i64,
}

impl Invoice {
    /// Sum of all lines before tax and discount.
    pub fn subtotal_cents(&self) -> i64 {
        self.lines
            .iter()
            .map(|line| i64::from(line.quantity) * line.unit_price_cents)
            .sum()
    }
}

/// Value added tax in cents for the invoice country, rounded half up. Countries without an entry
/// in `rates` are tax free.
pub fn calculate_tax(invoice: &Invoice, rates: &HashMap<String, f64>) -> i64 {
    let rate = rates.get(&invoice.country_code).copied().unwrap_or(0.0);
    let taxable = (invoice.subtotal_cents() - invoice.discount_cents).max(0);
    (taxable as f64 * rate).round() as i64
}

/// Applies a promotional discount code such as `SPRING10` (ten percent off) or `FLAT500`
/// (five euros off). Unknown or expired codes leave the invoice unchanged and return an error.
pub fn apply_discount_code(invoice: &mut Invoice, code: &str) -> Result<i64, String> {
    let code = code.trim().to_ascii_uppercase();
    let discount = if let Some(percent) = code.strip_prefix("SPRING") {
        let percent: i64 = percent.parse().map_err(|_| format!("unknown code {code}"))?;
        invoice.subtotal_cents() * percent.min(50) / 100
    } else if let Some(flat) = code.strip_prefix("FLAT") {
        flat.parse().map_err(|_| format!("unknown code {code}"))?
    } else {
        return Err(format!("unknown code {code}"));
    };
    invoice.discount_cents = discount.min(invoice.subtotal_cents());
    Ok(invoice.discount_cents)
}

/// Next invoice number of the year, e.g. `INV-2026-000042`.
pub fn next_invoice_number(year: u16, last_sequence: u32) -> String {
    format!("INV-{year}-{:06}", last_sequence + 1)
}
