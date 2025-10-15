use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use std::{
    collections::{BTreeMap, VecDeque}, sync::{
        atomic::{AtomicU64, AtomicU8, Ordering}, 
        Arc
    }, thread::current, time::{SystemTime, UNIX_EPOCH}
};
use thiserror::Error;
use std::cmp::min;
use parking_lot::{Mutex, RwLock};
use crossbeam_channel::{unbounded, Sender};

mod types;
mod events;

use types::*;
use events::*;


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
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;

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
                Ok(_) => {
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
    /// This is done in an atomic manner.
    pub fn cancel(&self) -> bool {
        let mut current_status = self.status.load(Ordering::Acquire);

        loop {
            if current_status == OrderStatus::Filled as u8 || current_status == OrderStatus::Cancelled as u8 {
                return false;
            }

            match self.status.compare_exchange_weak(
                current_status,
                OrderStatus::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire
            ) {
                Ok(_) => {
                    // Successfully updated status
                    return true;
                }
                Err(actual) => {
                    current_status = actual;
                }
            }
        }
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
/// - Mutex for thread safety and VecDeque for FIFO queue
/// - Cache total quantity for quick depth queries
pub struct PriceLevel {
    pub price: Price,

    /// FIFO queue of orders at this price
    /// Arc<Order> allows shared ownership with lookup map
    pub orders: Mutex<VecDeque<Arc<Order>>>,

    /// Cached total quanity at this level (for depth queries)
    /// Updated opportunistically during matching
    pub total_quantity: AtomicU64,
}

impl PriceLevel {
    pub fn new(price: Price) -> Self {
        Self {
            price: price,
            orders: Mutex::new(VecDeque::new()),
            total_quantity: AtomicU64::new(0),
        }
    }

    /// Add an order to this price level
    pub fn push_order(&self, order: Arc<Order>) {
        // Get the current remaining quantity of the order
        let vecDeque = &mut *self.orders.lock();
        let quantity = order.get_remaining_quantity();
        vecDeque.push_back(order);
        // Update the total quantity at this price level opportunistically
        self.total_quantity.fetch_add(quantity, Ordering::AcqRel);
    }

    /// Pop the front order from this price level
    pub fn pop_front(&self) -> Option<Arc<Order>> {
        let vecDeque = &mut *self.orders.lock();
        let order =vecDeque.pop_front();
        order
    }

    pub fn remove_order(&self, order_id: OrderId) -> Option<Quantity> {
        let vecDeque = &mut *self.orders.lock();

        let mut remaining_quantity: Option<Quantity> = None;
        let mut i: usize = 0;
        while i < vecDeque.len() {
            if vecDeque[i].order_id == order_id {
                let curr_order = vecDeque.remove(i).unwrap();
                let curr_remaining_q = curr_order.get_remaining_quantity();
                remaining_quantity = Some(curr_remaining_q);
                break;
            }

            i+=1;
        }

        // Subtract the remaining quantity from the total quantity
        if let Some(rem) = remaining_quantity {
            self.total_quantity.fetch_sub(rem, Ordering::AcqRel);
        }

        return remaining_quantity
    }   

    /// Get approximate total quantity (maybe slightly stale)
    /// TODO: Make this more accurate
    pub fn get_total_quantity(&self) -> Quantity {
        self.total_quantity.load(Ordering::AcqRel)
    }

    pub fn is_empty(&self) -> bool {
        let vecDeque = &mut *self.orders.lock();
        vecDeque.is_empty()
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

    /// Submit a limit order
    pub fn submit_limit_order(
        &self,
        side: Side,
        price: Price,
        quantity: Quantity,
    ) -> Result<OrderId, OrderBookError> {
        if price == 0 {
            return Err(OrderBookError::InvalidPrice(price));
        }

        if quantity <= 0 {
            return Err(OrderBookError::InvalidQuantity(quantity));
        }

        let order_id = self.generate_order_id();
        let order = Arc::new(Order::new(order_id, side, OrderType::Limit, price, quantity));

        let opposite_side = match side {
            Side::Buy => {
                &self.asks
            }
            Side::Sell => {
                &self.bids
            }
        };

        for entry in opposite_side.iter() {
            let price = *entry.key();
            let price_level: &Arc<PriceLevel> = entry.value();
            loop {
                match price_level.orders.pop() {
                    Some(resting_order) => {
                        if !resting_order.is_active() {
                            // Not active. Remove from adding it back to the orderbook.
                            // Lazy cleanup.
                            continue;
                        }

                        let match_quantity = min(
                            order.get_remaining_quantity(),
                            resting_order.get_remaining_quantity()
                        );

                        // Execute the match
                        resting_order.fill(match_quantity);
                        order.fill(match_quantity);
                        
                        if resting_order.get_remaining_quantity() > 0 {
                            price_level.orders.push(resting_order);
                        }

                        if order.get_remaining_quantity() == 0 {
                            break;
                        }
                    }
                    None => {
                        // Price level exhausted
                        break;
                    }
                }

                if price_level.orders.is_empty() {
                    opposite_side.remove(&price_level.price);
                }

                if order.get_remaining_quantity() == 0 {
                    break; // Fully matched
                }
            }

            // Update the orderbook
            if order.get_remaining_quantity() > 0 {
                opposite_side.insert(price_level.price, price_level.clone());
            }


        }
        Ok(order_id)
    }

    /// Submit a market order
    /// Returns the OrderId and actual filled quantity
    /// Market orders will be filled at the best available price and do not enter the orderbook.
    pub fn submit_market_order(
        &self, 
        side: Side,
        quantity: Quantity,
    ) -> Result<(OrderId, Quantity), OrderBookError> {
        if quantity <= 0 {
            return Err(OrderBookError::InvalidQuantity(quantity));
        }

        let opposite_side = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };

        if opposite_side.is_empty() {
            return Err(OrderBookError::EmptyBook);
        }

        let order_id = self.generate_order_id();
        let order = Arc::new(Order::new(order_id, side, OrderType::Market, 0, quantity));

        let iter: Box<dyn Iterator<Item = _> + '_> = match side {
            Side::Buy => Box::new(opposite_side.iter()), // Ascending for asks.
            Side::Sell => Box::new(opposite_side.iter().rev()) // Descending for bids.
        };

        for entry in iter {
            let price = *entry.key();
            let price_level: &Arc<PriceLevel> = entry.value();

            loop {
                match price_level.orders.pop() {
                    Some(resting_order) => {
                        if !resting_order.is_active() {
                            // Not active. Remove from adding it back to the orderbook.
                            // Lazy cleanup.
                            continue;
                        }

                        let match_quantity = min(
                            order.get_remaining_quantity(),
                            resting_order.get_remaining_quantity()
                        );

                        // Execute the match
                        resting_order.fill(match_quantity);
                        order.fill(match_quantity);
                        
                        if resting_order.get_remaining_quantity() > 0 {
                            price_level.orders.push(resting_order);
                        }

                        if order.get_remaining_quantity() == 0 {
                            break;
                        }
                    }
                    None => {
                        // Price level exhausted
                        break;
                    }
                }

                if order.get_remaining_quantity() == 0 {
                    break; // Fully matched
                }
            }

            
        }

        Ok((order_id, 0 as u64))


    }

    /// Cancel an order by ID
    pub fn cancel_order(&self, order_id: OrderId) -> Result<(), OrderBookError> {
        // TODO: Implement in Phase 2
        todo!("Implement order cancellation")
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}


fn main() {
    println!("Hello, This is a sample orderbook!");
    println!("Run the testcases to see this in action");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_creation() {
        let order = Order::new(1, Side::Buy, OrderType::Limit, 10_000, 100);
        assert_eq!(order.order_id, 1);
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.order_type, OrderType::Limit);
        assert_eq!(order.price, 10_000);
        assert_eq!(order.original_quantity, 100);
        assert_eq!(order.get_remaining_quantity(), 100);
        assert_eq!(order.get_status(), OrderStatus::Active);
    }

    #[test]
    fn test_order_fill() {
        let order = Order::new(1, Side::Buy, OrderType::Limit, 10_000, 100);

        // Partial Fill
        let filled = order.fill(30);
        assert_eq!(filled, 30);
        assert_eq!(order.get_remaining_quantity(), 70);
        assert_eq!(order.get_status(), OrderStatus::PartiallyFilled);

        // Complete fill
        let filled = order.fill(70);
        assert_eq!(filled, 70);
        assert_eq!(order.get_remaining_quantity(), 0);
        assert_eq!(order.get_status(), OrderStatus::Filled);
        
        // Try to fill already filled order
        let filled = order.fill(10);
        assert_eq!(filled, 0);
    }

    #[test]
    fn test_order_cancel() {
        let order = Order::new(1, Side::Buy, OrderType::Limit, 10000, 100);
        
        // Cancel active order
        assert!(order.cancel());
        assert_eq!(order.get_status(), OrderStatus::Cancelled);
        
        // Try to cancel again
        assert!(!order.cancel());
    }
    
    #[test]
    fn test_order_book_creation() {
        let book = OrderBook::new();
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.spread(), None);
    }
    
    #[test]
    fn test_order_id_generation() {
        let book = OrderBook::new();
        let id1 = book.generate_order_id();
        let id2 = book.generate_order_id();
        let id3 = book.generate_order_id();
        
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_end_to_end_orderbook() {
        let orderbook = OrderBook::new();
        let order = Order::new(1, Side::Buy, OrderType::Limit, 10_000, 100);
        orderbook.submit_limit_order(Side::Buy, 10_000, 100);
        
        let buy_order = Order::new(2, Side::Buy, OrderType::Limit, 10_000, 100);
        let sell_order = Order::new(3, Side::Sell, OrderType::Limit, 10_000, 100);

        orderbook.submit_limit_order(Side::Buy, 10_000, 100);
        orderbook.submit_limit_order(Side::Sell, 10_000, 100);
        
        orderbook.spread();
        // assert_eq!(orderbook.spread(), Some(0));
        // assert_eq!(orderbook.best_bid(), Some(10_000));
        // assert_eq!(orderbook.best_ask(), Some(10_000));
    }
}
