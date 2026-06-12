//! Dev drill: show keep-alive reuse against a real host. Request 1
//! pays DNS + TCP + TLS; pooled followers should skip all three.
//!
//! Usage: cargo run -p bui-net --example pool_timing -- [url] [count]

use bui_net::{Client, Url};

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "https://en.wikipedia.org/wiki/Rust".to_string());
    let count: usize = args.next().and_then(|c| c.parse().ok()).unwrap_or(5);
    let url = Url::parse(&url).expect("url");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = Client::new();
    rt.block_on(async {
        for i in 1..=count {
            let started = std::time::Instant::now();
            match client.get(&url).await {
                Ok(resp) => println!(
                    "#{i}: {} {} bytes in {:>5.0} ms",
                    resp.status,
                    resp.body.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                ),
                Err(e) => println!("#{i}: error: {e}"),
            }
        }
    });
}
