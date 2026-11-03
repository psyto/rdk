//! `cargo run -p rdk-clob --example quickstart`
//!
//! A maker rests an ask, a taker buys part of it at market.

use rdk_clob::{AccountId, Book, Order, OrderId, OrderType, Price, Qty, Side};

fn main() {
    let mut book = Book::new();
    // Prices are minor units: for a USDC market, Price(1_000_000) = $1.00.
    book.submit(Order {
        id: OrderId(1),
        account: AccountId(1),
        side: Side::Sell,
        qty: Qty(10),
        order_type: OrderType::Limit { price: Price(100_000_000) },
    });
    let result = book.submit(Order {
        id: OrderId(2),
        account: AccountId(2),
        side: Side::Buy,
        qty: Qty(4),
        order_type: OrderType::Market,
    });
    for fill in &result.fills {
        println!("filled {} at {} (maker {:?}, taker {:?})", fill.qty, fill.price, fill.maker_account, fill.taker_account);
    }
    println!("best ask left: {:?}", book.best_ask_with_qty());
}
