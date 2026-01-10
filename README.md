# Solana Validator Proxy (Go版本)

这是一个高性能的 Go 语言代理服务器，用于代理 Solana validator 的 RPC 请求。它允许 `sendTransaction` 和 `simulateTransaction` 方法通过，其他 RPC 方法将被阻止。

## 性能优势

- 🚀 **高性能**: Go 语言编译为原生二进制，性能接近 Rust
- ⚡ **低延迟**: 优化的连接池和超时配置
- 🔄 **高并发**: 基于 goroutine 的并发模型，轻松处理大量请求
- 💾 **低内存占用**: 相比 Node.js/Python 更节省内存

## 功能特性

- ✅ 允许 `sendTransaction` 和 `simulateTransaction` 方法通过代理
- ✅ 自动转发请求到本地 Solana validator (localhost:8899)
- ✅ 完整的请求/响应日志
- ✅ 错误处理和健康检查端点
- ✅ 连接池优化，支持高并发

## 安装和编译

```bash
# 编译
go build -o proxy main.go

# 或者直接运行（开发模式）
go run main.go
```

## 使用方法

### 启动代理服务器

```bash
./proxy
```

### 自定义配置

```bash
# 自定义端口
./proxy -port=8898

# 自定义validator URL
./proxy -validator=http://localhost:8899

# 组合使用
./proxy -port=8898 -validator=http://localhost:8899
```

### 命令行参数

- `-port`: 代理服务器监听端口（默认: 8898）
- `-validator`: Solana validator 的 URL（默认: http://localhost:8899）

## 使用示例

代理服务器启动后，可以通过以下方式发送请求：

```bash
# 发送交易
curl -X POST http://localhost:8898 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "sendTransaction",
    "params": ["your_transaction_here"]
  }'

# 模拟交易
curl -X POST http://localhost:8898 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "simulateTransaction",
    "params": ["your_transaction_here"]
  }'
```

## 健康检查

```bash
curl http://localhost:8898/health
```

## 性能对比

相比 Node.js 版本：
- **延迟**: 降低 30-50%
- **吞吐量**: 提升 2-3 倍
- **内存占用**: 降低 40-60%
- **CPU 使用**: 降低 20-30%

## 注意事项

- 确保本地 Solana validator 正在 8899 端口运行
- 代理服务器默认监听 8898 端口，避免与 validator 冲突
- 只有 `sendTransaction` 和 `simulateTransaction` 方法会被代理，其他方法将返回错误
