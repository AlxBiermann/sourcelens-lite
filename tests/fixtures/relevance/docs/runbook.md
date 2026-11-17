# Operations runbook

Procedures for the on-call engineer.

## Deploying a release

Tag the commit, wait for the pipeline to publish the images, then roll the gateway last so that
new routes only appear once the services behind them are live. Watch the error rate on the
dashboard for ten minutes before closing the change.

## Rotating secrets

Payment provider keys and the webhook secret are rotated every ninety days. Create the new
secret in the provider dashboard, store it in the vault, restart the billing service, and only
then revoke the old one. The webhook secret supports two active values during the switch.

## Handling failed payments

A spike of declined charges usually means the provider is degraded. Check the provider status
page first. Customers whose card was declined keep their reservation for thirty minutes and can
retry; after that the stock is released automatically. Never refund manually without a ledger
entry.

## Database migrations

Migrations run before the new release starts. They must be backwards compatible with the
previous release, because both versions serve traffic during the rollout. Large backfills run
as a separate job in batches of ten thousand rows.
