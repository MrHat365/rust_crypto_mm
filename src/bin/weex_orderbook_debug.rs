use std::env;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rust_test::exchanges::endpoints::WeexWs;
use rust_test::exchanges::weex::WeexBook;
use rust_test::exchanges::weex::orderbook::WeexDepthMsg;
use rust_test::exchanges::weex::rest::normalize_symbol;
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, connect};
use url::Url;

fn main() -> Result<()> {
    let symbol = env::args()
        .nth(1)
        .map(|s| normalize_symbol(&s))
        .unwrap_or_else(|| "BTCUSDT".to_string());

    println!("Connecting to WEEX public WS for {symbol} …");
    let parsed = Url::parse(WeexWs::PUBLIC_BASE).context("invalid WEEX URL")?;
    let mut request = parsed
        .into_client_request()
        .context("failed to build WEEX websocket request")?;
    request.headers_mut().insert(
        "User-Agent",
        "rust-crypto-mm/0.1"
            .parse()
            .context("invalid User-Agent header")?,
    );
    let (mut socket, _) = connect(request).context("failed to connect to WEEX websocket")?;

    let subscribe = WeexWs::subscribe(&symbol);
    println!("> {}", subscribe);
    socket
        .send(Message::Text(subscribe))
        .context("failed to send subscription")?;

    let mut book = WeexBook::<1024>::new(
        &symbol,
        WeexBook::<1024>::PRICE_SCALE,
        WeexBook::<1024>::QTY_SCALE,
    );
    let rt = tokio::runtime::Runtime::new().context("tokio rt")?;
    if let Err(err) = rt.block_on(book.init_from_rest(15)) {
        eprintln!("REST snapshot failed: {err}");
    }

    let mut last_log = Instant::now();
    loop {
        let msg = socket.read().context("error reading websocket message")?;
        match msg {
            Message::Text(text) => {
                if text.contains("\"e\":\"depth\"") {
                    match serde_json::from_str::<WeexDepthMsg>(&text) {
                        Ok(depth) => {
                            if book.apply_depth_update(&depth) {
                                log_book_state(&book, &symbol, "depth");
                            }
                        }
                        Err(err) => eprintln!("depth parse error: {err}"),
                    }
                } else if text.contains("\"event\":\"ping\"") {
                    socket.send(Message::Text(WeexWs::PONG.to_string()))?;
                } else if last_log.elapsed() > Duration::from_secs(15) {
                    println!("<- {}", text.chars().take(180).collect::<String>());
                    last_log = Instant::now();
                }
            }
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload))?;
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                println!("Websocket closed: {:?}", frame);
                break;
            }
            Message::Frame(_) => {}
            Message::Binary(bin) => {
                if let Ok(text) = std::str::from_utf8(&bin) {
                    println!("<- binary utf8 {}", text.chars().take(120).collect::<String>());
                }
            }
        }
    }
    Ok(())
}

fn log_book_state(book: &WeexBook<1024>, symbol: &str, source: &str) {
    let mid = book.mid_price_f64().unwrap_or_default();
    let (bids, asks) = book.top_levels_f64(1);
    println!(
        "[{source}] {symbol} ts={} mid={:.6} bid={:?} ask={:?}",
        book.last_ts(),
        mid,
        bids.first(),
        asks.first()
    );
}
