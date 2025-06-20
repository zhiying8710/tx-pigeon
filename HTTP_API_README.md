# tx-pigeon HTTP API

tx-pigeon现在提供了一个HTTP接口来广播比特币交易到libre relay节点。

## 启动服务器

```bash
cargo run
```

服务器将在 `http://127.0.0.1:3000` 启动。

## API 端点

### POST /broadcast

广播一个比特币交易到libre relay节点。

#### 请求格式

```json
{
  "tx": "hex_encoded_transaction"
}
```

#### 响应格式

成功响应：
```json
{
  "success": true,
  "message": "Transaction broadcasted to 5 libre relay nodes",
  "txid": "abc123...",
  "success_count": 5,
  "total_peers": 10
}
```

错误响应：
```json
{
  "error": "Invalid hex string: Invalid character"
}
```

#### 示例

使用curl测试API：

```bash
# 测试无效的交易hex
curl -X POST http://127.0.0.1:3000/broadcast \
  -H "Content-Type: application/json" \
  -d '{"tx": "invalid_hex_string"}'

# 使用有效的交易hex（替换为实际的交易hex）
curl -X POST http://127.0.0.1:3000/broadcast \
  -H "Content-Type: application/json" \
  -d '{"tx": "0200000001..."}'
```

## 功能特性

- **CORS支持**: 支持跨域请求
- **错误处理**: 详细的错误信息和状态码
- **并发处理**: 支持多个并发连接
- **Tor支持**: 通过Tor网络连接到节点
- **节点发现**: 自动发现libre relay节点

## 状态码

- `200 OK`: 请求成功处理
- `400 Bad Request`: 请求格式错误或无效的交易hex
- `500 Internal Server Error`: 服务器内部错误

## 注意事项

1. 确保Tor服务正在运行
2. 交易hex必须是有效的比特币交易格式
3. 服务器需要网络连接来发现和连接到比特币节点
4. 广播结果取决于libre relay节点的可用性和响应 