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
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, String> {
        let w = (width as usize).max(2) & !1;
        let h = (height as usize).max(2) & !1;
        let cfg = encoder_config(w, h, fps, bitrate_kbps);
        let encoder = Encoder::new(cfg).map_err(|e| format!("H264 编码器初始化失败: {e:?}"))?;
        Ok(Self {
            encoder,
            width: w,
            height: h,
            y: vec![0u8; w * h],
            u: vec![0u8; (w / 2) * (h / 2)],
            v: vec![0u8; (w / 2) * (h / 2)],
            fps,
            bitrate_kbps,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// 编码一帧 BGRA 画面，返回 Annex-B 字节流（可能包含 SPS/PPS/IDR）。
    pub fn encode(&mut self, bgra: &[u8], src_w: u32, src_h: u32) -> Result<Vec<u8>, String> {
        bgra_to_i420(
            bgra,
            src_w as usize,
            src_h as usize,
            self.width,
            self.height,
            &mut self.y,
            &mut self.u,
            &mut self.v,
        );
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

/// BGRA(top-down) -> I420，支持任意源尺寸到偶数目标尺寸的缩放。
/// 亮度用点采样，色度用 2x2 平均，速度快且对屏幕内容质量足够。
fn bgra_to_i420(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    y_out: &mut [u8],
    u_out: &mut [u8],
    v_out: &mut [u8],
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let c_w = dst_w / 2;
    let c_h = dst_h / 2;
    let y_stride = dst_w;
    let c_stride = c_w;

    for dy in 0..dst_h {
        let sy = (dy * src_h / dst_h).min(src_h - 1);
        for dx in 0..dst_w {
            let sx = (dx * src_w / dst_w).min(src_w - 1);
            let i = (sy * src_w + sx) * 4;
            if i + 2 >= src.len() {
                y_out[dy * y_stride + dx] = 16;
                continue;
            }
            let b = src[i] as i32;
            let g = src[i + 1] as i32;
            let r = src[i + 2] as i32;
            y_out[dy * y_stride + dx] = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(16, 235) as u8;
        }
    }

    for cy in 0..c_h {
        for cx in 0..c_w {
            let mut sb = 0i32;
            let mut sg = 0i32;
            let mut sr = 0i32;
            let mut n = 0i32;
            for yy in 0..2 {
                let sy = ((cy * 2 + yy) * src_h / dst_h).min(src_h - 1);
                for xx in 0..2 {
                    let sx = ((cx * 2 + xx) * src_w / dst_w).min(src_w - 1);
                    let i = (sy * src_w + sx) * 4;
                    if i + 2 < src.len() {
                        sb += src[i] as i32;
                        sg += src[i + 1] as i32;
                        sr += src[i + 2] as i32;
                        n += 1;
                    }
                }
            }
            let (u, v) = if n > 0 {
                let b = sb / n;
                let g = sg / n;
                let r = sr / n;
                let u = (128 + ((-44 * r - 85 * g + 128 * b) >> 8)).clamp(16, 240) as u8;
                let v = (128 + ((128 * r - 107 * g - 21 * b) >> 8)).clamp(16, 240) as u8;
                (u, v)
            } else {
                (128, 128)
            };
            u_out[cy * c_stride + cx] = u;
            v_out[cy * c_stride + cx] = v;
        }
    }
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
}
