# 做市商策略研究笔记（2025+）

本仓库新增/整合了三类现代做市与微观结构研究方向：

## 1. Avellaneda-Stoikov 最优做市

实现：`src/strategy/avellaneda_stoikov.rs`

核心思想：
- 用库存风险项 `γ σ² τ` 调整 reservation price
- 用订单到达强度 `k` 推导最优半价差
- 库存偏斜自动收窄一侧报价、放宽另一侧

参考：[hft-market-making-avellaneda-stoikov](https://github.com/Felooo8/hft-market-making-avellaneda-stoikov)

配置示例：`config/avellaneda_stoikov_gate.yaml`

## 2. Order-flow Toxicity（VPIN / OFI）

实现：`src/strategy/orderflow_toxicity.rs`

指标：
- **OFI**：盘口买卖量不平衡
- **VPIN**：成交量桶内方向性失衡
- **toxicity_score**：综合毒性分数，用于动态拉宽价差

参考：
- [orderflow-toxicity](https://github.com/Priyaanshu-Patel/orderflow-toxicity.git)
- [hft-orderflow-alpha](https://github.com/Felooo8/hft-orderflow-alpha.git)

AS 策略在 `max_toxicity_widen_bps` 下自动根据毒性加宽报价。

## 3. Lead-Lag 跨所套利

实现：`src/strategy/lead_lag.rs`

逻辑（Binance 领先，Digifinex 跟随）：
- `ΔP_long = leader_bid - follower_ask`：正向 lead-lag 做多 follower
- `ΔP_short = follower_bid - leader_ask`：反向 lead-lag 做空 follower
- 信号需持续 `signal_persist_ms` 才入场，避免噪声

参考：
- [lead-lag-arb](https://github.com/MrHat365/lead-lag-arb.git)
- [hft-lead-lag](https://github.com/MrHat365/hft-lead-lag.git)

配置示例：`config/lead_lag_binance_digifinex.yaml`

## 新增交易所

| 交易所 | WS | 数据 |
|--------|-----|------|
| Digifinex | `wss://openapi.digifinex.com/swap_ws/v2/` | depth/ticker/trades（zlib 压缩） |
| Weex | `wss://ws-contract.weex.com/v3/ws/public` | depth15/ticker/trade |

详见 `docs/digifinex_perp_ws.md` 与 `docs/weex_perp_ws.md`。

## 运行

```bash
# AS 做市 dry-run
cargo run -F gate_exec --bin gate_runner -- --config config/avellaneda_stoikov_gate.yaml

# Lead-lag dry-run
cargo run -F gate_exec --bin gate_runner -- --config config/lead_lag_binance_digifinex.yaml
```
