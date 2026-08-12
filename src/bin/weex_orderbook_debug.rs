use std::env;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rust_test::exchanges::endpoints::WeexWs;
use rust_test::exchanges::weex::WeexBook;
use rust_test::exchanges::weex::orderbook::WeexDepthMsg;
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, connect};
use url::Url;

const USER_AGENT: &str = "rust_test-weex/0.1";

fn main() -> Result<()> {
    let symbol = env::args()
        .nth(1)
        .unwrap_or_else(|| "BTCUSDT".to_string());

    println!("Connecting to WEEX contract WS for {symbol} …");

    let url = Url::parse(WeexWs::BASE).context("invalid WEEX URL")?;
    let mut request = url.into_client_request().context("failed to build request")?;
    request
        .headers_mut()
        .insert("User-Agent", USER_AGENT.parse().context("invalid UA")?);

    let (mut socket, _) = connect(request).context("failed to connect to WEEX websocket")?;

    let subscribe = WeexWs::subscribe_multi(
        &rust_test::exchanges::weex::rest::normalize_symbol(&symbol),
        &[WeexWs::DEPTH15, WeexWs::TRADE, WeexWs::TICKER],
    );
    println!("> {}", subscribe);
    socket
        .send(Message::Text(subscribe))
        .context("failed to send subscription")?;

    let mut book = WeexBook::<1024>::new(
        &symbol,
        WeexBook::<1024>::PRICE_SCALE,
        WeexBook::<1024>::QTY_SCALE,
        1.0,
    );

    let mut last_snapshot = Instant::now();
    loop {
        let msg = socket.read().context("error reading websocket message")?;
        match msg {
            Message::Text(text) => {
                if text.contains("\"event\":\"ping\"") {
                    let pong = r#"{"method":"PONG","id":1}"#;
                    socket.send(Message::Text(pong.to_string()))?;
                    continue;
                }
                handle_message(&mut book, &symbol, &text, &mut last_snapshot);
            }
            Message::Binary(bin) => {
                if let Ok(text) = std::str::from_utf8(&bin) {
                    handle_message(&mut book, &symbol, text, &mut last_snapshot);
                }
            }
            Message::Ping(payload) => {
                println!("<- ws ping ({} bytes)", payload.len());
                socket.send(Message::Pong(payload))?;
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                println!("Websocket closed: {:?}", frame);
                break;
            }
            Message::Frame(_) => {}
        }
    }

    Ok(())
}

fn handle_message(
    book: &mut WeexBook<1024>,
    symbol: &str,
    text: &str,
    last_snapshot: &mut Instant,
) {
    if let Ok(msg) = serde_json::from_str::<WeexDepthMsg>(text) {
        if msg.e == "depth" {
            if book.apply(&msg) {
                log_book_state(book, symbol, &msg.d);
            }
        }
    }

    if last_snapshot.elapsed() > Duration::from_secs(30) {
        println!("--- 30s heartbeat ---");
        log_book_state(book, symbol, "periodic");
        *last_snapshot = Instant::now();
    }
}

fn log_book_state(book: &WeexBook<1024>, symbol: &str, source: &str) {
    let mid = book.mid_price_f64().unwrap_or_default();
    let (bid_levels, ask_levels) = book.top_levels_f64(3);
    if let (Some(best_bid), Some(best_ask)) = (bid_levels.first(), ask_levels.first()) {
        println!(
            "[{source}] {symbol} ts={} mid={:.6} bid={:.6}@{:.3} ask={:.6}@{:.3} init={}",
            book.last_ts(),
            mid,
            best_bid.0,
            best_bid.1,
            best_ask.0,
            best_ask.1,
            book.is_initialized()
        );
    } else {
        println!(
            "[{source}] {symbol} ts={} mid={:.6} (book incomplete) init={}",
            book.last_ts(),
            mid,
            book.is_initialized()
        );
    }
}
