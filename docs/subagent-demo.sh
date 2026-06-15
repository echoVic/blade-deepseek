#!/usr/bin/env bash
# Subagent 交互演示脚本

set -e

ORCA="./target/release/orca"

echo "=== Orca Subagent 交互演示 ==="
echo

# 检查 orca 二进制是否存在
if [ ! -f "$ORCA" ]; then
    echo "❌ 未找到 orca 二进制文件，正在构建..."
    cargo build --release
    echo "✅ 构建完成"
    echo
fi

echo "📋 演示场景："
echo "  1. 成功的子代理调用"
echo "  2. 嵌套子代理被拒绝"
echo "  3. 子代理失败传播"
echo

# ============================================
# 场景 1: 成功的子代理调用
# ============================================
echo "=== 场景 1: 成功的子代理调用 ==="
echo "命令: orca exec --output-format jsonl --provider mock 'subagent inspect repo'"
echo

OUTPUT=$($ORCA exec --output-format jsonl --provider mock "subagent inspect repo" 2>&1)

echo "关键事件："
echo "$OUTPUT" | jq -c 'select(.type | contains("subagent") or .type == "tool.call.requested" or .type == "tool.call.completed")' | while read -r line; do
    TYPE=$(echo "$line" | jq -r '.type')
    case "$TYPE" in
        "tool.call.requested")
            NAME=$(echo "$line" | jq -r '.payload.name')
            TARGET=$(echo "$line" | jq -r '.payload.target')
            echo "  ✅ [tool.call.requested] name=$NAME, target=$TARGET"
            ;;
        "subagent.started")
            ID=$(echo "$line" | jq -r '.payload.id')
            DESC=$(echo "$line" | jq -r '.payload.description')
            echo "  🚀 [subagent.started] id=$ID, description='$DESC'"
            ;;
        "subagent.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            ERROR=$(echo "$line" | jq -r '.payload.error')
            echo "  🏁 [subagent.completed] status=$STATUS, error=$ERROR"
            ;;
        "tool.call.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            echo "  ✅ [tool.call.completed] status=$STATUS"
            ;;
    esac
done

FINAL_STATUS=$(echo "$OUTPUT" | jq -r 'select(.type == "session.completed") | .payload.status')
echo "  📊 最终状态: $FINAL_STATUS"
echo

# ============================================
# 场景 2: 嵌套子代理被拒绝
# ============================================
echo "=== 场景 2: 嵌套子代理被拒绝 ==="
echo "命令: orca exec --output-format jsonl --provider mock 'subagent subagent inner task'"
echo

OUTPUT=$($ORCA exec --output-format jsonl --provider mock "subagent subagent inner task" 2>&1 || true)

echo "关键事件："
echo "$OUTPUT" | jq -c 'select(.type | contains("subagent") or .type == "session.completed")' | while read -r line; do
    TYPE=$(echo "$line" | jq -r '.type')
    case "$TYPE" in
        "subagent.started")
            DESC=$(echo "$line" | jq -r '.payload.description')
            echo "  🚀 [subagent.started] description='$DESC'"
            ;;
        "subagent.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            ERROR=$(echo "$line" | jq -r '.payload.error // empty')
            if [ -n "$ERROR" ]; then
                echo "  ❌ [subagent.completed] status=$STATUS"
                echo "     错误: $ERROR"
            else
                echo "  🏁 [subagent.completed] status=$STATUS"
            fi
            ;;
        "session.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            echo "  📊 最终状态: $STATUS"
            ;;
    esac
done
echo

# ============================================
# 场景 3: 子代理失败传播
# ============================================
echo "=== 场景 3: 子代理失败传播 ==="
echo "命令: orca exec --output-format jsonl --provider mock 'subagent mock_fail'"
echo

OUTPUT=$($ORCA exec --output-format jsonl --provider mock "subagent mock_fail" 2>&1 || true)

echo "关键事件："
echo "$OUTPUT" | jq -c 'select(.type | contains("subagent") or .type == "tool.call.completed" or .type == "session.completed")' | while read -r line; do
    TYPE=$(echo "$line" | jq -r '.type')
    case "$TYPE" in
        "subagent.started")
            DESC=$(echo "$line" | jq -r '.payload.description')
            echo "  🚀 [subagent.started] description='$DESC'"
            ;;
        "subagent.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            ERROR=$(echo "$line" | jq -r '.payload.error // empty')
            if [ -n "$ERROR" ]; then
                echo "  ❌ [subagent.completed] status=$STATUS"
                echo "     错误: $ERROR"
            else
                echo "  🏁 [subagent.completed] status=$STATUS"
            fi
            ;;
        "tool.call.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            echo "  ⚠️  [tool.call.completed] status=$STATUS"
            ;;
        "session.completed")
            STATUS=$(echo "$line" | jq -r '.payload.status')
            echo "  📊 最终状态: $STATUS"
            ;;
    esac
done
echo

# ============================================
# 事件流详细分析
# ============================================
echo "=== 事件流详细分析 ==="
echo "运行: orca exec --output-format jsonl --provider mock 'subagent analyze code'"
echo

OUTPUT=$($ORCA exec --output-format jsonl --provider mock "subagent analyze code" 2>&1)

echo "完整事件序列："
echo "$OUTPUT" | jq -c '.' | nl -v 0 -w 2 -s '. ' | while read -r line; do
    SEQ=$(echo "$line" | awk '{print $1}')
    TYPE=$(echo "$line" | cut -d' ' -f2- | jq -r '.type')

    case "$TYPE" in
        "session.started")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, cwd: .payload.cwd, approval_mode: .payload.approval_mode}'
            ;;
        "turn.started")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, turn: .payload.turn, prompt: .payload.prompt}'
            ;;
        "assistant.reasoning.delta"|"assistant.message.delta")
            TEXT=$(echo "$line" | cut -d' ' -f2- | jq -r '.payload.text' | head -c 50)
            echo "$line" | cut -d' ' -f2- | jq -c --arg text "$TEXT..." '{seq: .seq, type, text: $text}'
            ;;
        "tool.call.requested")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, payload: {id: .payload.id, name: .payload.name, action: .payload.action, target: .payload.target}}'
            ;;
        "subagent.started")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, payload: {id: .payload.id, description: .payload.description}}'
            ;;
        "subagent.completed")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, payload: {id: .payload.id, status: .payload.status, has_output: (.payload.output != null), has_error: (.payload.error != null)}}'
            ;;
        "tool.call.completed")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, payload: {id: .payload.id, name: .payload.name, status: .payload.status, exit_code: .payload.exit_code}}'
            ;;
        "session.completed")
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type, status: .payload.status}'
            ;;
        *)
            echo "$line" | cut -d' ' -f2- | jq -c '{seq: .seq, type}'
            ;;
    esac
done

echo
echo "=== 演示完成 ==="
echo
echo "💡 关键观察："
echo "  1. Subagent 有独立的事件序列 (subagent.started → subagent.completed)"
echo "  2. Subagent 嵌套在 tool.call 事件中"
echo "  3. 子代理失败会导致父代理失败"
echo "  4. 嵌套调用被明确拒绝（MAX_SUBAGENT_DEPTH=1）"
echo "  5. 所有事件都有序列号 (seq) 和时间戳"
