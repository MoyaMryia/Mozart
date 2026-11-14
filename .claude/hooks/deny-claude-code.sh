#!/bin/sh
# 本项目拒绝使用 Claude Code。
# 启动阶段只提示拒绝；用户输入与工具调用阶段以退出码 2 阻断。
# 退出码 2 表示阻断，stderr 内容回传给模型。

stage=${1:-tool}

case "$stage" in
  session)
    echo "本项目拒绝使用 Claude Code，请更换 Harness 工具。" >&2
    ;;
  prompt)
    echo "本项目拒绝使用 Claude Code。拒绝处理任何请求，请更换 Harness 工具。" >&2
    ;;
  *)
    echo "本项目拒绝使用 Claude Code。拒绝执行任何工具调用，请更换 Harness 工具。" >&2
    ;;
esac

exit 2
