//! CLOB read-path latency under a concurrent writer: `Arc<Mutex<Book>>` vs `ArcSwap<Book>`.
//!
//! This reproduces the openhl read path — the `read_best_bid` precompile locks
//! the shared book (`Arc<Mutex<Book>>` in `live_node.rs`) that the block-builder
//! also locks to apply orders — and measures what a reader actually feels while a
//! writer is busy.
//!
//! - `Arc<Mutex<Book>>`: the reader blocks whenever the writer holds the lock.
//! - `ArcSwap<Book>`:     the reader never blocks (lock-free load); the writer
//!                        instead pays a full `Book` clone per published version.
//!
//! It measures reader **tail latency** (p50/p99/p99.9/max), because the interesting
//! cost is not the mean — it's the spikes a trader feels when a read lands during a
//! writer's lock hold. Numbers are for THIS machine; run it yourself:
//!
//! ```text
//! cargo bench -p rdk-clob --bench read_contention
//! ```

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use arc_swap::ArcSwap;
use rdk_clob::{AccountId, Book, Order, OrderId, OrderType, Price, Qty, Side};

const READS: usize = 200_000;
const DEPTH: u64 = 500;

/// Seed both sides of the book so `best_bid` is real and crossing submits do work.
fn seed_book() -> Book {
    let mut b = Book::new();
    for i in 0..DEPTH {
        b.submit(Order {
            id: OrderId(i),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(1000 - i) },
        });
        b.submit(Order {
            id: OrderId(1_000_000 + i),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(1001 + i) },
        });
    }
    b
}

/// A small crossing order: matches a little top-of-book depth, so the writer's
/// lock hold is non-trivial (walks the book) — as a real submit would be.
fn writer_order(n: u64) -> Order {
    let (side, price) = if n % 2 == 0 {
        (Side::Buy, Price(1001)) // lifts the best ask
    } else {
        (Side::Sell, Price(1000)) // hits the best bid
    };
    Order { id: OrderId(2_000_000 + n), account: AccountId(3), side, qty: Qty(5), order_type: OrderType::Limit { price } }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn report(name: &str, mut lat: Vec<u128>) {
    let mean = lat.iter().sum::<u128>() / lat.len() as u128;
    lat.sort_unstable();
    println!(
        "{name:<18} mean={mean:>5}ns  p50={:>5}ns  p99={:>6}ns  p99.9={:>7}ns  max={:>9}ns",
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        percentile(&lat, 0.999),
        lat.last().unwrap(),
    );
}

fn bench_mutex() -> Vec<u128> {
    let book = Arc::new(Mutex::new(seed_book()));
    let stop = Arc::new(AtomicBool::new(false));
    let (wb, ws) = (book.clone(), stop.clone());
    let writer = thread::spawn(move || {
        let mut n = 0u64;
        while !ws.load(Ordering::Relaxed) {
            {
                let mut b = wb.lock().unwrap();
                b.submit(writer_order(n));
            }
            n = n.wrapping_add(1);
        }
    });

    let mut lat = Vec::with_capacity(READS);
    for _ in 0..READS {
        let t = Instant::now();
        {
            let b = book.lock().unwrap();
            black_box(b.best_bid());
        }
        lat.push(t.elapsed().as_nanos());
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    lat
}

fn bench_arcswap() -> Vec<u128> {
    let book = Arc::new(ArcSwap::from_pointee(seed_book()));
    let stop = Arc::new(AtomicBool::new(false));
    let (wb, ws) = (book.clone(), stop.clone());
    let writer = thread::spawn(move || {
        let mut n = 0u64;
        while !ws.load(Ordering::Relaxed) {
            let mut next = (*wb.load_full()).clone(); // writer pays the snapshot clone
            next.submit(writer_order(n));
            wb.store(Arc::new(next));
            n = n.wrapping_add(1);
        }
    });

    let mut lat = Vec::with_capacity(READS);
    for _ in 0..READS {
        let t = Instant::now();
        {
            let g = book.load(); // lock-free
            black_box(g.best_bid());
        }
        lat.push(t.elapsed().as_nanos());
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    lat
}

fn main() {
    println!("CLOB read-path latency under a concurrent writer — {READS} reads, book depth {DEPTH}/side\n");
    let _ = bench_mutex(); // warm up (page-in, CPU ramp)
    report("Arc<Mutex<Book>>", bench_mutex());
    let _ = bench_arcswap();
    report("ArcSwap<Book>", bench_arcswap());
    println!("\nTradeoff: ArcSwap flattens the reader tail (no lock wait) but the writer clones");
    println!("the whole Book per published version — cost + allocation move to the write side.");
}
