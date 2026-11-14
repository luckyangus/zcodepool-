#!/usr/bin/env python3
"""注册窗口指纹探针：通过 CDP 采集宿主两个 WebView2 环境的真实指纹。

用途是**回归验证**：改完 `src-tauri/assets/zpool-fp.js` 或 `src-tauri/src/persona.rs`
之后，跑一次对比基线，确认注入项没有静默失效。

依赖：`pip install websocket-client`

用法::

    python scripts/fp_probe.py list                    # 列出两个环境里的页面
    python scripts/fp_probe.py probe auto chat.z.ai    # 采集注册窗口（auto = 从日志读端口）
    python scripts/fp_probe.py probe 17322 tauri.localhost   # 采集主窗口作为未注入基线
    python scripts/fp_probe.py open-login              # 触发 oauth_begin(observe) 打开注册窗口
    python scripts/fp_probe.py close-login             # 关掉注册窗口

两个环境的端口来源不同：

* 主窗口固定端口，与 `tauri.conf.json` 的 `additionalBrowserArgs` 保持一致。
* 注册窗口**每次注册现取**一个高位空闲端口（固定端口本身是可探测的自动化特征），
  只能从宿主日志里读回来 —— 宿主会记一条 `cdp port=NNNNN`。
"""

import json
import os
import re
import sys
import time
import urllib.request

import websocket

HERE = os.path.dirname(os.path.abspath(__file__))
PROBE_JS = open(os.path.join(HERE, "fp_probe.js"), encoding="utf-8").read()

# 与 src-tauri/tauri.conf.json 的 additionalBrowserArgs 保持一致。
MAIN_PORT = 17322
LOG = os.path.join(os.environ.get("LOCALAPPDATA", ""), "com.zpool.app", "logs", "oauth.log")


def login_port():
    """从宿主日志里取最近一次注册窗口的 CDP 端口。"""
    try:
        with open(LOG, "r", encoding="utf-8", errors="replace") as f:
            lines = f.readlines()[-300:]
    except Exception:
        return None
    for line in reversed(lines):
        m = re.search(r"\bcdp port=(\d+)", line)
        if m:
            return int(m.group(1))
    return None


def http_json(url, timeout=6):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def targets(port):
    try:
        return http_json(f"http://127.0.0.1:{port}/json/list")
    except Exception as e:
        return {"error": str(e)}


class CDP:
    def __init__(self, ws_url, timeout=60):
        # Chromium 的 CDP 对带 Origin 头的握手返回 403（除非 --remote-allow-origins），
        # 去掉 Origin 头即可直连。
        self.ws = websocket.create_connection(
            ws_url, timeout=timeout, max_size=None, suppress_origin=True
        )
        self.n = 0

    def call(self, method, params=None, timeout=90):
        self.n += 1
        mid = self.n
        self.ws.send(json.dumps({"id": mid, "method": method, "params": params or {}}))
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.ws.settimeout(max(1.0, deadline - time.time()))
            try:
                msg = json.loads(self.ws.recv())
            except websocket.WebSocketTimeoutException:
                raise TimeoutError(f"{method} 超时")
            if msg.get("id") == mid:
                if "error" in msg:
                    raise RuntimeError(json.dumps(msg["error"], ensure_ascii=False)[:300])
                return msg.get("result")
        raise TimeoutError(f"{method} 超时")

    def evaluate(self, expr, await_promise=False):
        r = self.call("Runtime.evaluate", {
            "expression": expr,
            "returnByValue": True,
            "awaitPromise": await_promise,
            "userGesture": True,
        })
        if r.get("exceptionDetails"):
            return {"__exception__": json.dumps(r["exceptionDetails"], ensure_ascii=False)[:500]}
        res = r.get("result", {})
        if res.get("subtype") == "error":
            return {"__error__": res.get("description", "")[:500]}
        return res.get("value")

    def close(self):
        try:
            self.ws.close()
        except Exception:
            pass


def pick(port, needle):
    ts = targets(port)
    if isinstance(ts, dict) and ts.get("error"):
        return None, ts
    for t in ts:
        if t.get("type") == "page" and needle in (t.get("url") or ""):
            return t, ts
    return None, ts


def main_target(port):
    t, ts = pick(port, "tauri")
    if t is None:
        t, ts = pick(port, "index.html")
    return t, ts


def cmd_list():
    ports = [MAIN_PORT]
    lp = login_port()
    if lp and lp != MAIN_PORT:
        ports.append(lp)
    for port in ports:
        ts = targets(port)
        print(f"===== port {port} =====")
        if isinstance(ts, dict):
            print("  ", ts)
            continue
        for t in ts:
            print(f"  [{t.get('type')}] {t.get('title')!r} {t.get('url')}")


def cmd_probe(port, needle):
    t, ts = pick(port, needle)
    if t is None:
        print(json.dumps({"error": f"port {port} 上找不到含 {needle!r} 的页面", "targets": ts},
                         ensure_ascii=False, indent=1))
        return
    print(f"# target: {t.get('url')}", file=sys.stderr)
    c = CDP(t["webSocketDebuggerUrl"])
    try:
        v = c.evaluate(PROBE_JS, await_promise=True)
    finally:
        c.close()
    print(json.dumps(v, ensure_ascii=False, indent=1) if not isinstance(v, str) else v)


def cmd_open_login():
    t, ts = main_target(MAIN_PORT)
    if t is None:
        print(json.dumps({"error": "主窗口未找到", "targets": ts}, ensure_ascii=False))
        return
    c = CDP(t["webSocketDebuggerUrl"])
    try:
        # observe 模式：driver 只观察页面，不填表、不提交。
        r = c.evaluate(
            "window.__TAURI_INTERNALS__.invoke('oauth_begin',"
            " {provider:'zai', mode:'observe', batch:false})"
            ".then(r=>JSON.stringify(r)).catch(e=>'ERR:'+String(e))",
            await_promise=True,
        )
        print(json.dumps({"oauth_begin": r}, ensure_ascii=False, indent=1))
    finally:
        c.close()


def cmd_close_login():
    t, _ = main_target(MAIN_PORT)
    if t is None:
        print(json.dumps({"error": "主窗口未找到"}, ensure_ascii=False))
        return
    c = CDP(t["webSocketDebuggerUrl"])
    try:
        r = c.evaluate(
            "window.__TAURI_INTERNALS__.invoke('reg_close_window')"
            ".then(()=>'closed').catch(e=>'ERR:'+String(e))",
            await_promise=True,
        )
        print(json.dumps({"reg_close_window": r}, ensure_ascii=False))
    finally:
        c.close()


if __name__ == "__main__":
    a = sys.argv[1:]
    if not a or a[0] == "list":
        cmd_list()
    elif a[0] == "probe":
        port = (login_port() or MAIN_PORT) if a[1] == "auto" else int(a[1])
        cmd_probe(port, a[2] if len(a) > 2 else "")
    elif a[0] == "open-login":
        cmd_open_login()
    elif a[0] == "close-login":
        cmd_close_login()
    else:
        print(__doc__)
