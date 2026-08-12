use crate::base_classes::demean::ExchangeKind;
use crate::base_classes::feed_gate::{ExchangeFeed, FeedKind, FeedTimestampGate, GateDecision};
use crate::base_classes::reference_publisher::ReferencePublisher;
use crate::base_classes::ring_buffer::Consumer;
use crate::base_classes::state::{SNAPSHOT_DEPTH, TradeDirection, TradeEvent};
use crate::base_classes::tickers::TickerStore;
use crate::collectors::digifinex;
use crate::exchanges::digifinex::{DigiFinexFrame, DigiFinexInstrumentMeta};
use crate::exchanges::endpoints::DigiFinexWs;

use super::demean_controller::DemeanController;
use super::helpers::{
    drain_latest_bbo, level_from_option, levels_to_array, lock_state, log_stale_update,
};

pub struct DigiFinexEngine<const N: usize> {
    consumer: Consumer<DigiFinexFrame, N>,
    pending: Option<DigiFinexFrame>,
    book: crate::exchanges::digifinex::DigiFinexBook<1024>,
    bbo: crate::base_classes::bbo_store::BboStore,
    trades: crate::base_classes::trades::FixedTrades<64>,
    tickers: TickerStore,
    instrument_id: String,
    qty_multiplier: f64,
}

impl<const N: usize> DigiFinexEngine<N> {
    pub fn new(
        instrument_id: String,
        consumer: Consumer<DigiFinexFrame, N>,
        meta: Option<DigiFinexInstrumentMeta>,
    ) -> Self {
        let qty_multiplier = meta
            .as_ref()
            .and_then(|m| m.qty_multiplier())
            .unwrap_or(1.0);
        Self {
            consumer,
            pending: None,
            book: crate::exchanges::digifinex::DigiFinexBook::<1024>::new(
                &instrument_id,
                crate::exchanges::digifinex::DigiFinexBook::<1024>::PRICE_SCALE,
                crate::exchanges::digifinex::DigiFinexBook::<1024>::QTY_SCALE,
                qty_multiplier,
            ),
            bbo: crate::base_classes::bbo_store::BboStore::default(),
            trades: crate::base_classes::trades::FixedTrades::<64>::default(),
            tickers: TickerStore::default(),
            instrument_id,
            qty_multiplier,
        }
    }

    #[inline(always)]
    fn is_bbo_frame(frame: &DigiFinexFrame) -> bool {
        matches!(
            frame.event(),
            Some(DigiFinexWs::TICKER_UPDATE) | Some(DigiFinexWs::DEPTH_UPDATE)
        )
    }

    pub fn process(
        &mut self,
        feed_gate: &mut FeedTimestampGate,
        publisher: &mut ReferencePublisher,
        demean: &mut DemeanController,
    ) -> bool {
        if let Some(mut f) = self.pending.take().or_else(|| self.consumer.try_pop().ok()) {
            drain_latest_bbo(
                &mut f,
                &self.consumer,
                &mut self.pending,
                Self::is_bbo_frame,
            );
            let ts = f.ts;
            let event = f.event().map(|s| s.to_string()).unwrap_or_default();
            {
                if event == DigiFinexWs::DEPTH_UPDATE {
                    for (feed, _) in digifinex::events_for(&mut f, &mut self.book) {
                        if feed == "orderbook" {
                            if let Some(mid) = self.book.mid_price_f64() {
                                let ob_ts = self.book.last_ts();
                                match feed_gate.evaluate(
                                    ExchangeFeed::Digifinex,
                                    FeedKind::OrderBook,
                                    ob_ts,
                                ) {
                                    GateDecision::Accept => {
                                        let (bid_vec, ask_vec) =
                                            self.book.top_levels_f64(SNAPSHOT_DEPTH);
                                        let bid_levels = levels_to_array(&bid_vec);
                                        let ask_levels = levels_to_array(&ask_vec);
                                        {
                                            let mut st = lock_state();
                                            let snap = &mut st.digifinex.orderbook;
                                            snap.price = Some(mid);
                                            snap.seq = snap.seq.wrapping_add(1);
                                            snap.ts_ns = Some(ts);
                                            snap.source_engine_ts_ns = Some(ob_ts);
                                            snap.source_system_ts_ns =
                                                self.book.last_system_ts_ns();
                                            snap.bid_levels = bid_levels;
                                            snap.ask_levels = ask_levels;
                                            snap.direction = None;
                                            snap.received_at = Some(f.recv_instant);
                                        }
                                        publisher.publish();
                                    }
                                    GateDecision::Reject {
                                        last_ts,
                                        reject_count,
                                    } => {
                                        log_stale_update(
                                            ExchangeFeed::Digifinex,
                                            FeedKind::OrderBook,
                                            ob_ts,
                                            last_ts,
                                            reject_count,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                if (event == DigiFinexWs::TICKER_UPDATE || event == DigiFinexWs::DEPTH_UPDATE)
                    && digifinex::update_bbo_store(&mut f, &mut self.bbo, self.qty_multiplier)
                {
                    let entry = self.bbo.get(&self.instrument_id).copied().or_else(|| {
                        self.bbo
                            .last_symbol()
                            .and_then(|symbol| self.bbo.get(symbol).copied())
                    });
                    if let Some(bbo) = entry {
                        let mid = 0.5 * (bbo.bid_px + bbo.ask_px);
                        let bbo_ts = bbo.ts;
                        let system_ts_ns = bbo.system_ts_ns.or_else(|| self.book.last_system_ts_ns());
                        match feed_gate.evaluate(ExchangeFeed::Digifinex, FeedKind::Bbo, bbo_ts) {
                            GateDecision::Accept => {
                                demean.record_other(
                                    ExchangeKind::Digifinex,
                                    Some(bbo_ts),
                                    Some(mid),
                                );
                                let bid_levels =
                                    level_from_option(Some((bbo.bid_px, bbo.bid_qty)));
                                let ask_levels =
                                    level_from_option(Some((bbo.ask_px, bbo.ask_qty)));
                                {
                                    let mut st = lock_state();
                                    let snap = &mut st.digifinex.bbo;
                                    snap.price = Some(mid);
                                    snap.seq = snap.seq.wrapping_add(1);
                                    snap.ts_ns = Some(ts);
                                    snap.source_engine_ts_ns = Some(bbo_ts);
                                    snap.source_system_ts_ns = system_ts_ns;
                                    snap.bid_levels = bid_levels;
                                    snap.ask_levels = ask_levels;
                                    snap.direction = None;
                                    snap.received_at = Some(f.recv_instant);
                                }
                                publisher.publish();
                            }
                            GateDecision::Reject {
                                last_ts,
                                reject_count,
                            } => {
                                log_stale_update(
                                    ExchangeFeed::Digifinex,
                                    FeedKind::Bbo,
                                    bbo_ts,
                                    last_ts,
                                    reject_count,
                                );
                            }
                        }
                    }
                }

                if event == DigiFinexWs::TRADES_UPDATE {
                    let new_trades =
                        digifinex::update_trades(&mut f, &mut self.trades, self.qty_multiplier);
                    if new_trades > 0 {
                        for trade in self.trades.iter_last(new_trades) {
                            let trade_ts = trade.ts;
                            match feed_gate.evaluate(
                                ExchangeFeed::Digifinex,
                                FeedKind::Trades,
                                trade_ts,
                            ) {
                                GateDecision::Accept => {
                                    let px = (trade.px as f64) / digifinex::PRICE_SCALE;
                                    let qty = (trade.qty as f64).abs() / digifinex::QTY_SCALE;
                                    let direction = if trade.is_buyer_maker {
                                        TradeDirection::Sell
                                    } else {
                                        TradeDirection::Buy
                                    };
                                    {
                                        let mut st = lock_state();
                                        let snap = &mut st.digifinex;
                                        snap.trade.price = Some(px);
                                        snap.trade.seq = snap.trade.seq.wrapping_add(1);
                                        snap.trade.ts_ns = Some(ts);
                                        snap.trade.source_engine_ts_ns = Some(trade_ts);
                                        snap.trade.source_system_ts_ns = trade.system_ts_ns;
                                        snap.trade.direction = Some(direction);
                                        snap.trade.size = Some(qty);
                                        snap.trade.received_at = Some(f.recv_instant);
                                        snap.trade.bid_levels = [None; SNAPSHOT_DEPTH];
                                        snap.trade.ask_levels = [None; SNAPSHOT_DEPTH];
                                        snap.trade_events.push_back(TradeEvent {
                                            ts_ns: trade_ts,
                                            price: px,
                                            direction: Some(direction),
                                            quantity: Some(qty),
                                        });
                                        if snap.trade_events.len() > 256 {
                                            snap.trade_events.pop_front();
                                        }
                                    }
                                    publisher.publish();
                                }
                                GateDecision::Reject {
                                    last_ts,
                                    reject_count,
                                } => {
                                    log_stale_update(
                                        ExchangeFeed::Digifinex,
                                        FeedKind::Trades,
                                        trade_ts,
                                        last_ts,
                                        reject_count,
                                    );
                                }
                            }
                        }
                    }
                }

                if event == DigiFinexWs::TICKER_UPDATE {
                    if let Some((_, ticker)) =
                        digifinex::update_tickers(&mut f, &mut self.tickers, self.qty_multiplier)
                    {
                        let mut st = lock_state();
                        let entry = &mut st.digifinex.ticker;
                        let price_scale = digifinex::PRICE_SCALE;
                        let qty_scale = digifinex::QTY_SCALE;

                        if ticker.ticker.last_px != 0 {
                            entry.last_price =
                                Some((ticker.ticker.last_px as f64) / price_scale);
                        }
                        if ticker.ticker.last_qty != 0 {
                            entry.last_qty = Some((ticker.ticker.last_qty as f64) / qty_scale);
                        }
                        if ticker.ticker.best_bid != 0 {
                            entry.best_bid =
                                Some((ticker.ticker.best_bid as f64) / price_scale);
                        }
                        if ticker.ticker.best_ask != 0 {
                            entry.best_ask =
                                Some((ticker.ticker.best_ask as f64) / price_scale);
                        }
                        if let Some(turnover) = ticker.turnover_24h {
                            entry.turnover_24h = Some(turnover);
                        }
                        if let Some(oi) = ticker.open_interest {
                            entry.open_interest = Some(oi);
                        }

                        entry.seq = if ticker.ticker.seq != 0 {
                            ticker.ticker.seq
                        } else {
                            entry.seq.wrapping_add(1)
                        };
                        entry.ts_ns = Some(if ticker.ticker.ts != 0 {
                            ticker.ticker.ts
                        } else {
                            ts
                        });
                    }
                }
            }
            true
        } else {
            false
        }
    }
}
