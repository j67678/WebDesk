//! 屏幕捕获、瓦片差分与 JPEG 编码。
//!
//! Windows 使用 GDI BitBlt + GetDIBits 抓取虚拟桌面（支持多显示器），
//! 输出自上而下的 32 位 BGRA；差分按瓦片做容差哈希，只编码变化的区域。

use std::io::{self, Cursor};

/// 一帧原始画面：BGRA、top-down。
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// 已转换为 RGB8，可直接送 JPEG 编码器
    pub rgb: Vec<u8>,
}

pub use imp::{virtual_screen_origin, ScreenCapturer};

/// 每个 WebSocket 连接独立维护一份瓦片哈希，避免跨会话串扰。
pub struct DiffDetector {
    tile_size: u32,
    prev: Vec<u64>,
    cols: usize,
    rows: usize,
    initialized: bool,
}

impl DiffDetector {
    pub fn new(tile_size: u32) -> Self {
        Self {
            tile_size: tile_size.clamp(16, 256),
            prev: Vec::new(),
            cols: 0,
            rows: 0,
            initialized: false,
        }
    }

    pub fn reset(&mut self) {
        self.initialized = false;
        self.prev.clear();
    }

    /// 返回本帧发生变化的瓦片（首帧或尺寸变化时返回全部瓦片）。
    pub fn diff(&mut self, frame: &Frame) -> Vec<Tile> {
        let ts = self.tile_size;
        let cols = (frame.width as usize + ts as usize - 1) / ts as usize;
        let rows = (frame.height as usize + ts as usize - 1) / ts as usize;
        let full = !self.initialized || self.cols != cols || self.rows != rows;
        self.cols = cols;
        self.rows = rows;
        self.initialized = true;
        if self.prev.len() != cols * rows {
            self.prev.resize(cols * rows, 0);
        }

        let mut out = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                let idx = row * cols + col;
                let x = (col as u32) * ts;
                let y = (row as u32) * ts;
                let w = ts.min(frame.width - x);
                let h = ts.min(frame.height - y);
                let hash = tile_hash(frame, x, y, w, h);
                let changed = full || hash != self.prev[idx];
                self.prev[idx] = hash;
                if changed {
                    out.push(extract_rgb(frame, x, y, w, h));
                }
            }
        }
        out
    }
}

/// 容差哈希：每通道丢弃低 2 位（等价于约 3 级色差阈值），抑制压缩/光标噪声。
fn tile_hash(frame: &Frame, x: u32, y: u32, w: u32, h: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let stride = frame.width as usize * 4;
    let mut ry = y as usize;
    while ry < (y + h) as usize {
        let base = ry * stride + x as usize * 4;
        let mut cx = 0usize;
        while cx < w as usize {
            let i = base + cx * 4;
            if i + 2 < frame.data.len() {
                for k in 0..3 {
                    let v = (frame.data[i + k] >> 2) as u64;
                    hash = (hash ^ v).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            cx += 2; // 横向隔点采样
        }
        ry += 2; // 纵向隔行采样
    }
    hash
}

fn extract_rgb(frame: &Frame, x: u32, y: u32, w: u32, h: u32) -> Tile {
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    let stride = frame.width as usize * 4;
    for row in y..(y + h) {
        let base = row as usize * stride + x as usize * 4;
        for col in 0..w as usize {
            let i = base + col * 4;
            // GetDIBits 输出 BGRA
            rgb.push(frame.data[i + 2]);
            rgb.push(frame.data[i + 1]);
            rgb.push(frame.data[i]);
        }
    }
    Tile { x, y, w, h, rgb }
}

pub fn encode_jpeg(tile: &Tile, quality: u8) -> io::Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::with_capacity(tile.rgb.len() / 4));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(
            &tile.rgb,
            tile.w,
            tile.h,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    Ok(out.into_inner())
}

// ──────────────────────────────────────────────────────────────────────────────
// Windows GDI 实现
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use super::Frame;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// 虚拟桌面的左上角坐标；输入注入时把客户端坐标平移到这里。
    pub fn virtual_screen_origin() -> (i32, i32) {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            )
        }
    }

    pub fn virtual_screen_size() -> (u32, u32) {
        unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
            )
        }
    }

    pub struct ScreenCapturer {
        buffer: Vec<u8>,
        width: u32,
        height: u32,
    }

    impl ScreenCapturer {
        pub fn new() -> io::Result<Self> {
            let (w, h) = virtual_screen_size();
            Ok(Self {
                buffer: vec![0u8; w as usize * h as usize * 4],
                width: w,
                height: h,
            })
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn capture(&mut self) -> io::Result<Frame> {
            unsafe {
                let (ox, oy) = virtual_screen_origin();
                let (w, h) = virtual_screen_size();
                if (w, h) != (self.width, self.height) {
                    self.width = w;
                    self.height = h;
                    self.buffer.resize(w as usize * h as usize * 4, 0);
                }

                let hwnd: HWND = null_mut();
                let screen_dc = GetDC(hwnd);
                if screen_dc.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let mem_dc = CreateCompatibleDC(screen_dc);
                let bitmap = CreateCompatibleBitmap(screen_dc, w as i32, h as i32);
                if mem_dc.is_null() || bitmap.is_null() {
                    if !bitmap.is_null() {
                        DeleteObject(bitmap);
                    }
                    if !mem_dc.is_null() {
                        DeleteDC(mem_dc);
                    }
                    ReleaseDC(hwnd, screen_dc);
                    return Err(io::Error::new(io::ErrorKind::Other, "无法创建兼容 DC / Bitmap"));
                }

                let old = SelectObject(mem_dc, bitmap);
                let ok = BitBlt(
                    mem_dc,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    screen_dc,
                    ox,
                    oy,
                    SRCCOPY | CAPTUREBLT,
                );
                if ok == 0 {
                    cleanup(screen_dc, mem_dc, bitmap, old);
                    return Err(io::Error::last_os_error());
                }

                let mut bmi: BITMAPINFO = zeroed();
                bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = w as i32;
                bmi.bmiHeader.biHeight = -(h as i32); // 负数 = 自上而下
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = 0; // BI_RGB
                let lines = GetDIBits(
                    mem_dc,
                    bitmap,
                    0,
                    h,
                    self.buffer.as_mut_ptr().cast(),
                    &mut bmi,
                    DIB_RGB_COLORS,
                );
                cleanup(screen_dc, mem_dc, bitmap, old);
                if lines == 0 {
                    return Err(io::Error::new(io::ErrorKind::Other, "GetDIBits 失败"));
                }

                Ok(Frame {
                    width: w,
                    height: h,
                    data: self.buffer.clone(),
                })
            }
        }
    }

    unsafe fn cleanup(
        screen_dc: windows_sys::Win32::Graphics::Gdi::HDC,
        mem_dc: windows_sys::Win32::Graphics::Gdi::HDC,
        bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
        old: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
    ) {
        if !old.is_null() {
            SelectObject(mem_dc, old);
        }
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(null_mut(), screen_dc);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 非 Windows：保持可编译，服务启动时给出明确错误
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(not(windows))]
mod imp {
    use super::Frame;
    use std::io;

    pub fn virtual_screen_origin() -> (i32, i32) {
        (0, 0)
    }

    pub struct ScreenCapturer;

    impl ScreenCapturer {
        pub fn new() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "当前版本仅在 Windows 提供屏幕捕获；Linux/macOS 请关注后续版本",
            ))
        }
        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
        #[allow(unused_variables)]
        pub fn capture(&mut self) -> io::Result<Frame> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "当前平台暂不支持屏幕捕获",
            ))
        }
    }
}
