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
        self.total_quantity.load(Ordering::Relaxed)
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
    bids: RwLock<BTreeMap<Price, Arc<PriceLevel>>>,

    /// Sell orders: Lower price = better (Ascending order)
    /// Natural ordering works here
    asks: RwLock<BTreeMap<Price, Arc<PriceLevel>>>,

    /// Fast lookup map: OrderId -> (Side, Price) to PriceLevel a bit faster
    order_lookup: DashMap<OrderId, (Side, Price)>,

    /// Atomic counter for generating unique order IDs
    next_order_id: AtomicU64,

    /// Trade receiver for events
    trade_tx: Sender<Trade>
}

impl OrderBook {
    /// Creates a new empty Orderbook
    pub fn new(trade_tx: Sender<Trade>) -> Self {
        Self {
            bids: RwLock::new(BTreeMap::new()),
            asks: RwLock::new(BTreeMap::new()),
            order_lookup: DashMap::new(),
            next_order_id: AtomicU64::new(1),
            trade_tx: trade_tx,
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
            We need to get the last entry
        */
        let bids = self.bids.read();
        bids.iter().next_back().map(|entry| entry.1.price)
    }

    /// Get the best ask price (lowest sell price)
    pub fn best_ask(&self) -> Option<Price> {
        let asks = self.asks.read();
        asks.iter().next().map(|entry| entry.1.price)
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
        let bids = self.bids.read();
        bids.iter().map(|entry| entry.1.get_total_quantity()).sum()
    }

    /// Get total quantity at all ask levels
    pub fn total_ask_quantity(&self) -> Quantity {
        let asks = self.asks.read();
        asks.iter().map(|entry| entry.1.get_total_quantity()).sum()
    }

    /// Get total quantity at all levels (bids + asks)
    pub fn total_quantity(&self) -> Quantity {
        self.total_bid_quantity() + self.total_ask_quantity()
    }


    fn get_or_create_level(map_lock: &RwLock<BTreeMap<Price, Arc<PriceLevel>>>, price: Price) -> Arc<PriceLevel> {
        {
            let map = map_lock.read();
            if let Some(level) = map.get(&price) {
                return level.clone();
            }
        }

        // Upgrade: take write lock and insert if missing.
        let mut map = map_lock.write();
        if let Some(level) = map.get(&price) {
            level.clone()
        } else {
            let level = Arc::new(PriceLevel::new(price));
            map.insert(price, level.clone());
            level
        }
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

       // taker matches against the opposite orderbook
       match side {
            Side::Buy => {
                let asks_read = self.asks.read();
                let mut candidate_prices: Vec<Price> = asks_read.range(..=price).map(|(p, _) | *p).collect();
                drop(asks_read);

                for level_price in candidate_prices {
                    let level_opt = {
                        let asks = self.asks.read();
                        asks.get(&level_price).cloned()
                    };

                    if level_opt.is_none() {
                        continue;
                    }

                    let level = level_opt.unwrap();

                    // Lock the price level's queue and process FIFO manner.
                    loop {
                        // pop and try to fill
                        let maybe_maker = {
                            let mut q = level.orders.lock();
                            q.pop_front()
                        };

                        let maker = match maybe_maker {
                            Some(m_order) => m_order,
                            None => break,
                        };

                        // If the current order is not active, subtract remaining and continue
                        // Lazy cleanup.
                        if !matches!(maker.get_status(), OrderStatus::Active | OrderStatus::PartiallyFilled) {
                            let rem = maker.get_remaining_quantity();
                            level.total_quantity.fetch_sub(rem, Ordering::AcqRel);
                            continue;
                        }

                        // Compute Match quantity
                        let taker_left = order.get_remaining_quantity();
                        if taker_left == 0 {
                            // push maker back to front? No — we popped it and must reinsert since taker didn't consume it
                            // but we intentionally popped it; since we haven't filled it, we should push_front to preserve FIFO:
                            let mut q = level.orders.lock();
                            q.push_front(maker.clone());
                            break;
                        }

                        let maker_left = maker.get_remaining_quantity();
                        let requested = min(taker_left, maker_left);
                        if requested == 0 {
                            continue;
                        }

                        // Execute fills
                        let filled_maker = maker.fill(requested);

                        if filled_maker == 0 {
                            // Another thread would have filled it. Continue to the next one.
                            continue;
                        }

                        level.total_quantity.fetch_sub(filled_maker, Ordering::AcqRel);

                        let filled_taker = order.fill(filled_maker);
                        // Emit trade event
                        let trade = Trade {
                            taker_order_id: order_id,
                            maker_order_id: maker.order_id,
                            price: level.price,
                            quantity: filled_taker,
                            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                        };

                        let _ = self.trade_tx.send(trade);

                        if maker.get_remaining_quantity() > 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker);
                        }

                        // If taker is fully filled, break
                        if order.get_remaining_quantity() == 0 {
                            break;
                        }
                    }

                    // After finishing the level, remove it if that level is empty.
                    if level.is_empty() {
                        let mut asks = self.asks.write();
                        if let Some(existing) = asks.get(&level_price) {
                            if Arc::ptr_eq(existing, &level) && existing.is_empty() {
                                asks.remove(&level_price);
                            }
                        }
                    }

                    if order.get_remaining_quantity() == 0 {
                        break;
                    }
                }

                // If remaining, insert into bids
                if order.get_remaining_quantity() > 0 {
                    let level = Self::get_or_create_level(&self.bids, price);
                    level.push_order(order.clone());
                    self.order_lookup.insert(order_id, (side, price));
                }
            },
            Side::Sell => {
                // Symmetric. But read through bids in descending order
                let bids_read = self.bids.read();
                let mut candidate_prices: Vec<Price> = bids_read.keys().cloned().collect();
                drop(bids_read);

                candidate_prices.sort_by(|a,b| b.cmp(a)); // sort in descending order

                for level_price in candidate_prices {
                    let level_opt = {
                        let bids = self.bids.read();
                        bids.get(&level_price).cloned()
                    };

                    if level_opt.is_none() {
                        continue;
                    }

                    let level = level_opt.unwrap();

                    loop {

                        let maybe_maker = {
                            let mut q = level.orders.lock();
                            q.pop_front()
                        };

                        let maker = match maybe_maker {
                            Some(m) => m,
                            None => break
                        };

                        if !matches!(maker.get_status(), OrderStatus::Active | OrderStatus::PartiallyFilled ){
                            let rem = maker.get_remaining_quantity();
                            level.total_quantity.fetch_sub(rem, Ordering::AcqRel);
                            continue;
                        }

                        let taker_left = order.get_remaining_quantity();
                        if taker_left == 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker.clone());
                            continue;
                        }

                        let maker_left = maker.get_remaining_quantity();
                        let requested = min(taker_left, maker_left);
                        if requested == 0 { 
                            continue; 
                        }

                        let filled_maker = maker.fill(requested);
                        if filled_maker == 0 {
                            continue;
                        }

                        level.total_quantity.fetch_sub(filled_maker, Ordering::AcqRel);
                        let filled_taker = order.fill(filled_maker);

                        let trade = Trade {
                            taker_order_id: order_id,
                            maker_order_id: maker.order_id,
                            price: level.price, 
                            quantity: filled_taker,
                            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                        };

                        let _ = self.trade_tx.send(trade);

                        if maker.get_remaining_quantity() > 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker);
                        }
                        if order.get_remaining_quantity() == 0 {
                            break;
                        }
                    }

                    if level.is_empty() {
                        let mut bids = self.bids.write();
                        if let Some(existing) = bids.get(&level_price) {
                            if Arc::ptr_eq(existing, &level) && existing.is_empty() {
                                bids.remove(&level_price);
                            }
                        }
                    }
                    if order.get_remaining_quantity() == 0 { 
                        break; 
                    }
                }

                if order.get_remaining_quantity() > 0 {
                    let level = Self::get_or_create_level(&self.asks, price);
                    level.push_order(order.clone());
                    self.order_lookup.insert(order_id, (side, price));
                }
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

        let order_id = self.generate_order_id();
        let order = Arc::new(Order::new(order_id, side, OrderType::Market, 0, quantity));
        let mut filled_total: Quantity = 0;

        match side {
            Side::Buy => {
                // walk asks ascending
                let asks_read = self.asks.read();
                let mut candidate_prices: Vec<Price> = asks_read.keys().cloned().collect();
                drop(asks_read);

                candidate_prices.sort(); // ascending
                for level_price in candidate_prices {
                    let level_opt = {
                        let asks = self.asks.read();
                        asks.get(&level_price).cloned()
                    };
                    if level_opt.is_none() { 
                        continue; 
                    }
                    let level = level_opt.unwrap();

                    loop {
                        let maybe_maker = { 
                            let mut q = level.orders.lock(); 
                            q.pop_front() 
                        };

                        let maker = match maybe_maker { 
                            Some(m) => m, 
                            None => break 
                        };

                        if !matches!(maker.get_status(), OrderStatus::Active | OrderStatus::PartiallyFilled) {
                            let rem = maker.get_remaining_quantity();
                            level.total_quantity.fetch_sub(rem, Ordering::AcqRel);
                            continue;
                        }
                        let maker_left = maker.get_remaining_quantity();

                        if maker_left == 0 { 
                            continue; 
                        }
                        let taker_left = order.get_remaining_quantity();
                        if taker_left == 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker.clone());
                            break;
                        }
                        let requested = min(taker_left, maker_left);
                        let filled_maker = maker.fill(requested);

                        if filled_maker == 0 { 
                            continue; 
                        }
                        level.total_quantity.fetch_sub(filled_maker, Ordering::AcqRel);
                        let filled_taker = order.fill(filled_maker);
                        filled_total += filled_taker;

                        let trade = Trade {
                            taker_order_id: order_id,
                            maker_order_id: maker.order_id,
                            price: level.price,
                            quantity: filled_taker,
                            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                        };
                        let _ = self.trade_tx.send(trade);

                        if maker.get_remaining_quantity() > 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker);
                        }
                        if order.get_remaining_quantity() == 0 { break; }
                    }
                    if order.get_remaining_quantity() == 0 { break; }
                }

                // Market orders NEVER go into book, simply return filled_total
            }

            Side::Sell => {
                // symmetric: process bids in descending order
                let bids_read = self.bids.read();
                let mut candidate_prices: Vec<Price> = bids_read.keys().cloned().collect();
                drop(bids_read);
                candidate_prices.sort_by(|a,b| b.cmp(a)); // descending

                for level_price in candidate_prices {
                    let level_opt = {
                        let bids = self.bids.read();
                        bids.get(&level_price).cloned()
                    };

                    if level_opt.is_none() { 
                        continue; 
                    }
                    let level = level_opt.unwrap();

                    loop {
                        let maybe_maker = { 
                            let mut q = level.orders.lock(); 
                            q.pop_front() 
                        };
                        
                        let maker = match maybe_maker { 
                            Some(m) => m, 
                            None => break 
                        };

                        if !matches!(maker.get_status(), OrderStatus::Active | OrderStatus::PartiallyFilled) {
                            let rem = maker.get_remaining_quantity();
                            level.total_quantity.fetch_sub(rem, Ordering::AcqRel);
                            continue;
                        }
                        let maker_left = maker.get_remaining_quantity();

                        if maker_left == 0 { 
                            continue; 
                        }
                        let taker_left = order.get_remaining_quantity();
                        if taker_left == 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker.clone());
                            break;
                        }
                        let requested = min(taker_left, maker_left);
                        let filled_maker = maker.fill(requested);

                        if filled_maker == 0 { 
                            continue; 
                        }
                        level.total_quantity.fetch_sub(filled_maker, Ordering::AcqRel);
                        let filled_taker = order.fill(filled_maker);
                        filled_total += filled_taker;
                        let trade = Trade {
                            taker_order_id: order_id,
                            maker_order_id: maker.order_id,
                            price: level.price,
                            quantity: filled_taker,
                            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
                        };
                        let _ = self.trade_tx.send(trade);
                        if maker.get_remaining_quantity() > 0 {
                            let mut q = level.orders.lock();
                            q.push_front(maker);
                        }
                        if order.get_remaining_quantity() == 0 { break; }
                    }
                    if order.get_remaining_quantity() == 0 { break; }
                }
            }
        }

        Ok((order_id, filled_total))
    }


    // Cancel an order by id. Deterministic and safe.
    /// Returns Ok(remaining_qty) if cancelled; Err if not found / already filled.
    pub fn cancel_order(&self, order_id: OrderId) -> Result<Quantity, String> {
        // Find order meta in lookup
        if let Some(entry) = self.order_lookup.get(&order_id) {
            let (side, price) = *entry.value();
            // Get PriceLevel
            let maybe_level = {
                match side {
                    Side::Buy => self.bids.read().get(&price).cloned(),
                    Side::Sell => self.asks.read().get(&price).cloned(),
                }
            };

            if let Some(level) = maybe_level {
                // Attempt atomic cancel of status first
                // But we must remove from queue to prevent matching
                // coord: perform cancel atomic, then remove from queue
                // We'll try to fetch the order object via scanning queue and compare id.
                // If other thread is filling simultaneously, cancel_atomic might fail; handle that.
                // Acquire queue lock and locate/remove
                let removed = level.remove_order(order_id);
                if let Some(rem_qty) = removed {
                    // mark status (best-effort)
                    // Try to set status to Cancelled if not already Filled
                    // We cannot fetch order object easily here; but we can attempt to set in lookup map?
                    // For simplicity, get the order object from elsewhere: the caller should also maintain pointer to Arc<Order>.
                    // We can find Arc<Order> by scanning or you persisted a pointer elsewhere.
                    // In this simplified design we return remaining qty removed and remove lookup entry.
                    self.order_lookup.remove(&order_id);
                    return Ok(rem_qty);
                } else {
                    return Err("Order not found in price level queue (maybe already matched)".into());
                }
            } else {
                return Err("Price level not found".into());
            }
        } else {
            return Err("Order not found".into());
        }
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
