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