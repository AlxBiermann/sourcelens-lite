//! Card payments through the payment provider, with refunds.

use crate::ledger::{Ledger, Transaction};

/// Why a charge or refund did not go through.
#[derive(Debug, PartialEq, Eq)]
pub enum PaymentError {
    CardDeclined { decline_code: String },
    InsufficientFunds,
    ProviderUnavailable,
    RefundExceedsCharge { charged_cents: i64, requested_cents: i64 },
}

pub struct PaymentProvider {
    pub api_key: String,
    pub base_url: String,
}

/// Charges a stored credit card. Declines are mapped to [`PaymentError`]; a successful charge is
/// recorded in the ledger before the provider's charge id is returned.
pub fn charge_credit_card(
    provider: &PaymentProvider,
    ledger: &mut Ledger,
    card_token: &str,
    amount_cents: i64,
    order_id: &str,
) -> Result<String, PaymentError> {
    let response = provider.post_charge(card_token, amount_cents, order_id)?;
    match response.status.as_str() {
        "succeeded" => {
            ledger.record_transaction(Transaction::charge(order_id, amount_cents));
            Ok(response.charge_id)
        }
        "insufficient_funds" => Err(PaymentError::InsufficientFunds),
        _ => Err(PaymentError::CardDeclined {
            decline_code: response.status,
        }),
    }
}

/// Returns money of an earlier charge to the customer, fully or in part. Refunding more than
/// was charged is refused.
pub fn refund_payment(
    provider: &PaymentProvider,
    ledger: &mut Ledger,
    charge_id: &str,
    charged_cents: i64,
    requested_cents: i64,
) -> Result<(), PaymentError> {
    if requested_cents > charged_cents {
        return Err(PaymentError::RefundExceedsCharge {
            charged_cents,
            requested_cents,
        });
    }
    provider.post_refund(charge_id, requested_cents)?;
    ledger.record_transaction(Transaction::refund(charge_id, requested_cents));
    Ok(())
}
