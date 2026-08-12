# Market-making research (toxicity MM)

The old `simple_quote` path is a static half-spread around a reference mid. That is 2010s inventory MM without a toxicity model. `toxicity_mm` replaces it with a causal stack that matches current HFT MM practice:

| Piece | Source | What it does here |
| --- | --- | --- |
| Best-level OFI | Cont–Kukanov–Stoikov 2014; [hft-orderflow-alpha](https://github.com/Felooo8/hft-orderflow-alpha) | EWMA of bid/ask size changes at the touch. Shifts the reservation price in the direction of incoming flow. |
| Microprice | Stoikov 2018 | `S_t = (ask * bid_qty + bid * ask_qty) / (bid_qty + ask_qty)`. Used as the fair value instead of mid. |
| VPIN | Easley–López de Prado–O'Hara; [orderflow-toxicity](https://github.com/Priyaanshu-Patel/orderflow-toxicity) | Bucketed buy/sell notional imbalance. Scales risk aversion `γ`. |
| Kyle λ | Kyle 1985 / orderflow-toxicity | `|Δmid| / |signed qty|` EWMA. Widens the quoted half-spread when impact is high. |
| Avellaneda–Stoikov | [hft-market-making-avellaneda-stoikov](https://github.com/Felooo8/hft-market-making-avellaneda-stoikov) | Reservation `r = S - q γ σ² τ`, spread `γ σ² τ + (2/γ) ln(1 + γ/k)`, with the `γ → 0` limit `σ² τ + 2/k`. |

## Quote construction

1. Prefer Binance BBO/trades; fall back to Gate if Binance is dark.
2. `S_t = microprice + ofi_gain * (OFI / depth) * microprice`
3. `γ_eff = γ * (1 + toxicity_gain * VPIN)`
4. `half = max(AS half-spread, min_half_spread_bps, kyle_gain * λ)`
5. Quotes are injected as a synthetic `ReferenceEvent` into `SimpleQuoteStrategy` with `quote_at_reference_bbo=true`, so the existing post-only / reprice / cancel path is reused.

Inventory is the running signed fill delta (`bid fill +`, `ask fill -`). `inventory_limit > 0` hard-stops quoting.

## Why this is not "just AS"

Plain AS assumes Poisson fills and a constant `σ`. Crypto perps are dominated by informed takers and queue-jumping. OFI + VPIN + λ are the standard causal features used to:

- pull quotes when flow is one-sided (OFI)
- fatten spread when the last N volume buckets were almost all one way (VPIN)
- fatten spread when a small trade moved the mid a lot (Kyle λ)

All three are computed only from already-received BBO/trade snapshots. No lookahead.

## Config

See `config/toxicity_mm.yaml`. Run:

```bash
cargo run --release --features gate_exec --bin gate_runner -- --config config/toxicity_mm.yaml
```

Keep `dry_run: true` until the fill logs look sane. Tune in this order:

1. `min_half_spread_bps` — venue fee + buffer floor
2. `gamma` / `toxicity_gain` — inventory skew vs VPIN
3. `ofi_gain` — how aggressively reservation chases CKS OFI
4. `kyle_gain` — extra width on impact
5. `vpin_bucket_usd` — should be a few seconds of typical notional, not a full minute

## Execution caveat

This still quotes on `strategy.venue` (`gate` or `lighter`). Digifinex/Weex are market-data only; they feed the book/toxicity features, they do not take fills.
