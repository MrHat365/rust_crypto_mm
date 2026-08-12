use crate::base_classes::demean::ExchangeKind;
use crate::base_classes::feed_gate::{ExchangeFeed, FeedKind, FeedTimestampGate, GateDecision};
use crate::base_classes::reference_publisher::ReferencePublisher;
use crate::base_classes::ring_buffer::Consumer;
use crate::base_classes::state::{SNAPSHOT_DEPTH, TradeDirection, TradeEvent};
use crate::base_classes::tickers::TickerStore;
use crate::collectors::weex;
use crate::exchanges::weex::WeexFrame;

use super::demean_controller::DemeanController;
use super::helpers::{drain_latest_bbo, level_from_option, lock_state, log_stale_update};

pub struct WeexEngine<const N: usize> {
    consumer: Consumer<WeexFrame, N>,
    pending: Option<WeexFrame>,
    book: crate::exchanges::weex::WeexBook<1024>,
    bbo: crate::base_classes::bbo_store::BboStore,
    trades: crate::base_classes::trades::FixedTrades<64>,
    tickers: TickerStore,
    symbol: String,
    qty_multiplier: f64,
}

impl<const N: usize> WeexEngine<N> {
    pub fn new(
        symbol: String,
        consumer: Consumer<WeexFrame, N>,
        contract_meta: Option<crate::exchanges::weex::WeexContractMeta>,
    ) -> Self {
        let normalized = crate::exchanges::weex::rest::normalize_symbol(&symbol);
        let qty_multiplier = contract_meta
            .as_ref()
            .and_then(|meta| meta.qty_multiplier())
            .unwrap_or(1.0);
        Self {
            consumer,
            pending: None,
            book: crate::exchanges::weex::WeexBook::<1024>::new(
                &normalized,
                crate::exchanges::weex::WeexBook::<1024>::PRICE_SCALE,
                crate::exchanges::weex::WeexBook::<1024>::QTY_SCALE,
                qty_multiplier,
            ),
            bbo: crate::base_classes::bbo_store::BboStore::default(),
            trades: crate::base_classes::trades::FixedTrades::<64>::default(),
            tickers: TickerStore::default(),
            symbol: normalized,
            qty_multiplier,
        }
    }

    #[inline(always)]
    fn is_bbo_frame(frame: &WeexFrame) -> bool {
        matches!(frame.channel(), Some("depth"))
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
            {
                for (feed, _) in weex::events_for(&mut f, &mut self.book) {
                    if feed == "orderbook" {
                        if let Some(mid) = self.book.mid_price_f64() {
                            let ob_ts = self.book.last_ts();
                            match feed_gate.evaluate(ExchangeFeed::Weex, FeedKind::OrderBook, ob_ts)
                            {
                                GateDecision::Accept => {
                                    let (bid_vec, ask_vec) =
                                        self.book.top_levels_f64(SNAPSHOT_DEPTH);
                                    let bid_levels = super::helpers::levels_to_array(&bid_vec);
                                    let ask_levels = super::helpers::levels_to_array(&ask_vec);
                                    {
                                        let mut st = lock_state();
                                        let snap = &mut st.weex.orderbook;
                                        snap.price = Some(mid);
                                        snap.seq = snap.seq.wrapping_add(1);
                                        snap.ts_ns = Some(ts);
                                        snap.source_engine_ts_ns = Some(ob_ts);
                                        snap.source_system_ts_ns = self.book.last_system_ts_ns();
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
                                        ExchangeFeed::Weex,
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

                if weex::update_bbo_from_book(&self.book, &mut self.bbo, &self.symbol) {
                    let entry = self.bbo.get(&self.symbol).copied();
                    if let Some(bbo) = entry {
                        let mid = 0.5 * (bbo.bid_px + bbo.ask_px);
                        let bbo_ts = bbo.ts;
                        let system_ts_ns =
                            bbo.system_ts_ns.or_else(|| self.book.last_system_ts_ns());
                        match feed_gate.evaluate(ExchangeFeed::Weex, FeedKind::Bbo, bbo_ts) {
                            GateDecision::Accept => {
                                demean.record_other(ExchangeKind::Weex, Some(bbo_ts), Some(mid));
                                let bid_levels = level_from_option(Some((bbo.bid_px, bbo.bid_qty)));
                                let ask_levels = level_from_option(Some((bbo.ask_px, bbo.ask_qty)));
                                {
                                    let mut st = lock_state();
                                    let snap = &mut st.weex.bbo;
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
                                    ExchangeFeed::Weex,
                                    FeedKind::Bbo,
                                    bbo_ts,
                                    last_ts,
                                    reject_count,
                                );
                            }
                        }
                    }
                }

                let new_trades = weex::update_trades(&mut f, &mut self.trades, self.qty_multiplier);
                if new_trades > 0 {
                    for trade in self.trades.iter_last(new_trades) {
                        let trade_ts = trade.ts;
                        match feed_gate.evaluate(ExchangeFeed::Weex, FeedKind::Trades, trade_ts) {
                            GateDecision::Accept => {
                                let px = (trade.px as f64) / weex::PRICE_SCALE;
                                let qty = (trade.qty as f64).abs() / weex::QTY_SCALE;
                                let direction = if trade.is_buyer_maker {
                                    TradeDirection::Sell
                                } else {
                                    TradeDirection::Buy
                                };
                                {
                                    let mut st = lock_state();
                                    let snap = &mut st.weex;
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
                                    ExchangeFeed::Weex,
                                    FeedKind::Trades,
                                    trade_ts,
                                    last_ts,
                                    reject_count,
                                );
                            }
                        }
                    }
                }

                if let Some((_, ticker)) =
                    weex::update_tickers(&mut f, &mut self.tickers, self.qty_multiplier)
                {
                    let mut st = lock_state();
                    let entry = &mut st.weex.ticker;

                    if ticker.ticker.last_px != 0 {
                        entry.last_price =
                            Some((ticker.ticker.last_px as f64) / weex::PRICE_SCALE);
                    }
                    if ticker.ticker.last_qty != 0 {
                        entry.last_qty = Some((ticker.ticker.last_qty as f64) / weex::QTY_SCALE);
                    }
                    if ticker.ticker.best_bid != 0 {
                        entry.best_bid =
                            Some((ticker.ticker.best_bid as f64) / weex::PRICE_SCALE);
                    }
                    if ticker.ticker.best_ask != 0 {
                        entry.best_ask =
                            Some((ticker.ticker.best_ask as f64) / weex::PRICE_SCALE);
                    }

                    if let Some(mark) = ticker.mark_px {
                        entry.mark_price = Some(mark);
                    }
                    if let Some(index) = ticker.index_px {
                        entry.index_price = Some(index);
                    }
                    if let Some(rate) = ticker.funding_rate {
                        entry.funding_rate = Some(rate);
                    }
                    if let Some(turnover) = ticker.turnover_24h {
                        entry.turnover_24h = Some(turnover);
                    }
                    if let Some(oi) = ticker.open_interest {
                        entry.open_interest = Some(oi);
                    }
                    if let Some(oi_val) = ticker.open_interest_value {
                        entry.open_interest_value = Some(oi_val);
                    } else if let (Some(oi), Some(mark)) = (entry.open_interest, entry.mark_price) {
                        entry.open_interest_value = Some(oi * mark);
                    }

                    if ticker.ticker.seq != 0 {
                        entry.seq = ticker.ticker.seq;
                    } else {
                        entry.seq = entry.seq.wrapping_add(1);
                    }

                    let ticker_ts = if ticker.ticker.ts != 0 {
                        ticker.ticker.ts
                    } else {
                        ts
                    };
                    entry.ts_ns = Some(ticker_ts);
                }
            }
            true
        } else {
            false
        }
    }
}
