use std::env;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rust_test::exchanges::digifinex::orderbook::DigiFinexDepthMsg;
use rust_test::exchanges::digifinex::parser::decode_digifinex_payload;
use rust_test::exchanges::digifinex::{DigiFinexBook, normalize_instrument_id};
use rust_test::exchanges::endpoints::DigiFinexWs;
use tungstenite::{Message, connect};
use url::Url;

fn main() -> Result<()> {
    let symbol = env::args()
        .nth(1)
        .unwrap_or_else(|| "BTC_USDT".to_string());
    let instrument_id = normalize_instrument_id(&symbol);

    println!("Connecting to DigiFinex swap WS for {instrument_id} …");
    let (mut socket, _) =
        connect(Url::parse(DigiFinexWs::BASE).context("invalid DigiFinex URL")?)
            .context("failed to connect to DigiFinex websocket")?;

    for sub in [
        DigiFinexWs::sub_depth(&instrument_id, 10),
        DigiFinexWs::sub_ticker(&instrument_id),
    ] {
        println!("> {sub}");
        socket
            .send(Message::Text(sub))
            .context("failed to send subscription")?;
    }

    let mut book = DigiFinexBook::<1024>::new(
        &instrument_id,
        DigiFinexBook::<1024>::PRICE_SCALE,
        DigiFinexBook::<1024>::QTY_SCALE,
        1.0,
    );

    let mut last_snapshot = Instant::now();
    loop {
        let msg = socket.read().context("error reading websocket message")?;
        match msg {
            Message::Text(text) => {
                handle_message(&mut book, &instrument_id, &text, &mut last_snapshot);
            }
            Message::Binary(bin) => {
                if let Some(text) = decode_digifinex_payload(&bin) {
                    handle_message(&mut book, &instrument_id, &text, &mut last_snapshot);
                } else {
                    println!("<- binary {} bytes (decode failed)", bin.len());
                }
            }
            Message::Ping(payload) => {
                println!("<- ping ({} bytes)", payload.len());
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
    book: &mut DigiFinexBook<1024>,
    instrument_id: &str,
    text: &str,
    last_snapshot: &mut Instant,
) {
    let preview: String = text.chars().take(220).collect();
    println!("<- {preview}");
    match serde_json::from_str::<DigiFinexDepthMsg>(text) {
        Ok(msg) if msg.event == DigiFinexWs::DEPTH_UPDATE => {
            if book.apply(&msg) {
                log_book_state(book, instrument_id, "depth.update");
            } else {
                println!("depth.update apply=false");
            }
        }
        Ok(_) => {}
        Err(err) => {
            // Non-depth control/ticker frames are expected.
            let _ = err;
        }
    }

    if last_snapshot.elapsed() > Duration::from_secs(30) {
        println!("--- 30s heartbeat ---");
        log_book_state(book, instrument_id, "periodic");
        *last_snapshot = Instant::now();
    }
}

fn log_book_state(book: &DigiFinexBook<1024>, instrument_id: &str, source: &str) {
    let mid = book.mid_price_f64().unwrap_or_default();
    let (bid_levels, ask_levels) = book.top_levels_f64(3);
    if let (Some(best_bid), Some(best_ask)) = (bid_levels.first(), ask_levels.first()) {
        println!(
            "[{source}] {instrument_id} seq={} ts={} mid={:.6} bid={:.6}@{:.3} ask={:.6}@{:.3}",
            book.last_seq(),
            book.last_ts(),
            mid,
            best_bid.0,
            best_bid.1,
            best_ask.0,
            best_ask.1,
        );
    } else {
        println!(
            "[{source}] {instrument_id} seq={} ts={} mid={:.6} (book incomplete)",
            book.last_seq(),
            book.last_ts(),
            mid,
        );
    }
}
