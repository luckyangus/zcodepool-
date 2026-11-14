#!/usr/bin/env python3
"""zcode-pool 滑块验证码求解器（CDP attach 模式）。

与 zai_captcha/solve.py 的关系：定位算法、筛选阈值、拖动轨迹完全一致；
区别只在**挂载方式** —— 这里不 launch 浏览器，而是 connect_over_cdp 挂到
宿主 WebView2 的登录窗口上。必须这么做：验证码 token 与会话（certifyId）绑定，
在另一个浏览器里解出来的结果搬不进注册表单。

用法：
    python captcha_solver.py [cdp_url] [max_attempts]

输出（stdout 最后一行 JSON）：
    {"ok": true, "attempts": 2, "code": "T001", "x": 187, "elapsed": 12.3}
    {"ok": false, "attempts": 5, "elapsed": 61.0, "error": "..."}
"""

import base64
import json
import sys
import time
import urllib.request
from io import BytesIO

# 强制 UTF-8 输出：Windows 上默认走 GBK，宿主按 UTF-8 读会拿到空串。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except Exception:
        pass

import numpy as np
from PIL import Image, ImageFilter
from playwright.sync_api import sync_playwright

# 碎片位置 = 0.00355×滑块² + 0.077×滑块（26 点拟合，跨会话稳定）
A, B = 0.00355, 0.077

# A/B 拟合与下面 RULE 的全部像素特征，都是在「主图 300px 宽、按 300px 显示」下标定的。
# 窗口一宽，验证码图片的显示宽度跟着变，这个基准就不再成立：
#   - 拖动距离按旧基准算 → 系统性偏大/偏小，缺口对不上，verify 请求要么失败要么不回；
#   - pix/rows/pw 是绝对像素量 → 分辨率一变，阈值全部误杀（表现为 8 轮 locateFail）。
# 所以：特征一律先归一化到这个基准，拖动距离再按实际显示宽度放大回去。
BASE_W = 300.0

# 来自优化版 rule.json（181 去重样本 / 275 人工标注，判别准确率 0.949；
# 目标函数是最小化在线期望延迟 E = 3 + 5(1-a)/a + 1.5(1-p)）。
# 注意：这里的特征依赖上面 locate() 的 min_abs=1e-3 与 margin/width 计算。
RULE = [
    {"field": "pix", "dir": "gt", "th": 834},
    {"field": "dip", "dir": "gt", "th": 0.1023},
    {"field": "rows", "dir": "lt", "th": 74},
    {"field": "pw", "dir": "lt", "th": 79},
]

READ_IMGS = """() => {
  const img = document.getElementById('aliyunCaptcha-img');
  const puz = document.getElementById('aliyunCaptcha-puzzle');
  const sl = document.getElementById('aliyunCaptcha-sliding-slider');
  return {main: img ? img.src : null, puzzle: puz ? puz.src : null,
          box: sl ? sl.getBoundingClientRect().toJSON() : null,
          imgBox: img ? img.getBoundingClientRect().toJSON() : null};
}"""

STATE = """() => {
  const wf = document.getElementById('aliyunCaptcha-window-float');
  const sl = document.getElementById('aliyunCaptcha-sliding-slider');
  const r = sl ? sl.getBoundingClientRect() : null;
  return {cls: wf ? String(wf.className) : null,
          slider: r ? {x: r.x, y: r.y, w: r.width, h: r.height} : null};
}"""

# 「点击开始验证」入口：必须去掉所有空白后精确匹配。
# 用 /点击开始验证/ 这类包含匹配会命中整屏根容器，点击等于落空（踩过）。
VERIFY_BTN = """() => {
  const norm = (s) => String(s || '').replace(/\\s+/g, '');
  const els = [...document.querySelectorAll('button,a,div,span')];
  const cands = els.filter(e => e.offsetParent !== null &&
                                /^(点击开始验证|Click to start verification)$/.test(norm(e.innerText)));
  if (!cands.length) return null;
  const btn = cands.find(e => e.tagName === 'BUTTON') || cands[cands.length - 1];
  const r = btn.getBoundingClientRect();
  return {x: r.x + r.width / 2, y: r.y + r.height / 2};
}"""

CLICK_START = """() => {
  const el = document.getElementById('aliyunCaptcha-captcha-body');
  if (el) el.click();
}"""


def log(msg):
    print(f"[solver] {msg}", file=sys.stderr, flush=True)


def hf(img, blur=2):
    """高频能量：原图与高斯模糊之差的绝对值。"""
    a = np.asarray(img.convert("L")).astype(np.float64)
    b = np.asarray(img.filter(ImageFilter.GaussianBlur(blur)).convert("L")).astype(np.float64)
    return np.abs(a - b)


def slider_for(target):
    disc = B * B + 4 * A * target
    return (-B + disc ** 0.5) / (2 * A)


def locate(main_img, puz_img, min_abs=1e-3, R=18):
    """缺口定位：碎片内容行范围内，高频能量相对邻域凹陷最明显的列。

    不能取全局最低 —— 背景/纯色区天然更模糊，必须用相对凹陷度。

    `min_abs` 只用于排除 ratio 退化到接近 0 的空白区，**绝不能设高**：
    实测真值处 ratio 的全局百分位中位仅 16.2%，且有 15.6% 的真值 ratio < 0.12
    （pw<22 的窄碎片更甚，最低 0.006）—— 旧的 0.12 会直接误杀正确位置。

    返回的 `best` 是**主图像素列**（给日志看），第二个返回值是已换算到
    BASE_W 基准的拖动距离；特征同样归一化到 BASE_W，RULE 才继续有效。
    """
    p = np.asarray(puz_img).astype(np.float64)
    alpha = p[:, :, 3] > 128
    pw = p.shape[1]
    mw, mh = main_img.size
    rows = np.where(alpha.any(axis=1))[0]
    if len(rows) == 0:
        log(f"locate 碎片图全透明（puz={puz_img.size}）")
        return None, None, None
    if pw > mw:
        log(f"locate 碎片比主图宽（pw={pw} > mw={mw}），图还没对齐")
        return None, None, None
    r0, r1 = int(rows.min()), int(rows.max())
    # 上界同时受主图高度和碎片图高度约束：碎片图比主图矮时（自测构造、或页面
    # 换了尺寸的过渡帧），alpha 切片会被 numpy 静默截断，而 m_hf 切片不会 ——
    # 两者行数一旦不等就是 IndexError，整个求解当场失败。
    r0e, r1e = max(0, r0 - 6), min(mh, p.shape[0], r1 + 7)
    m_hf = hf(main_img, 2)
    p_hf = hf(puz_img.convert("RGB"), 2)
    p_mean = float(p_hf[alpha].mean()) or 1.0
    mask = alpha[r0e:r1e, :]
    # 搜索范围由**主图实际宽度**决定，不再是写死的 300。
    n = mw - pw + 1
    ratio = np.full(n, 9.0)
    for x0 in range(n):
        if mask.any():
            ratio[x0] = float(m_hf[r0e:r1e, x0:x0 + pw][mask].mean()) / p_mean
    dip = np.zeros(n)
    for i in range(n):
        lo = max(0, i - R)
        hi = min(n, i + R + 1)
        neigh = np.concatenate([ratio[lo:i], ratio[i + 1:hi]])
        if len(neigh):
            dip[i] = np.median(neigh) - ratio[i]
    XL = 20
    valid = np.zeros(n, dtype=bool)
    valid[max(0, XL):max(0, n - XL)] = True
    valid &= ratio >= min_abs
    dv = dip.copy()
    dv[~valid] = -1e9
    if not valid.any():
        log(f"locate 无有效列（n={n} pw={pw} mw={mw} ratio_max={float(ratio.max()):.4f}）")
        return None, None, None
    best = int(np.argmax(dv))
    # 置信度特征：margin = best 与「次高峰」的差（次高峰排除 best 邻域 3pw）；
    # width = dip 半高处的峰宽。实测 margin 单调指示命中率：
    # 去重后 <0.062 -> 17.8%，0.109~0.186 -> 59.1%，>0.186 -> 80.9%。
    m2 = valid.copy()
    m2[max(0, best - 3 * pw):min(n, best + 3 * pw)] = False
    second = float(dip[m2].max()) if m2.any() else 0.0
    margin = float(dip[best]) - second
    half = dip[best] * 0.5
    w = 1
    i = best - 1
    while i >= 0 and dip[i] >= half:
        w += 1
        i -= 1
    i = best + 1
    while i < n and dip[i] >= half:
        w += 1
        i += 1
    # 像素量特征统一折算到 BASE_W 基准（尺寸没变时这些换算都是恒等）。
    sc = BASE_W / float(mw)
    best_base = best * sc
    return best, slider_for(best_base), {
        "pix": int(round(float(alpha.sum()) * sc * sc)),
        "dip": float(dip[best]),
        "rank_ratio": int(np.sum(ratio < ratio[best])) + 1,
        "ratio": float(ratio[best]),
        "pw": pw * sc,
        "rows": (r1 - r0 + 1) * sc,
        "margin": margin,
        "width": w * sc,
        "mw": mw,
    }


def passes_rule(m):
    for c in RULE:
        v = m.get(c["field"])
        if v is None:
            return False
        if c["dir"] == "gt":
            if not (v > c["th"]):
                return False
        elif not (v < c["th"]):
            return False
    return True


def load_img(u):
    if u.startswith("data:"):
        return Image.open(BytesIO(base64.b64decode(u.split(",", 1)[1])))
    return Image.open(BytesIO(urllib.request.urlopen(u, timeout=15).read()))


def find_page(browser):
    for ctx in browser.contexts:
        for p in ctx.pages:
            try:
                if "chat.z.ai" in p.url:
                    return p
            except Exception:
                continue
    return None


def captcha_ready(page):
    try:
        st = page.evaluate(STATE)
    except Exception:
        return False
    sl = st.get("slider") or {}
    return st.get("cls") == "window-show" and (sl.get("w") or 0) > 0


def ensure_open(page, tries=8):
    """确保验证码弹窗真正展开。

    两个坑（都踩过）：
      1. 弹窗未展开时滑块尺寸为 0、坐标在 (0,0)，此时拖动等于点在页面左上角；
      2. 「点击开始验证」必须去空白后精确匹配，包含匹配会命中整屏根容器。
    激活走 CDP 原生点击（isTrusted=true），WebView2 下实测有效。
    """
    for _ in range(tries):
        if captcha_ready(page):
            time.sleep(1.0)  # 等展开动画收尾，否则拿到的是动画中间坐标
            return True
        box = None
        try:
            box = page.evaluate(VERIFY_BTN)
        except Exception:
            box = None
        try:
            if box:
                page.mouse.click(box["x"], box["y"])
            else:
                page.evaluate(CLICK_START)
        except Exception:
            pass
        time.sleep(2.0)
    return False


MAIN_SRC_JS = """() => {
  const img = document.getElementById('aliyunCaptcha-img');
  return img ? img.src : null;
}"""

PAIR_SRC_JS = """() => {
  const img = document.getElementById('aliyunCaptcha-img');
  const puz = document.getElementById('aliyunCaptcha-puzzle');
  return [img ? img.src : null, puz ? puz.src : null];
}"""


def main_src(page):
    try:
        return page.evaluate(MAIN_SRC_JS)
    except Exception:
        return None


def imgs_src(page):
    """主图 + 碎片的 src 对。碎片必须一起看：只等主图换新时，
    碎片可能还是上一张 —— 拿新主图配旧碎片去定位，结果必然错位。"""
    try:
        return page.evaluate(PAIR_SRC_JS)
    except Exception:
        return None


def click_refresh(page, before=None, wait_sec=4.0):
    """换一张图，并且**等到图真的换了**才返回。

    以前这里只 `sleep(0.9)`：刷新请求慢于 0.9 秒时，下一轮 `READ_IMGS` 拿到的还是
    同一张图，定位必然再失败一次 —— 8 轮就这样空转完，最后返回 `attempts=0`。
    日志里那些 `elapsed≈15` 秒、`attempts=0` 的失败全是这个形态。

    `before` 是刷新前的图片 src（主图，或 `imgs_src` 给的主图+碎片对）；
    传 None 表示无从比较（比如图根本没取到），此时退回固定等待。
    """
    try:
        page.click("#aliyunCaptcha-btn-refresh", timeout=3500)
    except Exception:
        pass
    if before is None:
        time.sleep(0.9)
        return True
    deadline = time.time() + wait_sec
    while time.time() < deadline:
        time.sleep(0.25)
        if isinstance(before, (list, tuple)):
            now = imgs_src(page)
            if now and all(n and n != b for n, b in zip(now, before)):
                return True
        else:
            now = main_src(page)
            if now and now != before:
                return True
    return False


def drag(page, sx, sy, target):
    """CDP 原生鼠标事件（isTrusted=true）+ smoothstep 缓动。

    JS 合成事件（dispatchEvent）会被行为分析拒绝，必须走这里。
    """
    page.mouse.move(sx, sy)
    time.sleep(0.08)
    page.mouse.down()
    time.sleep(0.07)
    steps = 24
    for i in range(1, steps + 1):
        t = i / steps
        ease = t * t * (3 - 2 * t)
        dx = target * ease + (np.random.rand() - 0.5) * 1.2
        page.mouse.move(sx + dx, sy + (np.random.rand() - 0.5) * 1.4)
        time.sleep(0.014 + np.random.rand() * 0.026)
    time.sleep(0.1)
    page.mouse.move(sx + target, sy)
    time.sleep(0.07)
    page.mouse.up()


def connect_any(pw, cdps):
    """依次尝试各调试端口，返回第一个能看到 chat.z.ai 页面的连接。

    主窗口与登录窗口分属不同的 WebView2 环境（登录窗口有独立 data_directory），
    而调试端口是按环境绑的，所以这里要挨个试。
    """
    for cdp in cdps:
        try:
            browser = pw.chromium.connect_over_cdp(cdp)
        except Exception as e:
            log(f"{cdp} 连接失败：{str(e)[:80]}")
            continue
        page = find_page(browser)
        if page is not None:
            log(f"已挂载 {cdp}")
            try:
                page.bring_to_front()  # 输入事件要求窗口在前台
            except Exception:
                pass
            return browser, page
        try:
            browser.close()
        except Exception:
            pass
        log(f"{cdp} 上没有 chat.z.ai 页面")
    return None, None


def solve(cdps, max_attempts):
    t0 = time.time()
    with sync_playwright() as pw:
        browser, page = connect_any(pw, cdps)
        if page is None:
            return {
                "ok": False,
                "attempts": 0,
                "elapsed": round(time.time() - t0, 1),
                "error": "所有调试端口都没有 chat.z.ai 页面（注册窗口是否已打开？）",
            }
        if not ensure_open(page):
            return {
                "ok": False,
                "attempts": 0,
                "elapsed": round(time.time() - t0, 1),
                "error": "验证码弹窗未能展开",
            }
        attempts = 0
        # 定位失败轮数与拖动次数分开统计：否则「8 轮都没定位到缺口」会被报成
        # 「达到最大尝试次数」，排查时分不清是算法没找到，还是拖动被风控拒了。
        locate_fail = 0
        last_err = ""
        for _ in range(max_attempts):
            try:
                info = page.evaluate(READ_IMGS)
            except Exception as e:
                last_err = str(e)[:120]
                time.sleep(1.5)
                continue
            bx0 = info.get("box") or {}
            if not info.get("main") or not info.get("puzzle") or (bx0.get("width") or 0) <= 0:
                ensure_open(page)
                time.sleep(1.0)
                continue
            # 滑块必须落在视口内，否则鼠标事件落在窗口外（踩过：620 高的窗口里滑块 y=602，
            # 点击/拖动全部无效且没有任何报错）。
            vp = page.evaluate("() => ({w: innerWidth, h: innerHeight})")
            if (bx0.get("y") or 0) + (bx0.get("height") or 0) > vp.get("h", 0):
                return {
                    "ok": False,
                    "attempts": attempts,
                    "elapsed": round(time.time() - t0, 1),
                    "error": (
                        f"验证码滑块超出视口（slider y={int(bx0.get('y') or 0)}"
                        f"+h={int(bx0.get('height') or 0)} > vh={vp.get('h')}），请把窗口调大"
                    ),
                }
            try:
                main_img = load_img(info["main"]).convert("RGB")
                puz_img = load_img(info["puzzle"]).convert("RGBA")
            except Exception as e:
                last_err = f"取图失败 {str(e)[:80]}"
                click_refresh(page, (info.get("main"), info.get("puzzle")))
                continue
            # 图片实际显示宽度：拖动距离的换算依据（见 BASE_W 注释）。
            # 取不到就退回基准宽度，等于沿用旧行为。
            disp_w = float((info.get("imgBox") or {}).get("width") or 0) or BASE_W
            try:
                best, ts, meta = locate(main_img, puz_img)
            except Exception as e:
                # 单帧尺寸异常（碎片比主图宽、alpha 行数对不上等）只该废掉这一轮，
                # 不该让整次求解带着异常退出 —— 宿主那边只会看到一条无信息的失败。
                locate_fail += 1
                last_err = f"定位异常 {type(e).__name__}: {str(e)[:80]}"
                click_refresh(page, (info.get("main"), info.get("puzzle")))
                continue
            if best is None:
                locate_fail += 1
                click_refresh(page, (info.get("main"), info.get("puzzle")))
                continue
            # slider_for 给的是「按 300 显示」的距离，这里按真实显示宽度放大回去。
            drag_dist = ts * (disp_w / BASE_W)
            meta.update({"x": best, "slider": drag_dist, "disp_w": disp_w})
            if not passes_rule(meta):
                # 定位不可靠 → 换图重试，不浪费一次验证请求。
                # 必须把特征打出来：否则日志只剩一句「8 轮都没定位到缺口」，
                # 分不清是算法没找到、还是阈值把正确位置拒了。
                locate_fail += 1
                last_err = (
                    f"定位不可靠 pix={meta['pix']} dip={meta['dip']:.3f} "
                    f"rows={meta['rows']:.0f} pw={meta['pw']:.0f} mw={meta['mw']} disp={disp_w:.0f}"
                )
                log(
                    f"reject#{locate_fail} x={best} pix={meta['pix']} dip={meta['dip']:.4f} "
                    f"rows={meta['rows']:.1f} pw={meta['pw']:.1f} margin={meta['margin']:.4f} "
                    f"mw={meta['mw']} disp={disp_w:.0f}"
                )
                click_refresh(page, (info.get("main"), info.get("puzzle")))
                continue
            bx = info["box"]
            sx = bx["x"] + bx["width"] / 2
            sy = bx["y"] + bx["height"] / 2
            attempts += 1
            log(
                f"#{attempts} x={best} slider={drag_dist:.1f} pix={meta['pix']} "
                f"rr={meta['rank_ratio']} disp={disp_w:.0f}"
            )
            try:
                # 超时要短：正常情况下 verify 响应 1~3 秒就回来。
                # 之前给到 22 秒，拖动失败时每轮都要干等 22 秒，
                # 一轮下来能拖到几分钟，宿主那边看起来就是「卡住了没重试」。
                with page.expect_response(
                    lambda r: "no8xfe-verify" in r.url, timeout=8000
                ) as ri:
                    drag(page, sx, sy, drag_dist)
                res = json.loads(ri.value.text()).get("Result", {})
                if res.get("VerifyResult"):
                    return {
                        "ok": True,
                        "attempts": attempts,
                        "code": res.get("VerifyCode"),
                        "x": best,
                        "elapsed": round(time.time() - t0, 1),
                    }
                last_err = f"VerifyCode={res.get('VerifyCode')}"
            except Exception as e:
                last_err = str(e)[:120]
            click_refresh(page, (info.get("main"), info.get("puzzle")))
        if not last_err:
            last_err = (
                f"{locate_fail} 轮都没能定位到缺口（图可能没刷新）"
                if attempts == 0
                else "达到最大尝试次数"
            )
        return {
            "ok": False,
            "attempts": attempts,
            "locateFail": locate_fail,
            "elapsed": round(time.time() - t0, 1),
            "error": last_err,
        }


def main():
    raw = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:9222,http://127.0.0.1:9223"
    cdps = [u.strip() for u in raw.split(",") if u.strip()]
    # 单次调用的轮数不宜多：失败要尽快交回宿主去重试，
    # 而不是在这里闷头跑满几分钟。
    max_attempts = int(sys.argv[2]) if len(sys.argv) > 2 else 8
    try:
        result = solve(cdps, max_attempts)
    except Exception as e:
        result = {"ok": False, "attempts": 0, "error": f"{type(e).__name__}: {str(e)[:200]}"}
    print(json.dumps(result, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
