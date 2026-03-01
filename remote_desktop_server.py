#!/usr/bin/env python3
"""
远程桌面服务端
- HTTP + WebSocket 合并监听同一端口
- 自动托管客户端 HTML（内嵌或同目录文件）
- 启动后自动打开默认浏览器
依赖: pip install websockets mss pillow pynput numpy
"""

import asyncio
import websockets
import websockets.server
from websockets.http11 import Request, Response
from websockets.datastructures import Headers
import json
import base64
import io
import time
import logging
import threading
import webbrowser
import os
import sys
from http import HTTPStatus

logging.basicConfig(level=logging.INFO, format='[%(asctime)s] %(levelname)s %(message)s')
log = logging.getLogger("RemoteDesktop")

# ─── 屏幕捕获 ──────────────────────────────────────────────────────────────────
try:
    import mss
    MSS_AVAILABLE = True
except ImportError:
    MSS_AVAILABLE = False
    log.warning("mss not found, falling back to PIL ImageGrab")

try:
    from PIL import Image
    PIL_AVAILABLE = True
except ImportError:
    PIL_AVAILABLE = False

try:
    import numpy as np
    NUMPY_AVAILABLE = True
except ImportError:
    NUMPY_AVAILABLE = False
    log.warning("numpy not found, falling back to slow per-tile comparison")

# ─── 输入注入 ──────────────────────────────────────────────────────────────────
try:
    from pynput.mouse import Button, Controller as MouseController
    from pynput.keyboard import Key, Controller as KeyboardController
    mouse_ctrl = MouseController()
    keyboard_ctrl = KeyboardController()
    PYNPUT_AVAILABLE = True
except ImportError:
    PYNPUT_AVAILABLE = False
    log.warning("pynput not found, input injection disabled")


# ═══════════════════════════════════════════════════════════════════════════════
# 客户端 HTML 加载
# 优先读同目录的 remote_desktop_client.html，否则用内嵌占位页
# ═══════════════════════════════════════════════════════════════════════════════
def _load_client_html(ws_port: int) -> bytes:
    """加载客户端 HTML，并将 WebSocket 地址注入为默认值"""
    # PyInstaller 打包后资源在 sys._MEIPASS，普通运行在脚本同目录
    base_dir = getattr(sys, '_MEIPASS', os.path.dirname(os.path.abspath(__file__)))
    html_path = os.path.join(base_dir, 'remote_desktop_client.html')

    if os.path.exists(html_path):
        with open(html_path, 'r', encoding='utf-8') as f:
            html = f.read()
        # 把默认端口注入到 HTML 中
        html = html.replace(
            'value="8765"',
            f'value="{ws_port}"'
        )
        log.info(f"Serving client HTML from: {html_path}")
        return html.encode('utf-8')
    else:
        # 找不到文件时返回提示页
        log.warning(f"Client HTML not found at {html_path}, serving fallback page")
        return (
            f'<html><body style="font:16px sans-serif;padding:40px">'
            f'<h2>Remote Desktop</h2>'
            f'<p>找不到 <code>remote_desktop_client.html</code></p>'
            f'<p>请将其放在与服务端同一目录下</p>'
            f'</body></html>'
        ).encode('utf-8')


# ═══════════════════════════════════════════════════════════════════════════════
# 屏幕捕获器（线程安全：每线程独立 mss 实例）
# ═══════════════════════════════════════════════════════════════════════════════
class ScreenCapture:
    def __init__(self):
        self._local = threading.local()
        self._monitor_info = None
        self._detect_size()

    def _detect_size(self):
        if MSS_AVAILABLE:
            with mss.mss() as sct:
                m = sct.monitors[1]
                self._monitor_info = dict(m)
                log.info(f"Screen size: {m['width']}x{m['height']}")
        elif PIL_AVAILABLE:
            from PIL import ImageGrab
            img = ImageGrab.grab()
            self._monitor_info = {'left': 0, 'top': 0,
                                  'width': img.width, 'height': img.height}

    def _get_sct(self):
        if not getattr(self._local, 'sct', None):
            self._local.sct = mss.mss()
        return self._local.sct

    @property
    def width(self):
        return self._monitor_info['width'] if self._monitor_info else 1920

    @property
    def height(self):
        return self._monitor_info['height'] if self._monitor_info else 1080

    def grab(self) -> Image.Image:
        if MSS_AVAILABLE:
            sct = self._get_sct()
            raw = sct.grab(self._monitor_info)
            return Image.frombytes('RGB', (raw.width, raw.height), raw.rgb)
        elif PIL_AVAILABLE:
            from PIL import ImageGrab
            return ImageGrab.grab().convert('RGB')
        else:
            raise RuntimeError("No screen capture backend available")


# ═══════════════════════════════════════════════════════════════════════════════
# 差异计算器 - numpy 整帧差分
# ═══════════════════════════════════════════════════════════════════════════════
class DirtyRectDetector:
    def __init__(self, tile_size=64, threshold=3):
        self.tile_size = tile_size
        self.threshold = threshold
        self.prev_arr = None

    def get_dirty_tiles(self, frame: Image.Image):
        w, h = frame.size
        ts = self.tile_size
        if NUMPY_AVAILABLE:
            return self._numpy_diff(frame, w, h, ts)
        else:
            return self._fallback_diff(frame, w, h, ts)

    def _numpy_diff(self, frame, w, h, ts):
        curr_arr = np.asarray(frame, dtype=np.uint8)
        if self.prev_arr is None or self.prev_arr.shape != curr_arr.shape:
            self.prev_arr = curr_arr.copy()
            return self._all_tiles(frame, w, h, ts)

        diff = np.abs(curr_arr.astype(np.int16) - self.prev_arr.astype(np.int16))
        changed_px = np.any(diff > self.threshold, axis=2)

        cols = (w + ts - 1) // ts
        rows = (h + ts - 1) // ts
        ph, pw = rows * ts, cols * ts
        if ph != h or pw != w:
            padded = np.zeros((ph, pw), dtype=bool)
            padded[:h, :w] = changed_px
        else:
            padded = changed_px

        tile_changed = padded.reshape(rows, ts, cols, ts).any(axis=(1, 3))

        dirty = []
        for (row, col) in np.argwhere(tile_changed):
            x, y = int(col) * ts, int(row) * ts
            x2, y2 = min(x + ts, w), min(y + ts, h)
            dirty.append((x, y, x2 - x, y2 - y, frame.crop((x, y, x2, y2))))

        self.prev_arr = curr_arr.copy()
        return dirty

    def _fallback_diff(self, frame, w, h, ts):
        if not hasattr(self, '_sums'):
            self._sums = {}
        cols = (w + ts - 1) // ts
        rows = (h + ts - 1) // ts
        dirty = []
        for row in range(rows):
            for col in range(cols):
                x, y = col * ts, row * ts
                x2, y2 = min(x + ts, w), min(y + ts, h)
                tile = frame.crop((x, y, x2, y2))
                s = sum(tile.getdata()[0])
                key = (col, row)
                if self._sums.get(key) != s:
                    self._sums[key] = s
                    dirty.append((x, y, x2 - x, y2 - y, tile))
        return dirty

    def _all_tiles(self, frame, w, h, ts):
        cols = (w + ts - 1) // ts
        rows = (h + ts - 1) // ts
        dirty = []
        for row in range(rows):
            for col in range(cols):
                x, y = col * ts, row * ts
                x2, y2 = min(x + ts, w), min(y + ts, h)
                dirty.append((int(x), int(y), int(x2 - x), int(y2 - y),
                               frame.crop((x, y, x2, y2))))
        return dirty

    def reset(self):
        self.prev_arr = None


def tile_to_jpeg_b64(tile_img: Image.Image, quality=75) -> str:
    buf = io.BytesIO()
    tile_img.save(buf, format='JPEG', quality=quality, subsampling=0)
    return base64.b64encode(buf.getvalue()).decode('ascii')


# ═══════════════════════════════════════════════════════════════════════════════
# 输入处理
# ═══════════════════════════════════════════════════════════════════════════════
KEY_MAP = {
    'Backspace': 'backspace', 'Tab': 'tab', 'Enter': 'enter',
    'Escape': 'esc', 'Delete': 'delete', 'Insert': 'insert',
    'Home': 'home', 'End': 'end', 'PageUp': 'page_up', 'PageDown': 'page_down',
    'ArrowLeft': 'left', 'ArrowRight': 'right', 'ArrowUp': 'up', 'ArrowDown': 'down',
    'F1': 'f1', 'F2': 'f2', 'F3': 'f3', 'F4': 'f4', 'F5': 'f5',
    'F6': 'f6', 'F7': 'f7', 'F8': 'f8', 'F9': 'f9', 'F10': 'f10',
    'F11': 'f11', 'F12': 'f12',
    'Control': 'ctrl', 'Alt': 'alt', 'Shift': 'shift', 'Meta': 'cmd',
    'CapsLock': 'caps_lock', 'Space': 'space',
}
if PYNPUT_AVAILABLE:
    BUTTON_MAP = {0: Button.left, 1: Button.middle, 2: Button.right}


def handle_mouse_event(data: dict):
    if not PYNPUT_AVAILABLE:
        return
    etype = data.get('type')
    x, y = int(data.get('x', 0)), int(data.get('y', 0))
    btn = BUTTON_MAP.get(int(data.get('button', 0)), Button.left)
    try:
        if etype == 'mousemove':
            mouse_ctrl.position = (x, y)
        elif etype == 'mousedown':
            mouse_ctrl.position = (x, y); mouse_ctrl.press(btn)
        elif etype == 'mouseup':
            mouse_ctrl.position = (x, y); mouse_ctrl.release(btn)
        elif etype == 'dblclick':
            mouse_ctrl.position = (x, y); mouse_ctrl.click(btn, count=2)
        elif etype == 'wheel':
            dx, dy = int(data.get('deltaX', 0)), int(data.get('deltaY', 0))
            if dy: mouse_ctrl.scroll(0, -dy / 100)
            if dx: mouse_ctrl.scroll(dx / 100, 0)
    except Exception as e:
        log.debug(f"Mouse event error: {e}")


def handle_keyboard_event(data: dict):
    if not PYNPUT_AVAILABLE:
        return
    etype, key_str = data.get('type'), data.get('key', '')
    try:
        if key_str in KEY_MAP:
            pynput_key = getattr(Key, KEY_MAP[key_str], None)
            if pynput_key:
                if etype == 'keydown': keyboard_ctrl.press(pynput_key)
                elif etype == 'keyup': keyboard_ctrl.release(pynput_key)
        elif len(key_str) == 1:
            if etype == 'keydown': keyboard_ctrl.press(key_str)
            elif etype == 'keyup': keyboard_ctrl.release(key_str)
    except Exception as e:
        log.debug(f"Keyboard event error: {e}")


# ═══════════════════════════════════════════════════════════════════════════════
# 主服务器：HTTP + WebSocket 合并在同一端口
# websockets 库支持 process_request 钩子，可在同一 TCP 端口上
# 对普通 HTTP GET 返回 HTML，对 WS 升级请求正常走 WebSocket
# ═══════════════════════════════════════════════════════════════════════════════
class RemoteDesktopServer:
    def __init__(self, host='0.0.0.0', port=8765,
                 fps=15, tile_size=64, quality=75, no_browser=False):
        self.host = host
        self.port = port
        self.fps = fps
        self.tile_size = tile_size
        self.quality = quality
        self.no_browser = no_browser
        self.capture = ScreenCapture()
        # 预加载 HTML（注入端口号）
        self._html_bytes = _load_client_html(port)

    # ── HTTP 钩子：拦截非 WebSocket 请求，返回客户端 HTML ──────────────────────
    async def _process_request(self, connection, request):
        """
        websockets >= 12 的 process_request 签名是 (connection, request)。
        返回 None 表示继续走 WebSocket 握手；
        返回 Response 对象表示直接回 HTTP 响应。
        """
        # WebSocket 升级请求：交给 websockets 处理
        if request.headers.get('upgrade', '').lower() == 'websocket':
            return None

        # 普通 HTTP GET：返回客户端 HTML
        headers = Headers([
            ('Content-Type', 'text/html; charset=utf-8'),
            ('Content-Length', str(len(self._html_bytes))),
            ('Cache-Control', 'no-cache'),
        ])
        return Response(HTTPStatus.OK.value, HTTPStatus.OK.phrase, headers, self._html_bytes)

    # ── WebSocket 会话处理 ─────────────────────────────────────────────────────
    async def _ws_handler(self, websocket):
        client_addr = websocket.remote_address
        log.info(f"WS client connected: {client_addr}")
        detector = DirtyRectDetector(tile_size=self.tile_size, threshold=3)

        await websocket.send(json.dumps({
            'type': 'init',
            'width': self.capture.width,
            'height': self.capture.height,
            'tile_size': self.tile_size,
        }))

        async def recv_loop():
            try:
                async for message in websocket:
                    try:
                        data = json.loads(message)
                        etype = data.get('type', '')
                        if etype == 'ping':
                            # 立即响应 pong
                            await websocket.send(json.dumps({'type': 'pong'}))
                        elif etype.startswith('mouse') or etype in ('dblclick', 'wheel'):
                            handle_mouse_event(data)
                        elif etype.startswith('key'):
                            handle_keyboard_event(data)
                        elif etype == 'request_full':
                            detector.reset()
                    except Exception as e:
                        log.debug(f"Input error: {e}")
            except websockets.exceptions.ConnectionClosed:
                pass

        def grab_and_diff():
            return detector.get_dirty_tiles(self.capture.grab())

        async def send_loop():
            interval = 1.0 / self.fps
            loop = asyncio.get_event_loop()
            try:
                while True:
                    t0 = time.monotonic()
                    try:
                        dirty = await loop.run_in_executor(None, grab_and_diff)
                        if dirty:
                            tiles_data = [
                                {'x': x, 'y': y, 'w': w, 'h': h,
                                 'data': tile_to_jpeg_b64(img, self.quality),
                                 'fmt': 'jpeg'}
                                for (x, y, w, h, img) in dirty
                            ]
                            await websocket.send(json.dumps({
                                'type': 'frame',
                                'tiles': tiles_data,
                            }))
                    except websockets.exceptions.ConnectionClosed:
                        break
                    except Exception as e:
                        log.error(f"Frame send error: {e}")
                        break
                    await asyncio.sleep(max(0, interval - (time.monotonic() - t0)))
            except websockets.exceptions.ConnectionClosed:
                pass

        try:
            await asyncio.gather(recv_loop(), send_loop())
        finally:
            log.info(f"WS client disconnected: {client_addr}")

    # ── 启动 ──────────────────────────────────────────────────────────────────
    async def start(self):
        log.info(f"FPS={self.fps}, TileSize={self.tile_size}, Quality={self.quality}")
        log.info(f"pynput: {'OK' if PYNPUT_AVAILABLE else 'NOT AVAILABLE (read-only mode)'}")
        log.info(f"mss:    {'OK' if MSS_AVAILABLE else 'NOT AVAILABLE'}")
        log.info(f"numpy:  {'OK' if NUMPY_AVAILABLE else 'NOT AVAILABLE'}")

        # WebSocket 服务（带 HTTP 钩子，同端口托管 HTML）
        ws_server = await websockets.serve(
            self._ws_handler,
            self.host,
            self.port,
            process_request=self._process_request,
            max_size=50 * 1024 * 1024,
        )

        url = f"http://localhost:{self.port}/"
        log.info(f"✅ Remote Desktop running!")
        log.info(f"   Web UI  : {url}")
        log.info(f"   WS      : ws://localhost:{self.port}/")

        await asyncio.Future()  # run forever


# ═══════════════════════════════════════════════════════════════════════════════
# 启动入口
# ═══════════════════════════════════════════════════════════════════════════════
if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description='Remote Desktop Server')
    parser.add_argument('--host',      default='0.0.0.0', help='Bind address (default: 0.0.0.0)')
    parser.add_argument('--port',      type=int, default=8765, help='HTTP+WS port (default: 8765)')
    parser.add_argument('--fps',       type=int, default=15,   help='Max FPS (default: 15)')
    parser.add_argument('--tile-size', type=int, default=64,   help='Tile size px (default: 64)')
    parser.add_argument('--quality',   type=int, default=75,   help='JPEG quality 1-95 (default: 75)')
    args = parser.parse_args()

    server = RemoteDesktopServer(
        host=args.host,
        port=args.port,
        fps=args.fps,
        tile_size=args.tile_size,
        quality=args.quality,
    )
    asyncio.run(server.start())
