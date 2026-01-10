package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net/http"
	"time"
)

var (
	proxyPort    = flag.String("port", "8898", "Proxy server port")
	validatorURL = flag.String("validator", "http://localhost:8899", "Solana validator URL")
	// 允许的RPC方法列表
	allowedMethods = map[string]bool{
		"sendTransaction":     true,
		"simulateTransaction": true,
		"getLatestBlockhash":  true,
	}
)

type JSONRPCRequest struct {
	JSONRPC string        `json:"jsonrpc"`
	ID      interface{}   `json:"id"`
	Method  string        `json:"method"`
	Params  []interface{} `json:"params"`
}

type JSONRPCResponse struct {
	JSONRPC string      `json:"jsonrpc"`
	ID      interface{} `json:"id"`
	Result  interface{} `json:"result,omitempty"`
	Error   *RPCError   `json:"error,omitempty"`
}

type RPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
	Data    string `json:"data,omitempty"`
}

func main() {
	flag.Parse()

	// 创建HTTP客户端，配置超时和连接池
	client := &http.Client{
		Timeout: 30 * time.Second,
		Transport: &http.Transport{
			MaxIdleConns:        100,
			MaxIdleConnsPerHost: 10,
			IdleConnTimeout:     90 * time.Second,
		},
	}

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		handleProxyRequest(w, r, client)
	})

	http.HandleFunc("/health", handleHealth)

	addr := ":" + *proxyPort
	log.Printf("=================================")
	log.Printf("Solana Validator Proxy Server")
	log.Printf("=================================")
	log.Printf("Listening on port: %s", *proxyPort)
	log.Printf("Proxying to: %s", *validatorURL)
	log.Printf("Allowed methods: sendTransaction, simulateTransaction")
	log.Printf("=================================")

	if err := http.ListenAndServe(addr, nil); err != nil {
		log.Fatalf("Server failed to start: %v", err)
	}
}

func handleProxyRequest(w http.ResponseWriter, r *http.Request, client *http.Client) {
	if r.Method != http.MethodPost {
		http.Error(w, "Method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// 读取请求体
	body, err := io.ReadAll(r.Body)
	if err != nil {
		log.Printf("[ERROR] Failed to read request body: %v", err)
		sendErrorResponse(w, nil, -32700, "Parse error", "")
		return
	}
	defer r.Body.Close()

	// 解析JSON-RPC请求
	var req JSONRPCRequest
	if err := json.Unmarshal(body, &req); err != nil {
		log.Printf("[ERROR] Failed to parse JSON: %v", err)
		sendErrorResponse(w, nil, -32700, "Parse error", err.Error())
		return
	}

	// 检查方法名
	if !allowedMethods[req.Method] {
		log.Printf("[BLOCKED] Method: %s - not allowed", req.Method)
		sendErrorResponse(w, req.ID, -32601,
			fmt.Sprintf("Method not allowed: %s. Allowed methods: sendTransaction, simulateTransaction", req.Method), "")
		return
	}

	log.Printf("[ALLOWED] Method: %s, ID: %v", req.Method, req.ID)

	// 创建转发请求
	proxyReq, err := http.NewRequest(http.MethodPost, *validatorURL, bytes.NewReader(body))
	if err != nil {
		log.Printf("[ERROR] Failed to create proxy request: %v", err)
		sendErrorResponse(w, req.ID, -32603, "Internal proxy error", err.Error())
		return
	}

	// 复制请求头
	proxyReq.Header.Set("Content-Type", "application/json")
	for key, values := range r.Header {
		if key != "Host" && key != "Content-Length" {
			for _, value := range values {
				proxyReq.Header.Add(key, value)
			}
		}
	}

	// 发送请求到validator
	start := time.Now()
	resp, err := client.Do(proxyReq)
	if err != nil {
		log.Printf("[ERROR] Proxy request failed: %v", err)
		sendErrorResponse(w, req.ID, -32603, "Internal proxy error", err.Error())
		return
	}
	defer resp.Body.Close()

	duration := time.Since(start)
	log.Printf("[PROXY] Response status: %d, duration: %v", resp.StatusCode, duration)

	// 读取响应体
	respBody, err := io.ReadAll(resp.Body)
	if err != nil {
		log.Printf("[ERROR] Failed to read response body: %v", err)
		sendErrorResponse(w, req.ID, -32603, "Internal proxy error", err.Error())
		return
	}

	// 复制响应头
	for key, values := range resp.Header {
		for _, value := range values {
			w.Header().Add(key, value)
		}
	}
	w.WriteHeader(resp.StatusCode)

	// 写入响应体
	if _, err := w.Write(respBody); err != nil {
		log.Printf("[ERROR] Failed to write response: %v", err)
	}
}

func handleHealth(w http.ResponseWriter, r *http.Request) {
	health := map[string]interface{}{
		"status":        "ok",
		"timestamp":     time.Now().Format(time.RFC3339),
		"validator_url": *validatorURL,
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(health)
}

func sendErrorResponse(w http.ResponseWriter, id interface{}, code int, message, data string) {
	response := JSONRPCResponse{
		JSONRPC: "2.0",
		ID:      id,
		Error: &RPCError{
			Code:    code,
			Message: message,
			Data:    data,
		},
	}

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(response)
}
