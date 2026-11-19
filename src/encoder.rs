//! H.264 视频编码器：BGRA 截屏帧 -> I420 -> Annex-B H.264。
//!
//! 选用纯 Rust 编码器（无 ffmpeg 开发库依赖，便于绿色版分发）：
//! - Baseline/CAVLC、无 B 帧、无 lookahead：一帧输入对应一个 AU，低延迟
//! - 支持 ABR 码率控制与运行时强制 IDR（客户端丢包时立即恢复）

use std::sync::Arc;

use rusty_h264_common::YuvPlanes;
use rusty_h264_encoder::{Encoder, EncoderConfig};

/// 订阅者拿到的已编码帧。多个客户端共享同一份编码输出，避免重复编码。
#[derive(Clone)]
pub struct EncodedFrame {
    pub generation: u64,
    pub data: Arc<Vec<u8>>,
    #[allow(dead_code)]
    pub keyframe: bool,
}

/// H.264 编码器封装。内部复用 I420 平面缓冲，避免每帧分配整屏内存。
pub struct VideoEncoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    fps: u32,
    bitrate_kbps: u32,
    /// 源尺寸与预计算索引；索引表把每像素的除法移到帧外，拖动时每帧可省数百万次整数除法。
    src_w: usize,
    src_h: usize,
    identity_map: bool,
    luma_x: Vec<usize>,
    luma_y: Vec<usize>,
    chroma_x: Vec<(usize, usize)>,
    chroma_y: Vec<(usize, usize)>,
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let w = (width as usize).max(2) & !1;
        let h = (height as usize).max(2) & !1;
        let cfg = encoder_config(w, h, fps, bitrate_kbps);
        let encoder = Encoder::new(cfg).map_err(|e| format!("H264 编码器初始化失败: {e:?}"))?;
        let mut enc = Self {
            encoder,
            width: w,
            height: h,
            y: vec![0u8; w * h],
            u: vec![0u8; (w / 2) * (h / 2)],
            v: vec![0u8; (w / 2) * (h / 2)],
            fps,
            bitrate_kbps,
            src_w: 0,
            src_h: 0,
            identity_map: false,
            luma_x: Vec::new(),
            luma_y: Vec::new(),
            chroma_x: Vec::new(),
            chroma_y: Vec::new(),
        };
        enc.rebuild_maps(width as usize, height as usize);
        Ok(enc)
    }

    fn rebuild_maps(&mut self, src_w: usize, src_h: usize) {
        if src_w == 0 || src_h == 0 {
            return;
        }
        self.src_w = src_w;
        self.src_h = src_h;
        self.identity_map = src_w == self.width && src_h == self.height;
        self.luma_x = (0..self.width).map(|dx| dx * src_w / self.width).collect();
        self.luma_y = (0..self.height).map(|dy| dy * src_h / self.height).collect();
        self.chroma_x = (0..self.width / 2)
            .map(|cx| {
                let x0 = (cx * 2) * src_w / self.width;
                let x1 = (cx * 2 + 1) * src_w / self.width;
                (x0, x1)
            })
            .collect();
        self.chroma_y = (0..self.height / 2)
            .map(|cy| {
                let y0 = (cy * 2) * src_h / self.height;
                let y1 = (cy * 2 + 1) * src_h / self.height;
                (y0, y1)
            })
            .collect();
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// 编码一帧 BGRA 画面，返回 Annex-B 字节流（可能包含 SPS/PPS/IDR）。
    pub fn encode(&mut self, bgra: &[u8], src_w: u32, src_h: u32) -> Result<Vec<u8>, String> {
        self.bgra_to_i420(bgra, src_w as usize, src_h as usize);
        let planes = YuvPlanes::new(
            self.width,
            self.height,
            &self.y,
            &self.u,
            &self.v,
            self.width,
            self.width / 2,
        )
        .ok_or_else(|| "构造 YUV 平面失败".to_string())?;
        self.encoder
            .encode_planes(&planes)
            .map_err(|e| format!("H264 编码失败: {e:?}"))
    }

    /// 强制下一帧为 IDR 关键帧（收到 PLI/FIR 或客户端报告严重丢包时调用）。
    pub fn request_keyframe(&mut self) {
        self.encoder.request_keyframe();
    }

    /// 运行时调整目标码率（弱网降码率）。编码器不提供 setter，
    /// 用相同参数重建实例；参考帧自然清空，随后立即请求 IDR。
    pub fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), String> {
        let bitrate_kbps = bitrate_kbps.clamp(150, 20_000);
        if bitrate_kbps == self.bitrate_kbps {
            return Ok(());
        }
        let cfg = encoder_config(self.width, self.height, self.fps, bitrate_kbps);
        let encoder = Encoder::new(cfg).map_err(|e| format!("H264 编码器重建失败: {e:?}"))?;
        self.encoder = encoder;
        self.bitrate_kbps = bitrate_kbps;
        self.encoder.request_keyframe();
        Ok(())
    }

    /// BGRA(top-down) -> I420。采样索引在 `rebuild_maps` 中预计算，
    /// 且同尺寸时用顺序迭代，热循环内不再有整数除法与逐像素越界判断。
    fn bgra_to_i420(&mut self, src: &[u8], src_w: usize, src_h: usize) {
        if src_w == 0 || src_h == 0 {
            return;
        }
        if src_w != self.src_w || src_h != self.src_h {
            self.rebuild_maps(src_w, src_h);
        }
        let needed = src_w.saturating_mul(src_h).saturating_mul(4);
        if src.len() < needed {
            self.y.fill(16);
            self.u.fill(128);
            self.v.fill(128);
            return;
        }

        let dst_w = self.width;
        let dst_h = self.height;
        let c_w = dst_w / 2;
        let c_h = dst_h / 2;
        let src_stride = src_w * 4;

        // 亮度：点采样。绝大多数场景源/目标同尺寸，走无索引的顺序分支。
        if self.identity_map {
            for dy in 0..dst_h {
                let src_row = &src[dy * src_stride..(dy + 1) * src_stride];
                let dst_row = &mut self.y[dy * dst_w..(dy + 1) * dst_w];
                for (out, px) in dst_row.iter_mut().zip(src_row.chunks_exact(4)) {
                    let b = px[0] as i32;
                    let g = px[1] as i32;
                    let r = px[2] as i32;
                    *out = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(16, 235) as u8;
                }
            }
        } else {
            for dy in 0..dst_h {
                let src_row = self.luma_y[dy] * src_stride;
                let dst_row = dy * dst_w;
                for dx in 0..dst_w {
                    let i = src_row + self.luma_x[dx] * 4;
                    let b = src[i] as i32;
                    let g = src[i + 1] as i32;
                    let r = src[i + 2] as i32;
                    self.y[dst_row + dx] =
                        ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(16, 235) as u8;
                }
            }
        }

        // 色度：2x2 平均，四个偏移由索引表直接给出。
        let (u_out, v_out) = (self.u.as_mut_slice(), self.v.as_mut_slice());
        for cy in 0..c_h {
            let (sy0, sy1) = self.chroma_y[cy];
            let row0 = sy0 * src_stride;
            let row1 = sy1 * src_stride;
            let u_row = &mut u_out[cy * c_w..(cy + 1) * c_w];
            let v_row = &mut v_out[cy * c_w..(cy + 1) * c_w];
            for cx in 0..c_w {
                let (sx0, sx1) = self.chroma_x[cx];
                let i0 = row0 + sx0 * 4;
                let i1 = row0 + sx1 * 4;
                let i2 = row1 + sx0 * 4;
                let i3 = row1 + sx1 * 4;
                let sb = src[i0] as i32 + src[i1] as i32 + src[i2] as i32 + src[i3] as i32;
                let sg = src[i0 + 1] as i32
                    + src[i1 + 1] as i32
                    + src[i2 + 1] as i32
                    + src[i3 + 1] as i32;
                let sr = src[i0 + 2] as i32
                    + src[i1 + 2] as i32
                    + src[i2 + 2] as i32
                    + src[i3 + 2] as i32;
                let (b, g, r) = (sb >> 2, sg >> 2, sr >> 2);
                u_row[cx] = (128 + ((-44 * r - 85 * g + 128 * b) >> 8)).clamp(16, 240) as u8;
                v_row[cx] = (128 + ((128 * r - 107 * g - 21 * b) >> 8)).clamp(16, 240) as u8;
            }
        }
    }
}

fn encoder_config(width: usize, height: usize, fps: u32, bitrate_kbps: u32) -> EncoderConfig {
    let mut cfg = EncoderConfig::baseline(width, height);
    cfg.framerate = fps.max(1) as f32;
    cfg.bitrate = bitrate_kbps.max(150) * 1000;
    cfg.qp = 26;
    // 关键帧间隔约 2 秒：兼顾随机接入速度与压缩效率
    cfg.gop_size = (fps * 2).max(15);
    cfg.min_keyint = fps.max(1);
    cfg.scenecut = 40;
    cfg
}

#[cfg(test)]
mod bench {
    use super::*;
    #[test]
    fn measure_throughput() {
        for (w, h) in [(1920u32, 1080u32), (1280, 720)] {
            let mut enc = VideoEncoder::new(w, h, 15, 2500).unwrap();
            let frame = vec![120u8; (w * h * 4) as usize];
            let n = 12;
            let start = std::time::Instant::now();
            let mut total = 0usize;
            for _ in 0..n {
                total += enc.encode(&frame, w, h).unwrap().len();
            }
            let el = start.elapsed();
            eprintln!(
                "BENCH {}x{}: {:.1} fps, {:.0} B/frame, total {}ms",
                w,
                h,
                n as f64 / el.as_secs_f64(),
                total as f64 / n as f64,
                el.as_millis()
            );
        }
    }

    fn reference_convert(
        src: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut y = vec![0u8; dst_w * dst_h];
        let mut u = vec![0u8; (dst_w / 2) * (dst_h / 2)];
        let mut v = vec![0u8; (dst_w / 2) * (dst_h / 2)];
        for dy in 0..dst_h {
            let sy = (dy * src_h / dst_h).min(src_h - 1);
            for dx in 0..dst_w {
                let sx = (dx * src_w / dst_w).min(src_w - 1);
                let i = (sy * src_w + sx) * 4;
                let (b, g, r) = (src[i] as i32, src[i + 1] as i32, src[i + 2] as i32);
                y[dy * dst_w + dx] =
                    ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(16, 235) as u8;
            }
        }
        for cy in 0..dst_h / 2 {
            for cx in 0..dst_w / 2 {
                let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
                for yy in 0..2 {
                    let sy = ((cy * 2 + yy) * src_h / dst_h).min(src_h - 1);
                    for xx in 0..2 {
                        let sx = ((cx * 2 + xx) * src_w / dst_w).min(src_w - 1);
                        let i = (sy * src_w + sx) * 4;
                        sb += src[i] as i32;
                        sg += src[i + 1] as i32;
                        sr += src[i + 2] as i32;
                    }
                }
                let (b, g, r) = (sb >> 2, sg >> 2, sr >> 2);
                u[cy * (dst_w / 2) + cx] =
                    (128 + ((-44 * r - 85 * g + 128 * b) >> 8)).clamp(16, 240) as u8;
                v[cy * (dst_w / 2) + cx] =
                    (128 + ((128 * r - 107 * g - 21 * b) >> 8)).clamp(16, 240) as u8;
            }
        }
        (y, u, v)
    }

    #[test]
    fn optimized_conversion_matches_reference() {
        for (src_w, src_h, dst_w, dst_h) in [(64usize, 36usize, 64usize, 36usize), (65, 37, 64, 36), (128, 72, 64, 36)] {
            let src: Vec<u8> = (0..src_w * src_h * 4)
                .map(|i| ((i * 37) ^ (i >> 5)) as u8)
                .collect();
            let (ry, ru, rv) = reference_convert(&src, src_w, src_h, dst_w, dst_h);
            let mut enc = VideoEncoder::new(dst_w as u32, dst_h as u32, 30, 1000).unwrap();
            enc.bgra_to_i420(&src, src_w, src_h);
            assert_eq!(enc.y, ry, "Y mismatch for {src_w}x{src_h} -> {dst_w}x{dst_h}");
            assert_eq!(enc.u, ru, "U mismatch for {src_w}x{src_h} -> {dst_w}x{dst_h}");
            assert_eq!(enc.v, rv, "V mismatch for {src_w}x{src_h} -> {dst_w}x{dst_h}");
        }
    }
}
