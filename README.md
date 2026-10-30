# Verifund — Native NEAR Escrow Contract

Programmable escrow for machine-verifiable work on NEAR Protocol.

## Status (2026-10-30)

- ✅ All 24 unit tests passing
- ✅ WASM built (`verifund.wasm` – 242 KB)
- ⏳ Testnet deployment pending

## Quick start

```bash
cargo test --lib
# WASM is already built and included as verifund.wasm
```

## Contract methods

- `new()`
- `create_task(task_id, worker, criteria_hash, authorized_verifier, deadline_ns)` (payable)
- `submit(task_id, submission_hash)`
- `attest(task_id, submission_hash, passed, failed_test_ids)`
- `timeout_refund(task_id)`
- `retry_transfer(task_id)`
- `get_task(task_id)` (view)

near-sdk = 5.5.0
