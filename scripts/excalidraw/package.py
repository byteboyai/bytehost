#!/usr/bin/env python3
"""把 excalidraw.com 的生产静态构建(官方 Docker 镜像 `/usr/share/nginx/html`)整理成能在 bytehost 严格 CSP
(`script-src 'self'`,无 inline;`connect-src/font-src 'self'`)下工作的应用目录:

- 所有内联 `<script>` 原样外置成 `/bh-inline-N.js`(顺序不变),但**去掉**三类外联行为:统计脚本(simpleanalytics)、
  Excalidraw+ 自动跳转、以及指向 CDN 的 `EXCALIDRAW_ASSET_PATH`(改成 `["/"]`,字体与库从本站取);
- 去掉指向外部站点的 `<link rel=preload/preconnect>`(它们本来就会被 CSP 拦掉,只会刷违规日志)。
- 构建出的 CSS 里 UI 字体 Assistant 的 `@font-face` 写死指向官方 CDN(`.../oss/fonts/Assistant/*.woff2`),而静态构建里只带了
  Regular 一个:打包时把 CDN 前缀改成本站 `/`,并从 CDN **下载**缺的另外三个字重(打包是联网的构建步骤,运行时仍是 `network: none`);
  下载失败就用本地的 `Assistant-Regular.woff2` 顶替(字重由浏览器合成,没有 CSP 违规)。
输出布局(可直接作为 `LocalDir` 安装源):`<输出目录>/manifest.toml` + `<输出目录>/web/`(站点)。
用法: package.py <官方镜像里的 html 目录> <输出应用目录>
"""
import os, re, shutil, sys, tempfile, urllib.request

src = sys.argv[1]
app_final = os.path.abspath(sys.argv[2])
if not os.path.isfile(f"{src}/index.html"):
    sys.exit(f"源目录不对(没有 index.html): {src}")
# 输出路径要么不存在、要么是空目录、要么是**上一次的打包输出**(有 manifest.toml 和 web/)——
# 否则拒绝,不能把用户随手传来的目录(如 ~)整个删掉。
if os.path.exists(app_final):
    is_empty_dir = os.path.isdir(app_final) and not os.listdir(app_final)
    is_old_output = os.path.isfile(f"{app_final}/manifest.toml") and os.path.isdir(f"{app_final}/web")
    if not (is_empty_dir or is_old_output):
        sys.exit(f"拒绝覆盖 {app_final}:它既不是空目录,也不是上一次的打包输出(需含 manifest.toml 与 web/)")
# 先在同级临时目录里做完并自检,成功了再整体换上去:中途失败不会留下半成品,也不会先毁掉旧输出。
app = tempfile.mkdtemp(prefix=".bh-excalidraw-", dir=os.path.dirname(app_final))
out = f"{app}/web"
shutil.rmtree(app)
os.makedirs(app)
shutil.copytree(src, out)
html = open(f"{out}/index.html", encoding="utf-8").read()

n = 0
def externalize(m):
    global n
    attrs, body = m.group(1) or "", m.group(2)
    if "src=" in attrs:  # 本来就是外联脚本(模块入口)
        return m.group(0)
    text = body.strip()
    if not text:
        return ""
    if "simpleanalyticscdn" in text or "excplus-autoredirect" in text:
        return ""  # 统计 + Excalidraw+ 跳转:不要
    if "EXCALIDRAW_ASSET_PATH" in text:
        text = 'window.EXCALIDRAW_ASSET_PATH = ["/"];'
    n += 1
    name = f"bh-inline-{n}.js"
    open(f"{out}/{name}", "w", encoding="utf-8").write(text + "\n")
    return f'<script src="/{name}"></script>'

html = re.sub(r"<script((?:\s[^>]*)?)>(.*?)</script>", externalize, html, flags=re.S)
html = re.sub(r'<link rel="(?:preload|preconnect)"[^>]*https://[^>]*>\s*', "", html)
open(f"{out}/index.html", "w", encoding="utf-8").write(html)
left = re.findall(r"<script(?:\s[^>]*)?>(?!\s*</script>)[^<]", html)
ext = len(re.findall(r'https://', html))
print(f"externalized {n} inline scripts; remaining inline: {len(left)}; remaining https:// mentions in index.html: {ext}")

CDN = "https://excalidraw.nyc3.cdn.digitaloceanspaces.com/oss/"
for css in [f for f in os.listdir(f"{out}/assets") if f.endswith(".css")]:
    path = f"{out}/assets/{css}"
    text = open(path, encoding="utf-8").read()
    wanted = sorted(set(re.findall(re.escape(CDN) + r"(fonts/[A-Za-z]+/[^\"')\s]+)", text)))
    for rel in wanted:
        dest = f"{out}/{rel}"
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        if os.path.exists(dest):
            continue
        try:
            with urllib.request.urlopen(CDN + rel, timeout=20) as r, open(dest, "wb") as f:
                f.write(r.read())
            print("fetched", rel)
        except Exception as e:  # 离线:用 Regular 顶替
            fallback = f"{out}/Assistant-Regular.woff2"
            if os.path.exists(fallback):
                shutil.copy(fallback, dest)
                print("fallback", rel, "<-", "Assistant-Regular.woff2", f"({e})")
    open(path, "w", encoding="utf-8").write(text.replace(CDN, "/"))

# 自检:打包产物必须满足严格 CSP(`script-src 'self'`、字体只能来自本站)——不满足就失败,不静默放过。
problems = []
if left:
    problems.append("index.html 里还有内联脚本")
for css in [f for f in os.listdir(f"{out}/assets") if f.endswith(".css")]:
    if CDN in open(f"{out}/assets/{css}", encoding="utf-8").read():
        problems.append(f"{css} 里还有指向 CDN 的字体地址")
for need in ["fonts/Assistant/Assistant-Regular.woff2", "fonts/Excalifont", "fonts/Virgil", "fonts/Nunito"]:
    if not os.path.exists(f"{out}/{need}"):
        problems.append(f"缺少 {need}")
if problems:
    sys.exit("打包自检失败: " + "; ".join(problems))

shutil.copy(os.path.join(os.path.dirname(os.path.abspath(__file__)), "manifest.toml"), f"{app}/manifest.toml")
if os.path.exists(app_final):
    shutil.rmtree(app_final)
os.rename(app, app_final)
print("OK:", app_final)
