# Shopflow architecture

Shopflow is a small storefront split into a web client, a Go gateway and three backend services.

## Overview

The browser talks only to the gateway. The gateway proxies `/v1/users`, `/v1/orders` and
`/v1/billing` to the internal services and applies rate limits and access logs on the way.
Background workers run inventory, notification and reporting jobs from a shared queue.

## Order lifecycle

An order starts as `pending` when the customer checks out. The inventory worker reserves stock
for thirty minutes. When the payment succeeds the order becomes `paid`, the reservation turns
into a deduction and the customer receives a confirmation email. Shipping moves it to
`shipped`. A customer can cancel until then; cancelling a paid order triggers a refund.

## Payment flow

The billing service charges the stored card through the payment provider and records every
charge and refund in a double-entry ledger. The provider confirms charges asynchronously with
signed webhooks; the signature and its timestamp are checked before an event is processed, so
replayed or forged webhooks are dropped.

## Caching strategy

User profiles are cached in the web client for five minutes, keyed by user id. Changing an
email address evicts the entry. Order data is never cached because its status changes often.
