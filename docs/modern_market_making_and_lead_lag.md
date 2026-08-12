# 现代做市与 Binance → DigiFinex Lead-Lag 研究方案

## 当前实现

代码入口：

- `strategy/adaptive_market_maker.rs`：标准化 L2/主动成交输入，输出可执行双边报价。
- `strategy/lead_lag.rs`：Binance 作为 leader、DigiFinex 作为 lagger，输出带价格保护的方向性交易意图。
- `exchanges/digifinex`、`exchanges/weex`：官方协议对应的 REST、公共 WS、私有 WS。

这两个模型不会自行发送订单。调用方必须经过现有的风险网关和订单状态机，并使用私有 WS
确认成交。这样可以避免把“产生信号”和“订单已成交”错误地混成一个状态。

## 1. 做市模型

### 1.1 公允价

最佳档微价格：

```text
micro = (ask * bid_qty + bid * ask_qty) / (bid_qty + ask_qty)
```

Cont-Kukanov-Stoikov 风格 OFI：

```text
bid_flow = price_up ? new_bid_qty : price_down ? -old_bid_qty : Δbid_qty
ask_flow = price_down ? new_ask_qty : price_up ? -old_ask_qty : Δask_qty
OFI = clip((bid_flow - ask_flow) / mean_top_depth, -1, 1)
```

主动成交不平衡：

```text
TFI = EWMA(buy_volume - sell_volume) / EWMA(buy_volume + sell_volume)
```

组合短周期 alpha：

```text
alpha_bps =
    w_micro * log(micro / mid) * 10_000
  + w_ofi * OFI
  + w_trade * TFI

fair = mid * exp(alpha_bps / 10_000)
```

### 1.2 库存效用与价差

库存保留价使用 Avellaneda-Stoikov 的风险项，但库存先按硬限制归一化：

```text
reservation = fair * exp(-γ * normalized_inventory * σ² * horizon)
```

半价差由以下部分组成：

```text
half_spread =
    fee
  + [log(1 + γ/k) / γ + 0.5 * γ * σ² * horizon]
  + short_horizon_volatility
  + toxicity_widening
```

`k` 是成交到达强度，不应凭经验永久固定。生产校准应按
`P(fill | distance, queue, regime)` 拟合；当前配置值是冷启动先验。

### 1.3 毒性与状态切换

实现使用有界 VPIN-like 在线代理，而不是冒充严格的 volume-bucket VPIN：

```text
toxicity =
    0.55 * |TFI|
  + 0.45 * |OFI|
  + 0.25 * directional_disagreement
```

动作：

- 正常：双边报价。
- toxic：扩大价差并降低两侧尺寸；库存侧进一步减量。
- halted：行情陈旧或达到库存硬限制时不报价。

下一阶段若有逐笔成交和可靠的 aggressor side，应并行加入：

- bucketed VPIN / bulk volume classification；
- 成交后 50/100/250/500ms markout；
- cancel-to-trade、queue depletion hazard；
- Hawkes/self-exciting arrival intensity；
- 按波动率、价差、深度和事件时钟做 regime-conditioned 参数。

不要直接把模型复杂度当成收益。所有新增特征必须通过净收益、尾部 markout、库存占用和
撤单率的增量消融。

## 2. Lead-Lag

### 2.1 在线信号

每个 Binance BBO 变化产生 leader log-return：

```text
pending += β * log(binance_mid_new / binance_mid_old)
```

DigiFinex 更新后，从 pending 中扣除 lagger 已实现的 return。信号按半衰期衰减并限制到
`max_signal_bps`，防止断线恢复或异常跳价产生无限意图。

`β` 用“两个 DigiFinex 更新之间累计的 Binance return”与 DigiFinex return 做 EWMA
协方差/方差估计，完成最小样本前使用 `initial_beta`：

```text
β = clip(EWMA(r_leader * r_lagger) / EWMA(r_leader²), 0.05, 3.0)
```

### 2.2 可交易边际

```text
edge =
    |expected_move|
  - lagger_half_spread
  - taker_fee
  - impact_buffer
```

只有 `edge >= entry_threshold` 才生成意图。限价直接使用当时 DigiFinex ask/bid 作为
marketable-limit 保护，不允许执行器无界追价。

`LeadLagDecision::digifinex_execution_plan` 会根据私有仓位快照把意图转换成 DigiFinex
IOC 委托。反向穿仓会拆成“先平旧方向、再开新方向”两腿；调用方必须等待私有
`order.update` 确认第一腿完成后才能提交第二腿，不能仅凭 REST ACK 连续发送。

硬门控：

- Binance 和 DigiFinex 任一行情陈旧立即报错；
- 交易所时间戳倒退立即报错；
- 冷却期、阈值滞回和最大仓位；
- 信号幅度上限；
- 仓位必须由私有成交/仓位流回灌，不能根据下单 ACK 乐观更新。

### 2.3 回测协议

必须使用事件到达顺序，而不是按交易所时间戳排序后的“完美同步”数据：

1. 保存 `exchange_ts`、本机 `recv_ts`、解析完成时间和发送时间。
2. 训练/校准只使用当时已到达的信息。
3. 注入实测下单、撤单、ACK、成交延迟分布。
4. 以 lagger 可成交 bid/ask 计价，不以 mid 成交。
5. 计入 maker/taker fee、滑点、部分成交、拒单和未成交。
6. walk-forward 分割；参数选择和最终评估日期严格分离。
7. 报告 signal→send、send→ack、send→fill、markout、净 PnL、最大库存、CVaR。

最低上线条件应同时满足：样本外净收益为正、延迟扰动后仍为正、尾部 markout 可接受、
断线/陈旧行情/私有流中断时能停止交易。

## 3. 配置起点

以下只是冷启动范围，不是收益承诺：

```yaml
adaptive_market_maker:
  tick_size: 0.1
  base_order_size: 0.001
  fee_bps: 1.0
  min_half_spread_bps: 2.0
  max_half_spread_bps: 50.0
  volatility_alpha: 0.05
  flow_alpha: 0.10
  microprice_weight: 0.5
  ofi_weight_bps: 2.0
  trade_flow_weight_bps: 2.0
  risk_aversion: 0.01
  arrival_rate: 100.0
  horizon_seconds: 1.0
  inventory_limit: 0.01
  toxicity_spread_multiplier: 10.0
  toxicity_size_reduction: 0.8
  toxicity_halt_threshold: 0.85
  stale_after_ms: 250

lead_lag:
  symbol: BTC_USDT
  quantity: 0.001
  entry_threshold_bps: 2.0
  exit_hysteresis_bps: 0.5
  taker_fee_bps: 5.0
  impact_buffer_bps: 1.0
  max_signal_bps: 25.0
  max_position: 0.01
  max_feed_age_ms: 100
  cooldown_ms: 50
  signal_halflife_ms: 250.0
  calibration_alpha: 0.02
  initial_beta: 1.0
  min_calibration_samples: 200
```

## 4. 参考与许可边界

- Cont, Kukanov, Stoikov (2014), *The Price Impact of Order Book Events*。
- Avellaneda, Stoikov (2008), *High-frequency trading in a limit order book*。
- `Priyaanshu-Patel/orderflow-toxicity`、`Felooo8/hft-orderflow-alpha`、
  `Felooo8/hft-market-making-avellaneda-stoikov`：用于核对研究方向；仓库未声明许可证，
  本项目没有复制其代码。
- `MrHat365/lead-lag-arb`：MIT；用于参考事件驱动、风险门控和适配器拆分思路。
- `MrHat365/hft-lead-lag`：未声明许可证；仅参考公开研究主题，没有复制代码。
