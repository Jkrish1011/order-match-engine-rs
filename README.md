# match-engine-rust

Pure matching engine logic for centralized order books, written in Rust. This project focuses exclusively on the core matching logic so it can be embedded in other systems: exchanges, broker simulators, market microstructure research tools, backtesters, and educational projects.

The engine is designed for determinism, performance, and composability. It supports price-time priority, limit and market orders, cancels, partial fills, and efficient book queries (BBO, depth, snapshots). Concurrency primitives are used where appropriate to support multi-producer inputs and concurrent reads.

Status: active development. The intent is a full, self-contained, production-grade matching core with a clean API surface you can import as a library.

This README reflects the current code in `src/main.rs` and distinguishes between what is implemented today and what is planned next.

## Table of contents

- [Goals](#goals)
- [Features](#features)
- [Design overview](#design-overview)
- [Data structures](#data-structures)
- [Types](#types)
- [Order matching rules](#order-matching-rules)
- [API](#api)
  - [Creating an order book](#creating-an-order-book)
  - [Placing orders](#placing-orders)
  - [Cancelling and amending](#cancelling-and-amending)
  - [Querying the book](#querying-the-book)
  - [Receiving events](#receiving-events)
- [Examples](#examples)
- [Performance and benchmarking](#performance-and-benchmarking)
- [Testing and property checks](#testing-and-property-checks)
- [Integration guidelines](#integration-guidelines)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

## Goals

- Deterministic, price-time priority matching
- High throughput with predictable latency
- Pure matching logic (no networking, persistence, or external I/O)
- Ergonomic, well-documented API suitable for embedding
- Clear separation of concerns: matching vs. ingestion vs. persistence

## Features

- Limit and market orders
- Price-time priority at each price level
- Partial and full fills
- Cancels (and optional replace/amend)
- Efficient queries:
  - Best bid/offer (BBO)
  - Top-N depth and full depth snapshot
  - Price-level volume
- Event stream of trades, order status changes, and book updates
- Configurable tick size and price/quantity units (fixed-point integers)
- Concurrency-friendly ingestion and read-side queries

## Design overview

At a high level, the engine maintains two sides of the book: bids and asks. Each side holds price levels, and each price level maintains a FIFO queue of orders. Matching rules ensure price-time priority with minimal overhead. The engine exposes:

- A synchronous API for submitting commands (new/cancel/replace)
- A query API for snapshots and derived data (BBO, depth)
- An event feed API (trades, partial fills, full fills, cancels)

The core logic avoids external concerns (networking, databases, logging) so that you can integrate it into your system of choice.

## Data structures

This repository uses battle-tested concurrent primitives:

- `crossbeam_skiplist::SkipMap` for sorted price levels on each side (logarithmic inserts/lookups)
- `dashmap::DashMap` for order registry by `OrderId`
- `crossbeam_queue::SegQueue` for MPMC event/command queues when building concurrent pipelines

These choices provide predictable complexity and good multi-threaded performance characteristics.

## Types (implemented)

Defined in `src/main.rs`:

- `OrderId = u64` — globally unique identifier
- `Price = u64` — fixed-point integer; integration layer is responsible for scaling/formatting
- `Quantity = u64` — fixed-point integer quantity
- `Side` — `Buy` or `Sell`
- `OrderType` — `Limit` or `Market`
- `OrderStatus` — `Active`, `PartiallyFilled`, `Filled`, `Cancelled` (repr as `u8`)
- `OrderBookError` — domain errors (not yet returned from public APIs in the current snapshot)

You can select a tick size and unit scaling appropriate to your instrument universe in your integration layer.

## Order matching rules

- Price priority: better prices match first (`Buy` prefers higher prices; `Sell` prefers lower prices)
- Time priority: within the same price level, earlier orders match first (FIFO)
- A market order crosses immediately against the best available opposite side until it is filled or the book exhausts
- A limit order matches any immediately-crossing liquidity and then rests if unfilled
- Post-match accounting updates order statuses and emits events

## API

Below is a snapshot of the API broken into two parts: what exists now and what is planned.

### Implemented today (in `src/main.rs`)

- `Order`
  - `Order::new(order_id, side, order_type, price, quantity)` — constructs a new order with current timestamp
  - `get_remaining_quantity()` — current remaining qty
  - `get_status()` — current `OrderStatus`
  - `fill(quantity)` — atomically decrements remaining quantity, updates status to `PartiallyFilled`/`Filled`, returns actual filled
  - `cancel()` — attempts to mark as `Cancelled`; returns `true` if successful
  - `is_active()` — `true` if `Active` or `PartiallyFilled`

- `PriceLevel`
  - Holds FIFO queue of `Arc<Order>` and caches total quantity for quick depth queries
  - `add_order(order: Arc<Order>)` — push to FIFO and update cached quantity
  - `get_total_quantity()` — approximate total quantity (may be slightly stale)

- `OrderBook`
  - Internals: `bids: SkipMap<Price, Arc<PriceLevel>>`, `asks: SkipMap<Price, Arc<PriceLevel>>`, `order_lookup: DashMap<OrderId, Arc<Order>>`
  - `OrderBook::new()` — construct an empty book
  - `best_bid()` — highest bid price (if any)
  - `best_ask()` — lowest ask price (if any)
  - `spread()` — `ask - bid` if both sides exist
  - `total_bid_quantity()` / `total_ask_quantity()` / `total_quantity()` — sums cached totals across price levels

Notes:

- Public methods to place/cancel/replace orders against the book, matching logic, and public query APIs for depth/snapshots are not yet wired up in this snapshot.
- Event types and streams are not yet exposed.

### Planned (soon)

- Public ingestion API: `place`, `cancel`, `replace`
- Matching engine core: crossing logic for market/limit, resting logic, and FIFO priority within price levels
- Query API: BBO struct, top-N depth, full snapshot
- Event model: trades, fills, cancels, rejects
- Multi-symbol orchestration (one `OrderBook` per instrument)

## Examples

Because public ingestion/matching APIs are not yet exposed, examples focus on the currently available pieces.

Minimal usage of `Order`:

```rust
use match_engine_rust::*; // adjust to your crate path

let order = Order::new(1, Side::Buy, OrderType::Limit, 10_000, 5);
assert_eq!(order.get_remaining_quantity(), 5);

let filled = order.fill(3);
assert_eq!(filled, 3);
assert!(matches!(order.get_status(), OrderStatus::PartiallyFilled));

let filled2 = order.fill(10); // overfill request is clipped to remaining
assert_eq!(filled2, 2);
assert!(matches!(order.get_status(), OrderStatus::Filled));
```

Read-side helpers on an empty `OrderBook`:

```rust
let book = OrderBook::new();
assert!(book.best_bid().is_none());
assert!(book.best_ask().is_none());
assert!(book.spread().is_none());
assert_eq!(book.total_quantity(), 0);
```

## Performance and benchmarking

We use `criterion` for microbenchmarks and throughput measurements. Targets include:

- New order insert path (resting)
- Matching path (crossing taker vs. multiple makers)
- Cancel path under contention
- Snapshot and top-N depth queries

Run benchmarks:

```bash
cargo bench
```

Performance will depend on CPU topology, cache sizes, and your chosen scaling for `Price`/`Quantity`. For realistic numbers, pin threads and run with `--release`.

## Testing and property checks

- Unit tests for matching rules and edge cases
- Property-based tests with `proptest` (e.g., conservation of quantity, monotonic FIFO within price levels, never negative remaining)
- Deterministic replay tests for sequences of commands

Run tests:

```bash
cargo test
```

## Integration guidelines

- Keep networking/transport outside the engine. Convert external messages into `NewOrder`/`Cancel`/`Replace` commands.
- Convert price/quantity into the engine's fixed-point integers at the boundary.
- Subscribe to the event stream to drive downstream components (trade feed, positions, risk checks, persistence).
- If you need multi-symbol support, run one `OrderBook` per symbol or partition by symbol ID.

## Roadmap

- Wire up public ingestion API (place/cancel/replace)
- Implement matching (market/limit, price-time priority)
- Depth/snapshot query API
- Event model and stream
- IOC/FOK time-in-force semantics
- Iceberg orders (display vs. reserve)
- Self-trade prevention policies
- Optional persistent event log interface
- Pluggable risk checks at ingestion boundary
- Batched commands and bulk cancels

## Contributing

Contributions are welcome! If you find a bug or want to propose a feature, please open an issue or PR. For larger changes, start a discussion first to agree on API and invariants.

Coding guidelines:

- Keep the core deterministic and side-effect free
- Favor explicit types and invariants over ad-hoc checks
- Document performance assumptions and complexity of hot paths
- Add tests alongside features and bug fixes, including property tests where applicable

## License

This project is licensed under the MIT License. See `LICENSE` for details.
