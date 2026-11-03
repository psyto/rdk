//! `cargo run -p rdk-oracle --example oracle_quickstart`
//!
//! Three feeds report an index price; one is 5% off. The median, then the
//! deviation filter, then the median again drop it.

use rdk_oracle::{aggregate_index, OracleParams};
use rdk_units::IndexPrice;

fn main() {
    let params = OracleParams::hyperliquid_default(); // drop feeds >100 bp from the median, need 2
    let feeds = [IndexPrice(100_000_000), IndexPrice(100_050_000), IndexPrice(105_000_000)];
    match aggregate_index(&feeds, &params) {
        Ok(index) => println!("index price: {}", index.0),
        Err(e) => println!("no price: {e:?}"),
    }
    // With two feeds far apart, neither survives the filter and the oracle refuses to price.
    println!("{:?}", aggregate_index(&[IndexPrice(100_000_000), IndexPrice(120_000_000)], &params));
}
