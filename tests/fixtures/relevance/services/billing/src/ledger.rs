//! Double-entry ledger of every money movement.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Debit,
    Credit,
}

/// One money movement on an account.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub account: String,
    pub reference: String,
    pub direction: Direction,
    pub amount_cents: i64,
}

impl Transaction {
    pub fn charge(order_id: &str, amount_cents: i64) -> Self {
        Self {
            account: "receivables".into(),
            reference: order_id.into(),
            direction: Direction::Credit,
            amount_cents,
        }
    }

    pub fn refund(charge_id: &str, amount_cents: i64) -> Self {
        Self {
            account: "receivables".into(),
            reference: charge_id.into(),
            direction: Direction::Debit,
            amount_cents,
        }
    }
}

/// Append-only list of transactions with a running balance per account.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: Vec<Transaction>,
    balances: BTreeMap<String, i64>,
}

impl Ledger {
    /// Appends a transaction and updates the balance of its account. Debits lower the balance,
    /// credits raise it.
    pub fn record_transaction(&mut self, transaction: Transaction) {
        let signed = match transaction.direction {
            Direction::Credit => transaction.amount_cents,
            Direction::Debit => -transaction.amount_cents,
        };
        *self.balances.entry(transaction.account.clone()).or_insert(0) += signed;
        self.entries.push(transaction);
    }

    /// Current balance of an account in cents; zero for an account without transactions.
    pub fn balance_for_account(&self, account: &str) -> i64 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    /// Transactions that mention `reference`, oldest first, for the audit trail.
    pub fn entries_for_reference(&self, reference: &str) -> Vec<&Transaction> {
        self.entries
            .iter()
            .filter(|entry| entry.reference == reference)
            .collect()
    }
}
