# Modern Market Making Research Notes

This note upgrades the project's market-making research baseline beyond static half-spread + EWMA vol. It synthesizes the reference repos and maps them onto the live Rust stack (`as_toxicity` strategy).

## Why the old approach is stale

The previous `simple_quote` path is essentially:

- fair mid from demeaned reference
- half-spread = `max(min_bps, vol_ewma * mult + fee + buffer)`
- inventory handled mostly by cancel / risk gates

That ignores three effects that dominate short-horizon crypto MM PnL:

1. **Inventory-aware reservation pricing** (Avellaneda–Stoikov)
2. **Order-flow toxicity / OFI** (Cont–Kukanov–Stoikov and successors)
3. **Latency + cost-aware alpha decay** (orderflow-alpha experimental discipline)

## Reference synthesis

### 1) Avellaneda–Stoikov (`hft-market-making-avellaneda-stoikov`)

Core control law:

- reservation: `r = S - q * γ * σ² * τ`
- optimal spread: `δ = γσ²τ + (2/γ) ln(1 + γ/k)` (with γ→0 limit `2/k`)

Practical upgrades we keep:

- volatility floor
- spread floor (fees + min half-spread)
- inventory hard caps (stop quoting the saturated side)
- optional microprice as `S` instead of raw mid

### 2) Order-flow / toxicity (`orderflow-toxicity`, `hft-orderflow-alpha`)

Actionable features at the touch:

- Cont–Kukanov–Stoikov best-level OFI
- microprice − mid
- rolling signed volume / imbalance (VPIN-lite)
- short-horizon predictive value is small after costs; use signals to **skew / widen**, not as standalone directional bets

Operational rule used in `as_toxicity`:

- OFI → reservation skew (`ofi_skew_bps`)
- |signed-volume toxicity| → additive widen (`signed_vol_widen_bps`)
- do **not** claim post-cost directional alpha from OFI alone

### 3) Fair-value / lead-lag coupling

Cross-venue demean + Binance lead already exist in this repo. For MM:

- quote off demeaned / model mid when available
- widen when local book is quiet but leader is moving (future extension: lead-lag residual as toxicity)

## Implemented strategy: `as_toxicity`

Config block:

```yaml
strategy_kind: as_toxicity
as_toxicity:
  gamma: 0.1
  k: 1.5
  tau_secs: 30.0
  inventory_limit: 5.0
  toxicity_window: 32
  ofi_skew_bps: 2.0
  signed_vol_widen_bps: 3.0
  microprice_blend: 0.5
```

Hot-path behavior:

1. Update EWMA variance from mid returns
2. Update OFI from venue BBO size/price transitions
3. Update signed-volume toxicity from venue trades
4. Build AS reservation + half-spread
5. Apply OFI skew + toxicity widen + fee floors
6. Post-only quotes; suppress saturated inventory side

## Research backlog (not claimed done)

- multi-level / PCA-integrated OFI
- calibrated fill intensity `k` from markouts
- adverse-selection markout feedback into `γ`
- leader residual as additional toxicity feature
- offline experiment harness mirroring Felooo8 AS sim for parameter sweeps

## Sources

- Cont, Kukanov, Stoikov (2014), Price Impact of Order Book Events
- Avellaneda & Stoikov (2008), High-frequency trading in a limit order book
- Cont, Cucuringu, Zhang (2023), Cross-Impact of Order Flow Imbalance
- https://github.com/Felooo8/hft-market-making-avellaneda-stoikov
- https://github.com/Felooo8/hft-orderflow-alpha
- https://github.com/Priyaanshu-Patel/orderflow-toxicity
