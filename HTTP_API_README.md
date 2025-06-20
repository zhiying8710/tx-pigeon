# tx-pigeon HTTP API

tx-pigeon现在提供了一个HTTP接口来广播比特币交易到libre relay节点，并包含了性能优化功能。

## 启动服务器

```bash
cargo run
```

服务器将在 `http://127.0.0.1:3000` 启动。

## API 端点

### GET /health

健康检查端点，返回服务状态和版本信息。

#### 响应格式

```json
{
  "status": "healthy",
  "service": "tx-pigeon",
  "version": "0.1.0"
}
```

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

### POST /cache/clear

清除节点缓存，强制下次请求时重新发现节点。

#### 响应格式

```json
{
  "success": true,
  "message": "Peer cache cleared successfully"
}
```

#### 示例

使用curl测试API：

```bash
# 健康检查
curl -X GET http://127.0.0.1:3000/health

# 清除缓存
curl -X POST http://127.0.0.1:3000/cache/clear

# 测试无效的交易hex
curl -X POST http://127.0.0.1:3000/broadcast \
  -H "Content-Type: application/json" \
  -d '{"tx": "invalid_hex_string"}'

# 使用有效的交易hex（替换为实际的交易hex）
curl -X POST http://127.0.0.1:3000/broadcast \
  -H "Content-Type: application/json" \
  -d '{"tx": "0200000001..."}'
```

## 性能优化特性

- **节点缓存**: 节点发现结果缓存5分钟，避免重复发现
- **并行处理**: 交易解析和节点发现并行执行
- **CORS支持**: 支持跨域请求
- **错误处理**: 详细的错误信息和状态码
- **并发处理**: 支持多个并发连接
- **Tor支持**: 通过Tor网络连接到节点
- **智能重试**: 自动处理网络错误和超时

## 状态码

- `200 OK`: 请求成功处理
- `400 Bad Request`: 请求格式错误或无效的交易hex
- `500 Internal Server Error`: 服务器内部错误

## 缓存机制

- 节点发现结果缓存5分钟
- 可以通过 `/cache/clear` 端点手动清除缓存
- 缓存过期后自动重新发现节点
- 减少重复的DNS查询和节点爬取

## 注意事项

1. 确保Tor服务正在运行
2. 交易hex必须是有效的比特币交易格式
3. 服务器需要网络连接来发现和连接到比特币节点
4. 广播结果取决于libre relay节点的可用性和响应
5. 首次请求可能需要较长时间来发现节点
6. 后续请求会使用缓存的节点，响应更快 