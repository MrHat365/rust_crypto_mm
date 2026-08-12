# Weex Perpetual WebSocket (V3)

- Public WS: `wss://ws-contract.weex.com/v3/ws/public`
- REST: `https://api-contract.weex.com/capi/v3`
- Symbol: `BTCUSDT`（无后缀）

## 握手

需要 `User-Agent` header（已在 `WeexHandler::connect_headers` 配置）。

## 订阅

```json
{"method":"SUBSCRIBE","params":["BTCUSDT@depth15","BTCUSDT@ticker","BTCUSDT@trade"],"id":1}
```

## Ping/Pong

服务端 `{"event":"ping","time":"..."}` → 回复 `{"event":"pong","time":"..."}`

## 代码位置

- `src/exchanges/weex/`
- `src/collectors/weex.rs`
- `src/base_classes/engine/weex.rs`
