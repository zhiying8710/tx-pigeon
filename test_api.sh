#!/bin/bash

# 测试tx-pigeon HTTP接口
# 这是一个示例交易hex（无效的，仅用于测试格式）

echo "Testing tx-pigeon HTTP API..."

# 测试健康检查端点
echo "1. Testing health check endpoint..."
curl -X GET http://127.0.0.1:3000/health | jq '.'

echo ""
echo "2. Testing broadcast endpoint with invalid hex..."
# 测试无效的交易hex
curl -X POST http://127.0.0.1:3000/broadcast \
  -H "Content-Type: application/json" \
  -d '{"tx": "invalid_hex_string"}' \
  | jq '.'

echo ""
echo "API test completed. Use a valid transaction hex for actual broadcasting."
echo ""
echo "Example valid request format:"
echo 'curl -X POST http://127.0.0.1:3000/broadcast \'
echo '  -H "Content-Type: application/json" \'
echo '  -d '"'"'{"tx": "0200000001..."}'"'"'' 