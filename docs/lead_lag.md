# Binance → Digifinex lead-lag

Signal is computed from **Binance BBO (leader)** vs **Digifinex BBO (lagger)**. The construction follows [lead-lag-arb](https://github.com/MrHat365/lead-lag-arb) and [hft-lead-lag](https://github.com/MrHat365/hft-lead-lag):

```
ΔP_long  = leader_bid - lagger_ask     # buy lagger, sell leader
ΔP_short = lagger_bid - leader_ask     # sell lagger, buy leader
spread_bps = max(ΔP_*) / mid * 1e4
```

A signal is armed only when all of these hold:

- `spread_bps >= min_entry_spread_bps`
- same direction persists for `min_persist_ms` (filters flicker)
- `|recv(leader) - recv(lagger)| <= max_quote_skew_ms`
- both quotes younger than `max_quote_age_ms`
- leader touch notional `>= min_leader_depth_usd`
- not inside `cooldown_ms` after a reject

Exit when spread collapses to `exit_spread_bps` or `max_hold_ms` elapses.

On a long-lagger signal the strategy sends an **IOC bid at the Digifinex ask**; short-lagger sends an **IOC ask at the Digifinex bid**. Opposite-side working orders are cancelled.

## Execution caveat (read this)

Digifinex is wired as **market data only**, same as Binance/OKX/MEXC. There is no Digifinex private order gateway in this repo. IOC intents go to `strategy.venue` (`gate` or `lighter`).

That means live taking is **not** hitting Digifinex's book. Use this as:

1. dry-run / paper signal quality on Binance vs Digifinex BBO
2. a Gate/Lighter taker that is *informed by* the Digifinex lag, not a true cross-venue arb

True Binance-lead / Digifinex-take needs a Digifinex execution adapter (HMAC REST + private WS fills). Until that exists, keep `dry_run: true`.

Clock offset median (hft-lead-lag) is not implemented. Skew is computed from local `recv Instant`, which is the right quantity for a co-located process and avoids trusting venue clocks.

## Config

See `config/lead_lag.yaml`.

```bash
cargo run --release --features gate_exec --bin gate_runner -- --config config/lead_lag.yaml
```

`feeds.binance` and `feeds.digifinex` must be enabled (`true` or `auto`). Validation fails otherwise.
