# BOT-RUST

Standalone Rust implementation of the Polymarket bot.

## Scope

- Runtime standalone: no runtime import/read/execute dependency on sibling TypeScript projects.
- TypeScript implementation is reference-only for behavior parity.
- Environment keys use the native BOT-RUST contract (no legacy prefixes).

## Current Status

Initial implementation bootstrap is complete:

- Cargo workspace and crate structure.
- Config and env validation in Rust.
- Core domain types and output contracts.
- Data layer started: Binance, Gamma market discovery, CLOB orderbook snapshot.
- Execution core started: CLOB auth/signing, FOK submit path, status polling, spent accounting invariants.
- JSONL storage layer started.
- Telegram notifier started: send + command polling (/pause, /resume, /status).
- Runtime entrypoint and orchestrator wired to live execution path (UP-first candidate selection).

Not implemented yet in this bootstrap:

- Full orchestrator parity with complete stage machine and settlement lifecycle.
- Settlement relayer/direct redeem execution.
- Full Telegram lifecycle parity (all notification moments and control hooks).
- Full cycle orchestration parity.

## Workspace Layout

- crates/bot-core: core modules.
- crates/bot-bin: runtime binary entrypoint.
- crates/bot-tests: parity-oriented tests.

## Environment Setup

1. Create bot-rust local env from template:

```bash
cd "e:/Project/Polymarket Bot/BOT V2.0/bot-rust"
bash scripts/bootstrap_env.sh
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
