# jget.sh — status JSON 取值函数，tools/jetson-smoke.sh、tools/jetson-measure.sh
# 与 tools/jetson-file-test.sh 共用。原先两脚本各内嵌一份 Python 片段且已分叉，
# 现以 measure 版为准统一（多键能力保留，smoke 侧为零增益超集）。
# 用法：jget <json文件> <key> [子键]；key 取值：
#   pre       → "yes"/"no"（pre 段是否存在）
#   rvcstate  → rvc.state（缺省为空串）
#   rvcproc   → processes.rvc[子键]
#   preproc   → processes.pre[子键]
#   path      → 子键为点分路径（如 route.desired、models.0.exists），逐层取值；
#               缺失输出空串，布尔输出 true/false，对象/数组输出紧凑 JSON。
#               文件链路的状态与任务字段多样，逐字段加 key 会让本文件膨胀。
# 子键仅 rvcproc/preproc/path 用；其余调用没有第三参，set -u 下由调用方传空默认。
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
elif key == "path":
    cur = d
    for part in sys.argv[3].split("."):
        if isinstance(cur, list):
            try:
                cur = cur[int(part)]
            except (ValueError, IndexError):
                cur = None
        elif isinstance(cur, dict):
            cur = cur.get(part)
        else:
            cur = None
        if cur is None:
            break
    if cur is None:
        print("")
    elif isinstance(cur, bool):
        print("true" if cur else "false")
    elif isinstance(cur, (dict, list)):
        print(json.dumps(cur, ensure_ascii=False))
    else:
        print(cur)
' "$1" "$2" "${3:-}" 2>/dev/null; }
