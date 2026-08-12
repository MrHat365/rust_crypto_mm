# Binance → DigiFinex Lead-Lag Strategy

## Idea

Binance USD-M perps typically lead smaller venues on short horizons. DigiFinex is treated as the **lagger / execution venue**. When Binance bid/ask dislocates versus DigiFinex touch and Binance is temporally leading, take DigiFinex liquidity and exit on mean reversion / max age.

Signal (same structure as `hft-lead-lag` / `lead-lag-arb`):

- `bid_ask_bps = (leader.bid - lagger.ask) / lagger.ask * 1e4` → Long lagger
- `ask_bid_bps = (lagger.bid - leader.ask) / leader.ask * 1e4` → Short lagger
- enter if `max(bid_ask, ask_bid) >= min_entry_spread_bps`
- suppress if quote age/skew stale, or leader exchange/local ts is behind lagger

## Wiring in this repo

- Market data: existing Binance feed + new DigiFinex feed
- Strategy: `strategy_kind: lead_lag`
- Execution venue: `strategy.venue: digifinex` (IOC intents)
- Dry-run supported via `DryRunGateway`

## Example config

See `config/digifinex_lead_lag.yaml`.

## Live credentials

```bash
export DIGIFINEX_API_KEY=...
export DIGIFINEX_API_SECRET=...
```

Private REST uses DigiFinex swap v2 headers `ACCESS-KEY` / `ACCESS-SIGN` / `ACCESS-TIMESTAMP` (HMAC-SHA256 hex over `timestamp+METHOD+path+body`).

## Caveats

- DigiFinex order status polling is REST-based (no private WS fill stream yet)
- Contract sizing uses DigiFinex instrument `contract_value` when base-denominated; otherwise 1.0
- Thresholds must clear fees + adverse selection; start dry-run and calibrate `min_entry_spread_bps` from observed dislocations

## Sources

- https://github.com/MrHat365/hft-lead-lag
- https://github.com/MrHat365/lead-lag-arb
- DigiFinex swap docs: https://docs.digifinex.com/en-ww/swap/v2/rest.html
