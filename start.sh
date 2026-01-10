#!/bin/bash

# Solana Proxy 启动脚本
# 使用方法: ./start.sh [stop|restart|status]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_NAME="solana-proxy"
BINARY_PATH="$SCRIPT_DIR/target/release/$BINARY_NAME"
PID_FILE="$SCRIPT_DIR/$BINARY_NAME.pid"
LOG_FILE="$SCRIPT_DIR/proxy.log"
NOHUP_LOG="$SCRIPT_DIR/nohup.out"

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# 检查进程是否运行
check_running() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if ps -p "$PID" > /dev/null 2>&1; then
            return 0
        else
            rm -f "$PID_FILE"
            return 1
        fi
    fi
    return 1
}

# 停止服务
stop_service() {
    if check_running; then
        PID=$(cat "$PID_FILE")
        echo -e "${YELLOW}正在停止 $BINARY_NAME (PID: $PID)...${NC}"
        kill "$PID" 2>/dev/null
        
        # 等待进程结束
        for i in {1..10}; do
            if ! ps -p "$PID" > /dev/null 2>&1; then
                break
            fi
            sleep 1
        done
        
        # 如果还在运行，强制杀死
        if ps -p "$PID" > /dev/null 2>&1; then
            echo -e "${YELLOW}强制停止进程...${NC}"
            kill -9 "$PID" 2>/dev/null
        fi
        
        rm -f "$PID_FILE"
        echo -e "${GREEN}$BINARY_NAME 已停止${NC}"
    else
        echo -e "${YELLOW}$BINARY_NAME 未运行${NC}"
    fi
}

# 查看状态
show_status() {
    if check_running; then
        PID=$(cat "$PID_FILE")
        echo -e "${GREEN}$BINARY_NAME 正在运行 (PID: $PID)${NC}"
        echo "日志文件: $LOG_FILE"
        echo "Nohup 日志: $NOHUP_LOG"
        return 0
    else
        echo -e "${RED}$BINARY_NAME 未运行${NC}"
        return 1
    fi
}

# 编译项目
build_project() {
    echo -e "${YELLOW}正在编译项目 (release 模式)...${NC}"
    cd "$SCRIPT_DIR" || exit 1
    
    if cargo build --release; then
        echo -e "${GREEN}编译成功${NC}"
        return 0
    else
        echo -e "${RED}编译失败${NC}"
        return 1
    fi
}

# 启动服务
start_service() {
    # 检查是否已在运行
    if check_running; then
        PID=$(cat "$PID_FILE")
        echo -e "${RED}错误: $BINARY_NAME 已在运行 (PID: $PID)${NC}"
        echo "使用 './start.sh stop' 停止服务，或 './start.sh restart' 重启服务"
        exit 1
    fi
    
    # 检查可执行文件是否存在
    if [ ! -f "$BINARY_PATH" ]; then
        echo -e "${YELLOW}可执行文件不存在，开始编译...${NC}"
        if ! build_project; then
            exit 1
        fi
    fi
    
    # 启动服务
    echo -e "${YELLOW}正在启动 $BINARY_NAME...${NC}"
    cd "$SCRIPT_DIR" || exit 1
    
    # 使用 nohup 后台运行，输出到日志文件
    nohup "$BINARY_PATH" > "$LOG_FILE" 2>&1 &
    PID=$!
    
    # 保存 PID
    echo "$PID" > "$PID_FILE"
    
    # 等待一下，检查进程是否成功启动
    sleep 2
    if ps -p "$PID" > /dev/null 2>&1; then
        echo -e "${GREEN}$BINARY_NAME 启动成功 (PID: $PID)${NC}"
        echo "日志文件: $LOG_FILE"
        echo "使用 './start.sh stop' 停止服务"
        echo "使用 './start.sh status' 查看状态"
        echo "使用 'tail -f $LOG_FILE' 查看实时日志"
    else
        echo -e "${RED}$BINARY_NAME 启动失败${NC}"
        rm -f "$PID_FILE"
        
        # 检查是否是端口占用问题
        if grep -q "Address already in use" "$LOG_FILE" 2>/dev/null; then
            echo -e "${RED}错误: 端口已被占用${NC}"
            echo "可能已有实例在运行，请先执行: ./start.sh stop"
            echo "或检查端口占用: lsof -i :8898"
        else
            echo "请查看日志: $LOG_FILE"
        fi
        exit 1
    fi
}

# 重启服务
restart_service() {
    build_project
    stop_service
    sleep 1
    start_service
}

# 主逻辑
case "${1:-start}" in
    stop)
        stop_service
        ;;
    restart)
        restart_service
        ;;
    status)
        show_status
        ;;
    *)
        echo "使用方法: $0 [start|stop|restart|status|build]"
        echo ""
        echo "命令说明:"
        echo "  start   - 启动服务（默认）"
        echo "  stop    - 停止服务"
        echo "  restart - 重启服务"
        echo "  status  - 查看运行状态"
        echo "  build   - 编译项目（release 模式）"
        exit 1
        ;;
esac
