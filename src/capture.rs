//! 屏幕捕获、瓦片差分与 JPEG 编码。
//!
//! Windows 使用 CreateDIBSection + BitBlt 把虚拟桌面直接画进 BGRA 缓冲（支持多显示器），
//! 捕获资源与像素缓冲跨帧复用，不再每帧重建 GDI 对象或复制整屏；
//! 差分按瓦片做容差哈希，相邻脏瓦片合并成矩形后再编码，减少 JPEG 头开销。

use std::io::{self, Cursor};

/// 一帧原始画面：BGRA、top-down。
///
/// 像素数据由 `ScreenCapturer` 持有，帧本身只借用，避免每帧复制整屏缓冲。
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

/// 合并后的脏矩形（像素坐标），一次编码 / 一次下发的单位。
#[derive(Clone, Copy, Debug)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub use imp::{virtual_screen_origin, virtual_screen_size, ScreenCapturer};

/// 捕获线程产出的共享帧：像素 + 全局瓦片哈希。
///
/// 同一帧可被多个客户端并发读取：捕获与哈希全服务只做一次，
/// 各客户端只按自己的上一帧哈希做差集，提取需要的瓦片。
pub struct SharedFrame {
    pub width: u32,
    pub height: u32,
    pub cols: usize,
    pub rows: usize,
    pub hashes: Vec<u64>,
    pub data: Vec<u8>,
}

/// 瓦片网格行列数（与 `DiffDetector` 使用同一钳制范围）。
pub fn tile_grid(width: u32, height: u32, tile_size: u32) -> (usize, usize) {
    let ts = tile_size.clamp(16, 256) as usize;
    (
        (width as usize + ts - 1) / ts,
        (height as usize + ts - 1) / ts,
    )
}

/// 全帧瓦片哈希（行优先），由捕获线程每帧算一次供所有客户端复用。
pub fn compute_hashes(frame: &Frame<'_>, tile_size: u32, cols: usize, rows: usize) -> Vec<u64> {
    let ts = tile_size.clamp(16, 256);
    let mut hashes = vec![0u64; cols * rows];
    for row in 0..rows {
        for col in 0..cols {
            let x = (col as u32) * ts;
            let y = (row as u32) * ts;
            let w = ts.min(frame.width - x);
            let h = ts.min(frame.height - y);
            hashes[row * cols + col] = tile_hash(frame, x, y, w, h);
        }
    }
    hashes
}

/// 每个 WebSocket 连接独立维护一份瓦片哈希，避免跨会话串扰。
pub struct DiffDetector {
    prev: Vec<u64>,
    cols: usize,
    rows: usize,
    initialized: bool,
    /// 本帧变化瓦片的下标（按扫描顺序）
    changed: Vec<usize>,
}

impl DiffDetector {
    pub fn new() -> Self {
        Self {
            prev: Vec::new(),
            cols: 0,
            rows: 0,
            initialized: false,
            changed: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.initialized = false;
        self.prev.clear();
    }

    /// 用捕获线程算好的全局哈希与本会话上一帧做差集，
    /// 变化瓦片的下标通过 `changed()` 读取；首帧或尺寸变化时全部视为变化。
    pub fn diff(&mut self, hashes: &[u64], cols: usize, rows: usize) {
        let full = !self.initialized || self.cols != cols || self.rows != rows;
        self.cols = cols;
        self.rows = rows;
        self.initialized = true;

        let slots = cols * rows;
        debug_assert_eq!(hashes.len(), slots);
        if self.prev.len() != slots {
            self.prev.resize(slots, 0);
        }
        self.changed.clear();

        if full {
            self.changed.extend(0..slots);
            self.prev.copy_from_slice(hashes);
            return;
        }
        for (idx, &hash) in hashes.iter().enumerate() {
            if hash != self.prev[idx] {
                self.prev[idx] = hash;
                self.changed.push(idx);
            }
        }
    }

    /// 本帧变化瓦片的下标。
    pub fn changed(&self) -> &[usize] {
        &self.changed
    }
}

/// 把相邻变化瓦片合并成矩形，减少 JPEG 编码次数与每块 JPEG 的头开销。
///
/// 先按行找连续脏瓦片，再向上/下扩展整段都脏的行；合并结果写入 `out`
/// （由调用方复用，避免每帧分配）。所有矩形互不重叠。
pub fn merge_dirty_rects(
    changed: &[usize],
    cols: usize,
    rows: usize,
    width: u32,
    height: u32,
    tile_size: u32,
    out: &mut Vec<DirtyRect>,
) {
    let ts = tile_size.clamp(16, 256);
    let mut dirty = vec![false; cols * rows];
    for &idx in changed {
        if idx < dirty.len() {
            dirty[idx] = true;
        }
    }

    for row in 0..rows {
        let mut col = 0;
        while col < cols {
            if !dirty[row * cols + col] {
                col += 1;
                continue;
            }
            let c0 = col;
            while col < cols && dirty[row * cols + col] {
                col += 1;
            }
            let c1 = col; // 不含

            // 向下扩展：整段仍然全脏才并入同一矩形
            let mut r_end = row + 1;
            while r_end < rows && (c0..c1).all(|c| dirty[r_end * cols + c]) {
                r_end += 1;
            }
            for r in row..r_end {
                for c in c0..c1 {
                    dirty[r * cols + c] = false;
                }
            }

            let x = c0 as u32 * ts;
            let y = row as u32 * ts;
            let x_end = ((c1 as u32) * ts).min(width);
            let y_end = ((r_end as u32) * ts).min(height);
            out.push(DirtyRect {
                x,
                y,
                w: x_end.saturating_sub(x),
                h: y_end.saturating_sub(y),
            });
        }
    }
}

/// 容差哈希：每通道丢弃低 2 位（等价于约 3 级色差阈值），抑制压缩/光标噪声。
fn tile_hash(frame: &Frame<'_>, x: u32, y: u32, w: u32, h: u32) -> u64 {
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

/// 把任意矩形的 BGRA 像素转换进已复用的 RGB 缓冲，避免逐帧分配。
pub fn extract_rgb(frame: &Frame<'_>, x: u32, y: u32, w: u32, h: u32, out: &mut Vec<u8>) {
    let w = w as usize;
    let h = h as usize;
    let stride = frame.width as usize * 4;
    let row_bytes = w * 3;
    out.resize(row_bytes * h, 0);

    for row in 0..h {
        let src_start = (y as usize + row) * stride + x as usize * 4;
        // DIB 为 BGRA
        let src = &frame.data[src_start..src_start + w * 4];
        let dst = &mut out[row * row_bytes..(row + 1) * row_bytes];
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(3)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
        }
    }
}

pub fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8) -> io::Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::with_capacity(rgb.len() / 4));
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    Ok(out.into_inner())
}

// ──────────────────────────────────────────────────────────────────────────────
// Windows GDI 实现
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use super::Frame;
    use std::ffi::c_void;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::ptr::null_mut;

    use windows::core::{Error as WinError, Interface};
    use windows::Win32::Foundation::{HMODULE, RECT};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
        DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC,
        HGDIOBJ, SRCCOPY,
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

    fn win_err(e: WinError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, e.to_string())
    }

    /// 捕获后端选择：优先 DXGI Desktop Duplication，条件不满足时回退 GDI BitBlt。
    pub struct ScreenCapturer {
        backend: Backend,
        width: u32,
        height: u32,
    }

    enum Backend {
        Dxgi(DxgiCapturer),
        Gdi(GdiCapturer),
    }

    impl ScreenCapturer {
        pub fn new() -> io::Result<Self> {
            let (width, height) = virtual_screen_size();
            let backend = match DxgiCapturer::new() {
                Ok(d) => {
                    crate::log_info!("屏幕捕获后端: DXGI Desktop Duplication（不含远程光标）");
                    Backend::Dxgi(d)
                }
                Err(e) => {
                    crate::log_warn!("DXGI 不可用，回退 GDI BitBlt: {e}");
                    Backend::Gdi(GdiCapturer::new()?)
                }
            };
            Ok(Self {
                backend,
                width,
                height,
            })
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        /// 返回 `None` 表示本次没有任何显示器更新（DXGI 空闲），调用方可跳过哈希与下发。
        pub fn capture(&mut self) -> io::Result<Option<Frame<'_>>> {
            let (w, h) = virtual_screen_size();
            if (w, h) != (self.width, self.height) {
                crate::log_debug!(
                    "桌面尺寸变化 {}x{} -> {}x{}，重建捕获后端",
                    self.width,
                    self.height,
                    w,
                    h
                );
                self.width = w;
                self.height = h;
                self.rebuild()?;
                return Ok(None);
            }

            let mut updated = false;
            let mut need_rebuild = false;
            match &mut self.backend {
                Backend::Dxgi(dxgi) => match dxgi.capture() {
                    Ok(v) => updated = v,
                    Err(e) => {
                        crate::log_warn!("DXGI 捕获异常，重建捕获后端: {e}");
                        need_rebuild = true;
                    }
                },
                Backend::Gdi(gdi) => {
                    gdi.capture()?;
                    updated = true;
                }
            }
            if need_rebuild {
                self.rebuild()?;
                return Ok(None);
            }
            if !updated {
                return Ok(None);
            }

            let data = match &self.backend {
                Backend::Dxgi(d) => &d.buffer[..],
                Backend::Gdi(g) => g.data(),
            };
            Ok(Some(Frame {
                width: self.width,
                height: self.height,
                data,
            }))
        }

        fn rebuild(&mut self) -> io::Result<()> {
            // 先构造 GDI 兜底并释放旧 backend：同一输出在旧 duplication 存活时
            // 无法再次 DuplicateOutput，必须先 Drop。
            let fallback = GdiCapturer::new()?;
            let old = std::mem::replace(&mut self.backend, Backend::Gdi(fallback));
            drop(old);

            match DxgiCapturer::new() {
                Ok(d) => self.backend = Backend::Dxgi(d),
                Err(e) => crate::log_warn!("DXGI 重建失败，继续使用 GDI: {e}"),
            }
            Ok(())
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // DXGI Desktop Duplication
    // ──────────────────────────────────────────────────────────────────────────

    struct OutputDup {
        /// 在虚拟桌面缓冲中的偏移
        dx: u32,
        dy: u32,
        width: u32,
        height: u32,
        duplication: IDXGIOutputDuplication,
        staging: Option<ID3D11Texture2D>,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    }

    impl OutputDup {
        /// 把当前帧拷贝进虚拟桌面缓冲；返回本次是否有新内容。
        fn copy_frame(&mut self, buffer: &mut [u8], screen_width: u32) -> io::Result<bool> {
            let mut info: DXGI_OUTDUPL_FRAME_INFO = unsafe { zeroed() };
            let mut resource: Option<IDXGIResource> = None;
            match unsafe { self.duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) => return Err(win_err(e)),
            }

            let result = (|| -> io::Result<bool> {
                let Some(resource) = resource else {
                    return Ok(false);
                };
                let texture: ID3D11Texture2D = resource.cast().map_err(win_err)?;

                if self.staging.is_none() {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: self.width,
                        Height: self.height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging: Option<ID3D11Texture2D> = None;
                    unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                        .map_err(win_err)?;
                    self.staging = staging;
                }
                let staging = self
                    .staging
                    .as_ref()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "创建暂存纹理失败"))?;

                let dst: ID3D11Resource = staging.cast().map_err(win_err)?;
                let src: ID3D11Resource = texture.cast().map_err(win_err)?;
                unsafe { self.context.CopyResource(&dst, &src) };

                let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { zeroed() };
                unsafe {
                    self.context
                        .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                }
                .map_err(win_err)?;
                copy_rows(
                    buffer,
                    screen_width,
                    self.dx,
                    self.dy,
                    self.width,
                    self.height,
                    mapped.pData as *const u8,
                    mapped.RowPitch as usize,
                );
                unsafe { self.context.Unmap(&dst, 0) };
                Ok(true)
            })();

            // 无论拷贝是否成功都必须配对释放帧
            let _ = unsafe { self.duplication.ReleaseFrame() };
            result
        }
    }

    struct DxgiCapturer {
        outputs: Vec<OutputDup>,
        buffer: Vec<u8>,
        width: u32,
    }

    impl DxgiCapturer {
        fn new() -> io::Result<Self> {
            let (origin_x, origin_y) = virtual_screen_origin();
            let (width, height) = virtual_screen_size();
            if width == 0 || height == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "无效的桌面尺寸"));
            }

            let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(win_err)?;

            // 先统计桌面上所有已连接输出，用于确认 DXGI 覆盖完整
            let mut attached = 0u32;
            let mut adapter_index = 0u32;
            loop {
                let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
                    Ok(a) => a,
                    Err(_) => break,
                };
                adapter_index += 1;
                let mut output_index = 0u32;
                loop {
                    let output = match unsafe { adapter.EnumOutputs(output_index) } {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    output_index += 1;
                    let desc = unsafe { output.GetDesc() }.map_err(win_err)?;
                    if desc.AttachedToDesktop.as_bool() {
                        attached += 1;
                    }
                }
            }
            if attached == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "没有可捕获的显示器"));
            }

            let mut outputs = Vec::new();
            let mut adapter_index = 0u32;
            loop {
                let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
                    Ok(a) => a,
                    Err(_) => break,
                };
                adapter_index += 1;

                // DuplicateOutput 要求设备与输出属于同一适配器
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                let mut level = D3D_FEATURE_LEVEL_11_0;
                let created = unsafe {
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE(null_mut()),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        Some(&[
                            D3D_FEATURE_LEVEL_11_0,
                            D3D_FEATURE_LEVEL_10_1,
                            D3D_FEATURE_LEVEL_10_0,
                        ]),
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        Some(&mut level),
                        Some(&mut context),
                    )
                };
                if created.is_err() {
                    continue;
                }
                let (Some(device), Some(context)) = (device, context) else {
                    continue;
                };

                let mut output_index = 0u32;
                loop {
                    let output = match unsafe { adapter.EnumOutputs(output_index) } {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    output_index += 1;

                    let desc = unsafe { output.GetDesc() }.map_err(win_err)?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "显示器存在旋转，DXGI 暂不支持",
                        ));
                    }

                    let output1: IDXGIOutput1 = output.cast().map_err(win_err)?;
                    let duplication = unsafe { output1.DuplicateOutput(&device) }.map_err(win_err)?;
                    let dup_desc = unsafe { duplication.GetDesc() };
                    if dup_desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "显示器像素格式不是 BGRA8（可能为 HDR）",
                        ));
                    }

                    let rect: RECT = desc.DesktopCoordinates;
                    let left = rect.left.max(origin_x);
                    let top = rect.top.max(origin_y);
                    let right = rect.right.min(origin_x + width as i32);
                    let bottom = rect.bottom.min(origin_y + height as i32);
                    if right <= left || bottom <= top {
                        continue;
                    }

                    outputs.push(OutputDup {
                        dx: (left - origin_x) as u32,
                        dy: (top - origin_y) as u32,
                        width: (right - left) as u32,
                        height: (bottom - top) as u32,
                        duplication,
                        staging: None,
                        device: device.clone(),
                        context: context.clone(),
                    });
                }
            }

            if outputs.is_empty() {
                return Err(io::Error::new(io::ErrorKind::Other, "没有可用的 DXGI 输出"));
            }
            if outputs.len() as u32 != attached {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "显示器分布在多个显卡上，DXGI 覆盖不完整",
                ));
            }

            let mut capturer = Self {
                outputs,
                buffer: vec![0u8; width as usize * height as usize * 4],
                width,
            };
            // 先用 GDI 抓一帧作为初始内容，避免首个脏帧到来前黑屏
            if let Ok(mut gdi) = GdiCapturer::new() {
                if gdi.capture().is_ok() {
                    let data = gdi.data();
                    if data.len() == capturer.buffer.len() {
                        capturer.buffer.copy_from_slice(data);
                    }
                }
            }
            Ok(capturer)
        }

        fn capture(&mut self) -> io::Result<bool> {
            let DxgiCapturer {
                outputs,
                buffer,
                width,
            } = self;
            let mut updated = false;
            for out in outputs.iter_mut() {
                if out.copy_frame(buffer, *width)? {
                    updated = true;
                }
            }
            Ok(updated)
        }
    }

    /// 把一行行 BGRA 数据拷贝到虚拟桌面缓冲的指定偏移。
    fn copy_rows(
        buffer: &mut [u8],
        screen_width: u32,
        dx: u32,
        dy: u32,
        w: u32,
        h: u32,
        src: *const u8,
        pitch: usize,
    ) {
        let dst_stride = screen_width as usize * 4;
        let row_bytes = w as usize * 4;
        for row in 0..h as usize {
            let dst_start = (dy as usize + row) * dst_stride + dx as usize * 4;
            let dst = &mut buffer[dst_start..dst_start + row_bytes];
            let src_row = unsafe { src.add(row * pitch) };
            unsafe { std::ptr::copy_nonoverlapping(src_row, dst.as_mut_ptr(), row_bytes) };
        }
    }

    /// GDI 兼容路径：复用同一块 DIB Section，BitBlt 直接写入像素内存。
    struct GdiCapturer {
        width: u32,
        height: u32,
        screen_dc: HDC,
        mem_dc: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u8,
        bits_len: usize,
    }

    impl GdiCapturer {
        fn new() -> io::Result<Self> {
            let (w, h) = virtual_screen_size();
            let mut capturer = Self {
                width: w,
                height: h,
                screen_dc: null_mut(),
                mem_dc: null_mut(),
                bitmap: null_mut(),
                old: null_mut(),
                bits: null_mut(),
                bits_len: 0,
            };
            capturer.init()?;
            Ok(capturer)
        }

        /// 按当前分辨率建立 DC / DIB Section，之后跨帧复用。
        fn init(&mut self) -> io::Result<()> {
            self.release();
            unsafe {
                let screen_dc = GetDC(null_mut());
                if screen_dc.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let mem_dc = CreateCompatibleDC(screen_dc);
                if mem_dc.is_null() {
                    ReleaseDC(null_mut(), screen_dc);
                    return Err(io::Error::last_os_error());
                }

                let mut bmi: BITMAPINFO = zeroed();
                bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = self.width as i32;
                bmi.bmiHeader.biHeight = -(self.height as i32); // 负数 = 自上而下
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = 0; // BI_RGB

                let mut bits: *mut c_void = null_mut();
                let bitmap = CreateDIBSection(
                    screen_dc,
                    &bmi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    null_mut(),
                    0,
                );
                if bitmap.is_null() || bits.is_null() {
                    if !bitmap.is_null() {
                        DeleteObject(bitmap);
                    }
                    DeleteDC(mem_dc);
                    ReleaseDC(null_mut(), screen_dc);
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "无法创建 DIB Section",
                    ));
                }
                let old = SelectObject(mem_dc, bitmap);

                self.screen_dc = screen_dc;
                self.mem_dc = mem_dc;
                self.bitmap = bitmap;
                self.old = old;
                self.bits = bits as *mut u8;
                self.bits_len = self.width as usize * self.height as usize * 4;
            }
            Ok(())
        }

        fn capture(&mut self) -> io::Result<()> {
            unsafe {
                let (w, h) = virtual_screen_size();
                if (w, h) != (self.width, self.height) {
                    self.width = w;
                    self.height = h;
                    self.init()?;
                }
                let (ox, oy) = virtual_screen_origin();
                let ok = BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    self.screen_dc,
                    ox,
                    oy,
                    SRCCOPY | CAPTUREBLT,
                );
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }

        fn data(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.bits, self.bits_len) }
        }

        fn release(&mut self) {
            unsafe {
                if !self.mem_dc.is_null() {
                    if !self.old.is_null() {
                        SelectObject(self.mem_dc, self.old);
                    }
                    if !self.bitmap.is_null() {
                        DeleteObject(self.bitmap);
                    }
                    DeleteDC(self.mem_dc);
                }
                if !self.screen_dc.is_null() {
                    ReleaseDC(null_mut(), self.screen_dc);
                }
            }
            self.screen_dc = null_mut();
            self.mem_dc = null_mut();
            self.bitmap = null_mut();
            self.old = null_mut();
            self.bits = null_mut();
            self.bits_len = 0;
        }
    }

    impl Drop for GdiCapturer {
        fn drop(&mut self) {
            self.release();
        }
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

    pub fn virtual_screen_size() -> (u32, u32) {
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
        pub fn capture(&mut self) -> io::Result<Option<Frame<'_>>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "当前平台暂不支持屏幕捕获",
            ))
        }
    }
}
