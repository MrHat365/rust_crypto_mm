use std::env;
use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use flate2::read::{DeflateDecoder, ZlibDecoder};
use rust_test::exchanges::digifinex::DigifinexBook;
use rust_test::exchanges::digifinex::orderbook::DigifinexDepthMsg;
use rust_test::exchanges::endpoints::DigifinexWs;
use rust_test::exchanges::digifinex::rest::normalize_instrument_id;
use tungstenite::{Message, connect};
use url::Url;

fn main() -> Result<()> {
    let instrument = env::args()
        .nth(1)
        .map(|s| normalize_instrument_id(&s))
        .unwrap_or_else(|| "BTCUSDTPERP".to_string());

    println!("Connecting to Digifinex swap WS for {instrument} …");
    let (mut socket, _) = connect(Url::parse(DigifinexWs::BASE).context("invalid Digifinex URL")?)
        .context("failed to connect to Digifinex websocket")?;

    let subscribe = DigifinexWs::subscribe_depth(&instrument, 20, 1);
    println!("> {}", subscribe);
    socket
        .send(Message::Text(subscribe))
        .context("failed to send subscription")?;

    let mut book = DigifinexBook::<1024>::new(
        &instrument,
        DigifinexBook::<1024>::PRICE_SCALE,
        DigifinexBook::<1024>::QTY_SCALE,
        1.0,
    );
    let mut last_snapshot = Instant::now();
    loop {
        let msg = socket.read().context("error reading websocket message")?;
        match msg {
            Message::Text(text) => handle_text(&mut book, &instrument, &text, &mut last_snapshot),
            Message::Binary(bin) => {
                if let Some(text) = inflate_to_string(&bin) {
                    handle_text(&mut book, &instrument, &text, &mut last_snapshot);
                } else {
                    println!("<- binary inflate failed ({} bytes)", bin.len());
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
        }
    }
    Ok(())
}

fn inflate_to_string(data: &[u8]) -> Option<String> {
    let try_zlib = || {
        let mut decoder = ZlibDecoder::new(data);
        let mut out = String::new();
        decoder.read_to_string(&mut out).ok()?;
        if out.is_empty() { None } else { Some(out) }
    };
    let try_deflate = || {
        let mut decoder = DeflateDecoder::new(data);
        let mut out = String::new();
        decoder.read_to_string(&mut out).ok()?;
        if out.is_empty() { None } else { Some(out) }
    };
    try_zlib()
        .or_else(try_deflate)
        .or_else(|| std::str::from_utf8(data).ok().map(|s| s.to_string()))
}

fn handle_text(
    book: &mut DigifinexBook<1024>,
    instrument: &str,
    text: &str,
    last_snapshot: &mut Instant,
) {
    if text.contains("\"event\":\"depth.update\"") {
        match serde_json::from_str::<DigifinexDepthMsg>(text) {
            Ok(msg) => {
                if book.apply(&msg) {
                    log_book_state(book, instrument, "depth");
                }
            }
            Err(err) => eprintln!("depth parse error: {err}"),
        }
    } else if last_snapshot.elapsed() > Duration::from_secs(15) {
        println!("<- {}", text.chars().take(180).collect::<String>());
        *last_snapshot = Instant::now();
    }
}

fn log_book_state(book: &DigifinexBook<1024>, instrument: &str, source: &str) {
    let mid = book.mid_price_f64().unwrap_or_default();
    let (bids, asks) = book.top_levels_f64(1);
    println!(
        "[{source}] {instrument} ts={} mid={:.6} bid={:?} ask={:?}",
        book.last_ts(),
        mid,
        bids.first(),
        asks.first()
    );
}
