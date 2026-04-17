# BOT-RUST

Standalone Rust rewrite of BOT V3.0.

## Scope

- Runtime standalone: no runtime import/read/execute dependency on bot v2.0 or bot v3.0.
- BOT V3.0 is reference-only for behavior parity.
- Environment keys are compatible with BOT V3.0 keys.

## Current Status

Initial implementation bootstrap is complete:

- Cargo workspace and crate structure.
- V3-compatible config and env validation in Rust.
- Core domain types and output contracts.
- Data layer started: Binance, Gamma market discovery, CLOB orderbook snapshot.
- JSONL storage layer started.
- Telegram notifier scaffold.
- Runtime entrypoint and orchestrator scaffold.

Not implemented yet in this bootstrap:

- CLOB signed order execution and polling.
- Settlement relayer/direct redeem execution.
- Full Telegram command polling loop.
- Full cycle orchestration parity.

## Workspace Layout

- crates/bot-core: core modules.
- crates/bot-bin: runtime binary entrypoint.
- crates/bot-tests: parity-oriented tests.

## Environment Setup

1. Create bot-rust local env by copying from BOT V3.0 once:

```bash
cd "e:/Project/Polymarket Bot/BOT V2.0/bot-rust"
bash scripts/migrate_env_from_v3.sh
```

2. Runtime uses bot-rust/.env only.

## Run

```bash
cd "e:/Project/Polymarket Bot/BOT V2.0/bot-rust"
cargo run -p bot-bin -- --once
```

## Tests

```bash
cd "e:/Project/Polymarket Bot/BOT V2.0/bot-rust"
cargo test -p bot-tests
```

## Notes

- Keep stake small during future live smoke testing.
- Rotate secrets if current .env values were exposed or shared.
