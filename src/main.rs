use crossbeam_queue::SegQueue;
use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use std::{
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering}, 
        Arc
    },
    time::{SystemTime, UNIX_EPOCH}
};
use thiserror::Error;

// Unique Identifier for each order (globally unique)
pub type OrderId = u64;

// Price represented as fixed-point integer.
pub type Price = u64;

// Quantity of the asset
pub type Quantity = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderStatus {
    Active = 0,
    PartiallyFilled = 1, 
    Filled = 2,
    Cancelled = 3,
}

impl From<u8> for OrderStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => OrderStatus::Active,
            1 => OrderStatus::PartiallyFilled,
            2 => OrderStatus::Filled,
            3 => OrderStatus::Cancelled,
            _ => OrderStatus::Active, // Default fallback!
        }
    }
}

#[derive(Error, Debug)]
pub enum OrderBookError {
    #[error("Order not found: {0}")]
    OrderNotFound(OrderId),
    
    #[error("Invalid price: {0}")]
    InvalidPrice(Price),
    
    #[error("Invalid quantity: {0}")]
    InvalidQuantity(Quantity),
    
    #[error("Order already cancelled: {0}")]
    AlreadyCancelled(OrderId),
    
    #[error("Market order cannot be placed in empty book")]
    EmptyBook,
}

#[derive(Debug)]
struct Bid {

}

#[derive(Debug)]
struct Ask {

}

/// Represents a single order in the order book
///
/// Design Notes:
/// - Arc wrapper for shared ownership across multiple data structures
/// - Immutable fields: id, side, order_type, price, timestamp
/// - Mutable fields: remaining_quantity, status
/// - Original quantity stored for record-keeping
#[derive(Debug)]
pub struct Order {
    /// Unique Order identifier
    pub order_id: OrderId,

    /// Buy or Sell
    pub side: Side,

    /// Limit or Market
    pub order_type: OrderType,

    /// Price level (0 for market orders)
    pub price: Price,

    /// Original quantity when order was created
    pub original_quantity: Quantity,

    /// Remaining quantity (atomic for concurrent updates)
    /// This gets decremented during matching
    pub remaining_quantity: AtomicU64,

    /// Current status of the order
    pub status: AtomicU8,

    /// Timestamp for time priority (nanoseconds since epoch!)
    pub timestamp: u64,
}

impl Order {

    /// Create a new order
    pub fn new(
        order_id: OrderId,
        side: Side,
        order_type: OrderType,
        price: Price,
        quantity: Quantity,
    ) -> Self {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;

        Self {
            order_id,
            side,
            order_type,
            price,
            original_quantity: quantity,
            remaining_quantity: AtomicU64::new(quantity),
            status: AtomicU8::new(OrderStatus::Active as u8),
            timestamp,
        }
    }


    /// Get current remaining quantity
    pub fn get_remaining_quantity(&self) -> Quantity {
        self.remaining_quantity.load(Ordering::Acquire)
    }


    /// Get the current status
    pub fn get_status(&self) -> OrderStatus {
        OrderStatus::from(self.status.load(Ordering::Acquire))
    }

    /// Fills a portion or full order.
    ///
    /// Returns the actual quantity filled. (Can be lesser than requested)
    pub fn fill(&self, quantity: Quantity) -> Quantity {
        let mut current = self.remaining_quantity.load(Ordering::Acquire);

        loop {

            if current == 0 {
                // Order is already filled. Return 0.
                return 0;
            }

            let fill_amount = current.min(quantity);
            let new_remaining = current - fill_amount;

            match self.remaining_quantity.compare_exchange_weak(
                current,
                new_remaining,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) -> {
                    // Successflly updated remaining quantity
                    if new_remaining == 0 {
                        self.status.store(OrderStatus::Filled as u8, Ordering::Release);
                    } else {
                        self.status.store(OrderStatus::PartiallyFilled as u8, Ordering::Release)
                    }
                    return fill_amount;
                }
                Err(actual) => {
                    // Another thread modified it, retry with new value!
                    current = actual;
                }
            }

        }
    }

    /// Cancel the order
    /// Returns true if cancel is successful, else false if already filled/cancelled.
    pub fn cancel(&self) -> bool {
        let current_status = self.status.load(Ordering::Acquire);

        if current_status == OrderStatus::Filled as u8 || current_status == OrderStatus::Cancelled as u8 {  
            return false;
        }

        self.status.store(OrderStatus::Cancelled as u8, Ordering::Release);
        true
    }

    /// Checks if the order is still active (not filled or cancelled)
    pub fn is_active(&self) -> bool {
        let status = self.get_status();
        status == OrderStatus::Active || status == OrderStatus::PartiallyFilled
    }
}

/// Represents all orders at a specific price level
///
/// Design Notes:
/// - FIFO queue for time priority within the price level
/// - SegQueue is lock-free MPMC queue from crossbeam
/// - Cache total quantity for quick depth queries
pub struct PriceLevel {
    pub price: Price,

    /// FIFO queue of orders at this price
    /// Arc<Order> allows shared ownership with lookup map
    pub orders: SegQueue<Arc<Order>>,

    /// Cached total quanity at this level (for depth queries)
    /// Updated opportunistically during matching
    pub total_quantity: AtomicU64,
}

impl PriceLevel {
    pub new(price: PriceLevel) -> Self {
        Self {
            price: price,
            orders: SegQueue::new(),
            total_quantity: AtomicU64::new(0),
        }
    }

    /// Add an order to this price level
    pub fn add_order(&self, order: Arc<Order>) {
        // Get the current remaining quantity of the order
        let quantity = order.get_remaining_quantity();
        self.orders.push(order);
        // Update the total quantity at this price level opportunistically
        self.total_quantity.fetch_add(quantity, Ordering::Relaxed);
    }

    /// Get approximate total quantity (maybe slightly stale)
    /// TODO: Make this more accurate
    pub fn get_total_quantity(&self) -> Quantity {
        self.total_quantity.load(Ordering::Relaxed)
    }
}


/// Main order book structure
///
/// Architecture:
/// - Separate skip lists for bids and asks (different sort orders to match reverse)
/// - Skip lists maintain price ordering automatically
/// - DashMap for O(1) order lookup by ID 
/// - Atomic counter for generating unique order IDs.
pub struct OrderBook {
    /// Buy orders: Higher price = better (descending order)
    /// Key is Price, but we will use Reverse<Price> or custom comparator
    /// Value is Arc<PriceLevel> for shared ownership and access
    bids: SkipMap<Price, Arc<PriceLevel>>,

    /// Sell orders: Lower price = better (Ascending order)
    /// Natural ordering works here
    asks: SkipMap<Price, Arc<PriceLevel>>,

    /// Fast lookup map: OrderId -> Arc<Order>
    /// used for cancellations and queries
    order_lookup: DashMap<OrderId, Arc<Order>>,

    /// Atomic counter for generating unique order IDs
    next_order_id: AtomicU64,
}

impl OrderBook {
    /// Creates a new empty Orderbook
    pub fn new() -> Self {
        Self {
            bids: SkipMap::new(),
            asks: SkipMap::new(),
            order_lookup: DashMap::new(),
            next_order_id: AtomicU64::new(1),
        }
    }

    /// Generate a unique order id.
    fn generate_order_id(&self) -> OrderId {
        self.next_order_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Get the best bid price (highest buy price)
    pub fn best_bid(&self) -> Option<Price> {
        /*
            For bids, we want the HIGHEST price
            SkipMap iterates in ascending order by default
            We need to get the last entry
        */
        self.bids.iter().next_back().map(|entry| *entry.key())
    }

    /// Get the best ask price (lowest sell price)
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.iter().next().map(|entry| *entry.key())
    }

    // Get current Spread
    pub fn spread(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask.saturating_sub(bid)),
            _ => None,
        }
    }

    /// Get total quantity at all bid levels
    pub fn total_bid_quantity(&self) -> Quantity {
        self.bids.iter().map(|entry| entry.value().get_total_quantity()).sum()
    }

    /// Get total quantity at all ask levels
    pub fn total_ask_quantity(&self) -> Quantity {
        self.asks.iter().map(|entry| entry.value().get_total_quantity()).sum()
    }

    /// Get total quantity at all levels (bids + asks)
    pub fn total_quantity(&self) -> Quantity {
        self.total_bid_quantity() + self.total_ask_quantity()
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}


fn main() {
    println!("Hello, world!");
}
