use crate::types::*;

pub struct Trade {
    pub taker_order_id: OrderId,
    pub maker_order_id: OrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub timestamp: u64
}