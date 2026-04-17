# Parity Matrix: TypeScript BOT -> BOT-RUST

## Runtime Boundary

- Standalone target: BOT-RUST must run without runtime dependency to sibling TypeScript bots.
- Reference source only: TypeScript implementation for behavior mapping.

## Module Mapping

| TypeScript module                     | BOT-RUST module                                        | Status  |
| ------------------------------------- | ------------------------------------------------------ | ------- |
| src/config/index.ts                   | crates/bot-core/src/config/mod.rs                      | Started |
| src/types/index.ts                    | crates/bot-core/src/types/mod.rs                       | Started |
| src/data/binance.ts                   | crates/bot-core/src/data/binance.rs                    | Started |
| src/data/marketDiscovery.ts           | crates/bot-core/src/data/market_discovery.rs           | Started |
| src/data/orderbook.ts                 | crates/bot-core/src/data/orderbook.rs                  | Started |
| src/execution/client.ts               | crates/bot-core/src/execution/client.rs                | Started |
| src/execution/orderExecutor.ts        | crates/bot-core/src/execution/order_executor.rs        | Started |
| src/settlement/resultResolver.ts      | crates/bot-core/src/settlement/result_resolver.rs      | TODO    |
| src/settlement/settlementService.ts   | crates/bot-core/src/settlement/settlement_service.rs   | Started |
| src/storage/resultLogger.ts           | crates/bot-core/src/storage/result_logger.rs           | Started |
| src/storage/tradeLogger.ts            | crates/bot-core/src/storage/trade_logger.rs            | Started |
| src/notifications/telegramNotifier.ts | crates/bot-core/src/notifications/telegram_notifier.rs | Started |
| src/index.ts                          | crates/bot-core/src/orchestrator.rs                    | Started |

## Hard Invariants

- FOK market BUY spentUsd must follow requested stake amount, not filledSize \* filledPrice.
- Null/non-object order status must be treated as transient retryable path.
- Settlement PnL must use executed filled basis.
- Runtime env and output paths must be resolved from bot-rust root.

## Next Implementation Targets

1. Extend orchestrator to full stage parity (resolve/claim lifecycle, pause/guard handling).
2. Settlement redeem flow with relayer/direct fallback.
3. Telegram lifecycle notification parity across all runtime stages.
4. End-to-end runtime parity and lifecycle notifications.
