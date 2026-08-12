# Digifinex Perpetual WebSocket

- Base URL: `wss://openapi.digifinex.com/swap_ws/v2/`
- REST: `https://openapi.digifinex.com/swap/v2`
- Symbol: `BTCUSDT` → `BTCUSDTPERP`

## 订阅

```json
{"event":"depth.subscribe","id":1,"instrument_id":"BTCUSDTPERP","level":20}
{"event":"ticker.subscribe","id":2,"instrument_id":"BTCUSDTPERP"}
{"event":"trades.subscribe","id":3,"instrument_id":"BTCUSDTPERP"}
```

## 心跳

每 30s 发送：`{"event":"server.ping","id":N}`

## 压缩

所有推送使用 zlib deflate，parser 自动解压。

## 代码位置

- `src/exchanges/digifinex/`
- `src/collectors/digifinex.rs`
- `src/base_classes/engine/digifinex.rs`
