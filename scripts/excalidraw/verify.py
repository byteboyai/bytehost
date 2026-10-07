#!/usr/bin/env python3
"""V2 自动验证:把打包好的应用目录交给真实 gateway(严格 CSP)与受限 wry webview(`spike/v2-excalidraw`),
断言:无 inline 脚本违规、资源路径指向本站、画布出现、全部绘图字体能从本站加载、`window.open` 被拒、
Service Worker 注册、localStorage 可写。会短暂弹出一个窗口(约 20 秒)。
用法: verify.py <输出应用目录> [--fixed-port 24681]
退出码非 0 = 有断言失败(同时打印整份探测结果)。
"""
import json, subprocess, sys, os

app = sys.argv[1]
port = sys.argv[sys.argv.index("--fixed-port") + 1] if "--fixed-port" in sys.argv else "0"
spike = os.path.join(os.path.dirname(__file__), "../../../spike/v2-excalidraw")
proc = subprocess.run(
    ["cargo", "run", "-q", "--", "--dir", f"{app}/web", "--port", port, "--wait", "10"],
    cwd=spike, capture_output=True, text=True, timeout=600,
)
line = next((l for l in proc.stdout.splitlines() if l.startswith("RESULT ")), None)
if line is None:
    sys.exit("没有拿到探测结果:\n" + proc.stderr[-2000:])
r = json.loads(line[7:])
fails = []
def need(cond, msg):
    if not cond:
        fails.append(msg)
need(not any(v.startswith("script-src") for v in r["csp"]), "有内联/外部脚本被 CSP 拦截")
need(r["assetPath"] == 'object:["/"]', "EXCALIDRAW_ASSET_PATH 不是 [/]: " + r["assetPath"])
need(r["canvases"] >= 1 and r["excalidrawRoot"], "Excalidraw 没有渲染出来")
for fam in ["Excalifont", "Virgil", "Cascadia", "Nunito", "Comic Shanns", "Assistant", "Lilita One", "Liberation Sans"]:
    need(r["fontLoad"].get(fam, "").endswith("status=loaded"), f"字体 {fam} 没能从本站加载: {r['fontLoad'].get(fam)}")
need(r["windowOpen"] == "null", f"window.open 没被拒: {r['windowOpen']}")
need(r["swRegs"] == 1, f"Service Worker 注册数 {r['swRegs']}")
need(r["localStorage"] == "1", "localStorage 不可写")
need(r["deniedNavigations"] == [], f"有导航被拒: {r['deniedNavigations']}")
noise = [v for v in r["csp"] if not v.startswith("font-src <- https://esm.sh/@excalidraw/excalidraw/dist/prod/fonts/")]
need(noise == [], f"除 esm.sh 字体回退源之外还有 CSP 违规: {noise[:5]}")
print(json.dumps({k: r[k] for k in ["assetPath", "canvases", "fontLoad", "windowOpen", "swRegs", "download", "downloadAttempts", "clipWrite", "clipRead"]}, ensure_ascii=False, indent=1))
if fails:
    print("FAIL:\n- " + "\n- ".join(fails))
    sys.exit(1)
print("V2 OK(esm.sh 字体回退源产生的 CSP 违规是已知噪音:字体已从本站加载)")
