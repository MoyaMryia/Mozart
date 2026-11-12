# jget.sh — status JSON 取值函数，tools/jetson-smoke.sh 与 tools/jetson-measure.sh
# 共用。原先两脚本各内嵌一份 Python 片段且已分叉，现以 measure 版为准统一
# （多键能力保留，smoke 侧为零增益超集）。
# 用法：jget <status.json> <key> [子键]；key 取值：
#   pre       → "yes"/"no"（pre 段是否存在）
#   rvcstate  → rvc.state（缺省为空串）
#   rvcproc   → processes.rvc[子键]
#   preproc   → processes.pre[子键]
# 子键仅 rvcproc/preproc 用；其余调用没有第三参，set -u 下由调用方传空默认。
jget() { python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
key = sys.argv[2]
if key == "pre":
    print("yes" if d.get("pre") else "no")
elif key == "rvcstate":
    print((d.get("rvc") or {}).get("state") or "")
elif key == "rvcproc":
    print(d["processes"]["rvc"][sys.argv[3]])
elif key == "preproc":
    print(d["processes"]["pre"][sys.argv[3]])
' "$1" "$2" "${3:-}" 2>/dev/null; }
