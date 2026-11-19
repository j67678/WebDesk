//! WebRTC PeerConnection 会话（仅 IP 直连 / host candidate，无 STUN/TURN）。
//!
//! 服务端作为 answerer：浏览器发起 offer，服务端建立 PC 并挂载 H.264 视频轨，
//! 采用 non-trickle ICE：等 ICE 收集完成后把完整 SDP 作为 answer 一次回给浏览器。
//! 输入/信令仍走已有 WebSocket，视频走 WebRTC（拥塞控制、抖动缓冲、NACK/PLI 由浏览器负责）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use rtc::media::Sample;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::MediaStreamTrack;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCPeerConnectionState, RTCSessionDescription, Registry,
};

const H264_FMTP: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Handler {
    gather_tx: mpsc::UnboundedSender<()>,
    state_tx: mpsc::UnboundedSender<RTCPeerConnectionState>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_tx.send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state_tx.send(state);
    }
}

/// 视频轨写入句柄：可克隆，供媒体发送任务持有。
#[derive(Clone)]
pub struct TrackWriter {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: PayloadType,
}

impl TrackWriter {
    /// 把一帧 Annex-B H.264 写入 RTP 轨（库内部负责分片、序号、时间戳）。
    pub async fn write_h264(&self, data: Vec<u8>, duration: Duration) -> Result<(), String> {
        let sample = Sample {
            data: Bytes::from(data),
            duration,
            ..Sample::new(Instant::now())
        };
        self.track
            .sample_writer(self.ssrc, self.payload_type)
            .write_sample(&sample)
            .await
            .map_err(|e| format!("写入 RTP sample 失败: {e}"))
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// 一条 WebRTC 会话：写入句柄 + 连接状态通知。
pub struct RtcSession {
    pub writer: TrackWriter,
    pub state_rx: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
}

/// 根据浏览器 offer 建立 WebRTC 会话，返回 (会话, answer SDP JSON)。
pub async fn establish(offer_sdp: String) -> Result<(RtcSession, String), String> {
    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: H264_FMTP.to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    };

    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(video_codec.clone(), RtpCodecKind::Video)
        .map_err(|e| format!("注册 H264 编码失败: {e}"))?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|e| format!("注册拦截器失败: {e}"))?;

    // 无 ice_servers => 只收集 host candidate，保证纯 IP 直连
    let config = RTCConfigurationBuilder::new().with_ice_servers(vec![]).build();

    let (gather_tx, mut gather_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    let handler = Arc::new(Handler {
        gather_tx,
        state_tx,
    });

    // 双栈通配绑定：webrtc-rs 会把 0.0.0.0 / [::] 展开为本机各可用接口的真实
    // host candidate（跳过 loopback/link-local）。只绑 IPv4 时纯 IPv6 客户端没有
    // 同族 candidate 可配对，ICE 永远停在 checking，表现为“已协商但无画面”。
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0", "[::]:0"])
        .build()
        .await
        .map_err(|e| format!("创建 PeerConnection 失败: {e}"))?;

    let ssrc = rand::random::<u32>();
    let track = Arc::new(
        TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "webdesk-stream".to_owned(),
                "webdesk-video".to_owned(),
                "webdesk-video".to_owned(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: video_codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            ),
        )
        .map_err(|e| format!("创建视频轨失败: {e}"))?,
    );
    let sender = pc
        .add_track(track.clone() as Arc<dyn TrackLocal>)
        .await
        .map_err(|e| format!("添加视频轨失败: {e}"))?;

    let offer = RTCSessionDescription::offer(offer_sdp)
        .map_err(|e| format!("解析 offer SDP 失败: {e}"))?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| format!("设置远端 SDP 失败: {e}"))?;
    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("创建 answer 失败: {e}"))?;
    pc.set_local_description(answer)
        .await
        .map_err(|e| format!("设置本地 SDP 失败: {e}"))?;

    // non-trickle：等待本地 ICE 收集完成，answer 中带上全部 host candidate
    let _ = tokio::time::timeout(ICE_GATHER_TIMEOUT, gather_rx.recv()).await;
    let local_desc = pc
        .local_description()
        .await
        .ok_or_else(|| "未生成 answer SDP".to_string())?;
    let answer_value = serde_json::to_value(&local_desc)
        .map_err(|e| format!("序列化 answer SDP 失败: {e}"))?;
    let answer_sdp = answer_value
        .get("sdp")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    let payload_type = sender
        .get_parameters()
        .await
        .map_err(|e| format!("读取协商参数失败: {e}"))?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| "协商后无可用编解码器".to_string())?;

    Ok((
        RtcSession {
            writer: TrackWriter {
                pc: Arc::new(pc) as Arc<dyn PeerConnection>,
                track,
                ssrc,
                payload_type,
            },
            state_rx,
        },
        answer_sdp,
    ))
}
