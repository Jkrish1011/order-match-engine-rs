# match-engine-rust

Pure matching engine logic for centralized order books, written in Rust. This project focuses exclusively on the core matching logic so it can be embedded in other systems: exchanges, broker simulators, market microstructure research tools, backtesters, and educational projects.

The engine is designed for determinism, performance, and composability. It supports price-time priority, limit and market orders, cancels, partial fills, and efficient book queries (BBO, depth, snapshots). Concurrency primitives are used where appropriate to support multi-producer inputs and concurrent reads.

Status: active development. The intent is a full, self-contained, production-grade matching core with a clean API surface you can import as a library.

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

## Types

Core types (some already present in `src/main.rs`):

- `OrderId = u64` — globally unique identifier
- `Price = u64` — fixed-point integer; external code is responsible for scaling/formatting
- `Quantity = u64` — fixed-point integer quantity
- `Side` — `Buy` or `Sell`
- `OrderType` — `Limit` or `Market`
- `OrderStatus` — `Active`, `PartiallyFilled`, `Filled`, `Cancelled`
- `OrderBookError` — domain errors (e.g., invalid inputs, not found)

You can select a tick size and unit scaling appropriate to your instrument universe in your integration layer.

## Order matching rules

- Price priority: better prices match first (`Buy` prefers higher prices; `Sell` prefers lower prices)
- Time priority: within the same price level, earlier orders match first (FIFO)
- A market order crosses immediately against the best available opposite side until it is filled or the book exhausts
- A limit order matches any immediately-crossing liquidity and then rests if unfilled
- Post-match accounting updates order statuses and emits events

## API

Below is the intended ergonomic API of the engine when used as a library. Method names are indicative; minor differences may exist while development is in progress.

### Creating an order book

```rust
use match_engine_rust::{OrderBook, Side, OrderType, Price, Quantity};

let mut book = OrderBook::new("BTC-USD", /*tick_size=*/ 100, /*quantity_step=*/ 1000);
```

### Placing orders

```rust
use match_engine_rust::{NewOrder, OrderId, Side, OrderType};

let order_id: OrderId = 1;
let ack = book.place(NewOrder {
    order_id,
    side: Side::Buy,
    order_type: OrderType::Limit,
    price: 50_000_00,      // 50000.00 with 1/100 scaling
    quantity: 10_000_000,  // 10.000000 with 1/1_000_000 scaling
});

assert!(ack.accepted);
```

For market orders, set `order_type: OrderType::Market` and `price` is ignored.

### Cancelling and amending

```rust
book.cancel(order_id)?;           // Cancel by OrderId
book.replace(order_id, 49_900_00, 8_000_000)?; // Optional replace: new price/qty
```

### Querying the book

```rust
let bbo = book.bbo();
println!("best bid: {:?}, best ask: {:?}", bbo.bid, bbo.ask);

let depth = book.depth(/*levels=*/10);
for level in depth.bids { println!("bid @{} x{}", level.price, level.quantity); }
for level in depth.asks { println!("ask @{} x{}", level.price, level.quantity); }

let snapshot = book.snapshot(); // Full, side-sorted, stable snapshot
```

### Receiving events

Every command (new, cancel, replace) results in zero or more events. Subscribe to the event stream to capture fills, partial fills, trades, cancels, and rejections.

```rust
use match_engine_rust::{Event, Trade, Fill, CancelAck};

while let Some(ev) = book.next_event() {
    match ev {
        Event::Trade(Trade { price, quantity, maker_order_id, taker_order_id, .. }) => { /* ... */ }
        Event::Fill(Fill { order_id, filled, remaining, .. }) => { /* ... */ }
        Event::CancelAck(CancelAck { order_id, .. }) => { /* ... */ }
        Event::Reject(err) => { eprintln!("rejected: {err}"); }
        _ => {}
    }
}
```

## Examples

End-to-end toy example (limit vs. limit cross):

```rust
use match_engine_rust::*;

let mut book = OrderBook::new("ETH-USD", 1, 1);

// Maker adds resting ask
book.place(NewOrder { order_id: 1, side: Side::Sell, order_type: OrderType::Limit, price: 2000, quantity: 5 });

// Taker crosses with a buy limit >= best ask
book.place(NewOrder { order_id: 2, side: Side::Buy, order_type: OrderType::Limit, price: 2100, quantity: 3 });

// Consume events
let mut traded = 0;
while let Some(ev) = book.next_event() { if let Event::Trade(t) = ev { traded += t.quantity; } }
assert_eq!(traded, 3);

// Remaining maker quantity
let remaining = book.order(1).unwrap().get_remaining_quantity();
assert_eq!(remaining, 2);
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
