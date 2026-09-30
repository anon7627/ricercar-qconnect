//! Qobuz cloud WebSocket client: qcloud framing + AUTHENTICATE/SUBSCRIBE/JOIN_SESSION,
//! with reconnect and self-minted token refresh. Joins the signed-in
//! account's session, like the web player.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use prost::encoding::{decode_varint, encode_varint};
use prost::Message as _;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, WebSocketStream};

use crate::api::{now_ms, ApiClient, WsToken};
use crate::config::{DeviceIdentity, Quality};
use crate::msgtype::{self, qcloud};
use crate::proto::{
    Authenticate, CtrlSrvrJoinSession, DeviceCapabilities, DeviceInfo, DeviceType, ErrorMessage, Payload,
    QConnectBatch, QConnectMessage, Subscribe,
};

/// Events the WS client reports to the session.
#[derive(Debug)]
pub enum WsEvent {
    Connected,
    Disconnected,
    /// A decoded inbound QConnectBatch.
    Batch(QConnectBatch),
}

pub struct WsParams {
    pub device: DeviceIdentity,
    pub device_name: String,
    pub brand: String,
    pub model: String,
    pub max_quality: Quality,
    /// Tell controllers the volume cannot be changed.
    pub fixed_volume: bool,
}

type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Backoff bounds between reconnection attempts.
const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 30_000;

/// Runs the connection loop until the task is aborted. Reconnects with
/// exponential backoff, self-minting a fresh token when the current one lapses.
pub async fn run_ws(
    params: WsParams,
    api: Arc<tokio::sync::Mutex<ApiClient>>,
    event_tx: UnboundedSender<WsEvent>,
    mut batch_rx: UnboundedReceiver<QConnectBatch>,
) {
    let mut token: Option<WsToken> = None;
    let mut force_refresh = false;
    let mut backoff_ms = BACKOFF_MIN_MS;
    // The msgId counter persists across reconnects: the server only requires
    // monotonically increasing ids, and a continuing counter always is.
    let mut msg_id: u32 = 0;

    loop {
        // Ensure we hold a usable token, minting a fresh one when possible.
        let tok = loop {
            let usable = token.as_ref().filter(|t| !t.is_expired(60)).cloned();
            if let Some(t) = usable.clone().filter(|_| !force_refresh) {
                break t;
            }
            let minted = api.lock().await.get_ws_token(token.is_some()).await;
            match minted {
                Ok(Some(t)) => {
                    token = Some(t.clone());
                    break t;
                }
                Ok(None) => tracing::debug!("ws: token endpoint returned no token"),
                Err(e) => tracing::debug!("ws: minting token failed: {e}"),
            }
            if let Some(t) = usable {
                // Could not refresh, but the current token is still valid.
                break t;
            }
            tracing::warn!("ws: no valid token; retrying in 10s");
            tokio::time::sleep(Duration::from_secs(10)).await;
        };

        // Drop stale outbound state accumulated while disconnected; replaying
        // it against a fresh connection gets the new session killed.
        while batch_rx.try_recv().is_ok() {}

        let mut joined = false;
        let result = connect_and_run(&tok, &params, &mut msg_id, &mut batch_rx, &event_tx, &mut joined).await;

        if joined {
            backoff_ms = BACKOFF_MIN_MS;
        }
        let why = match result {
            Ok(()) => "closed by server".to_string(),
            Err(e) => e.to_string(),
        };
        tracing::warn!("ws: connection ended ({why}); retrying in {backoff_ms}ms");
        let _ = event_tx.send(WsEvent::Disconnected);
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
        // The server may have dropped us because of the token; get a new one.
        force_refresh = true;
    }
}

async fn connect_and_run(
    tok: &WsToken,
    params: &WsParams,
    msg_id: &mut u32,
    batch_rx: &mut UnboundedReceiver<QConnectBatch>,
    event_tx: &UnboundedSender<WsEvent>,
    joined: &mut bool,
) -> anyhow::Result<()> {
    let mut request = tok.endpoint.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Origin", HeaderValue::from_static(msgtype::api::PLAY_ORIGIN));
    headers.insert("User-Agent", HeaderValue::from_static(msgtype::api::USER_AGENT));

    let (mut ws, _resp) = connect_async(request).await?;

    // Handshake: AUTHENTICATE -> SUBSCRIBE -> JOIN_SESSION.
    let auth = Authenticate { msg_id: Some(next_msg_id(msg_id)), msg_date: Some(now_ms()), jwt: Some(tok.jwt.clone()) };
    send_frame(&mut ws, qcloud::AUTHENTICATE, &auth.encode_to_vec()).await?;
    let sub = Subscribe {
        msg_id: Some(next_msg_id(msg_id)),
        msg_date: Some(now_ms()),
        proto: Some(qcloud::PROTO_QCONNECT),
        // The account session's uuid is not known yet.
        channels: vec![],
    };
    send_frame(&mut ws, qcloud::SUBSCRIBE, &sub.encode_to_vec()).await?;
    send_payload(&mut ws, msg_id, &join_batch(params)).await?;

    *joined = true;
    let _ = event_tx.send(WsEvent::Connected);
    tracing::info!("ws: connected; joining the account's session");

    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                ws.send(WsMessage::Ping(Default::default())).await?;
            }
            maybe_batch = batch_rx.recv() => {
                let Some(batch) = maybe_batch else {
                    let _ = ws.close(None).await;
                    return Ok(());
                };
                send_payload(&mut ws, msg_id, &batch).await?;
            }
            incoming = ws.next() => {
                let Some(msg) = incoming else {
                    return Ok(());
                };
                match msg? {
                    WsMessage::Binary(data) => handle_binary_frame(&data, event_tx)?,
                    WsMessage::Close(_) => return Ok(()),
                    // tungstenite answers pings itself on the next write/flush.
                    _ => {}
                }
            }
        }
    }
}

/// Controller-renderer join without a session uuid, as the web player does.
fn join_batch(params: &WsParams) -> QConnectBatch {
    let device_info = DeviceInfo {
        device_uuid: Some(params.device.uuid_bytes().to_vec()),
        friendly_name: Some(params.device_name.clone()),
        brand: Some(params.brand.clone()),
        model: Some(params.model.clone()),
        serial_number: Some(params.device.uuid.clone()),
        r#type: Some(DeviceType::Speaker as i32),
        capabilities: Some(DeviceCapabilities {
            min_audio_quality: Some(Quality::Mp3.protocol()),
            max_audio_quality: Some(params.max_quality.protocol()),
            // 1 = NOT_ALLOWED, 2 = ALLOWED
            volume_remote_control: Some(if params.fixed_volume { 1 } else { 2 }),
        }),
        software_version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    let message = QConnectMessage {
        message_type: Some(msgtype::msg::CTRL_SRVR_JOIN_SESSION),
        ctrl_srvr_join_session: Some(CtrlSrvrJoinSession { session_uuid: None, device_info: Some(device_info) }),
        ..Default::default()
    };

    QConnectBatch { messages_time: Some(now_ms()), messages_id: Some(0), messages: vec![message] }
}

async fn send_payload(ws: &mut WsStream, msg_id: &mut u32, batch: &QConnectBatch) -> anyhow::Result<()> {
    let payload = Payload {
        msg_id: Some(next_msg_id(msg_id)),
        msg_date: Some(now_ms()),
        proto: Some(qcloud::PROTO_QCONNECT),
        src: None,
        dests: vec![vec![qcloud::CHANNEL_BACKEND]],
        payload: Some(batch.encode_to_vec()),
    };
    send_frame(ws, qcloud::PAYLOAD, &payload.encode_to_vec()).await
}

async fn send_frame(ws: &mut WsStream, msg_type: u8, payload: &[u8]) -> anyhow::Result<()> {
    ws.send(WsMessage::Binary(encode_frame(msg_type, payload).into())).await?;
    Ok(())
}

/// qcloud frame: 1 type byte, varint payload length, payload.
fn encode_frame(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + 10 + payload.len());
    frame.push(msg_type);
    encode_varint(payload.len() as u64, &mut frame);
    frame.extend_from_slice(payload);
    frame
}

/// Split a qcloud frame into (type, payload). `None` if truncated.
fn decode_frame(data: &[u8]) -> anyhow::Result<Option<(u8, &[u8])>> {
    let Some((&msg_type, mut rest)) = data.split_first() else {
        return Ok(None);
    };
    let len = decode_varint(&mut rest)? as usize;
    Ok(rest.get(..len).map(|payload| (msg_type, payload)))
}

/// Inbound payloads are wrapped in a qcloud `Payload` envelope; fall back to
/// a bare batch for servers that skip it.
fn decode_batch(payload: &[u8]) -> Result<QConnectBatch, String> {
    let wrapped = match Payload::decode(payload) {
        // An envelope with a batch in it: that batch or nothing (read as a
        // bare batch, the envelope would pass for an empty one).
        Ok(Payload { payload: Some(inner), .. }) => {
            return QConnectBatch::decode(inner.as_slice()).map_err(|e| format!("batch in the envelope: {e}"));
        }
        Ok(_) => "envelope without a batch".to_string(),
        Err(e) => format!("envelope: {e}"),
    };
    QConnectBatch::decode(payload).map_err(|bare| format!("{wrapped}; as a bare batch: {bare}"))
}

fn handle_binary_frame(data: &[u8], event_tx: &UnboundedSender<WsEvent>) -> anyhow::Result<()> {
    let Some((msg_type, payload)) = decode_frame(data)? else {
        tracing::debug!("ws: ignoring truncated frame ({} bytes)", data.len());
        return Ok(());
    };

    match msg_type {
        qcloud::PAYLOAD => {
            match decode_batch(payload) {
                Ok(batch) => {
                    let _ = event_tx.send(WsEvent::Batch(batch));
                }
                // A whole batch lost: the session misses commands. Loud, since
                // it means the protocol description no longer matches Qobuz.
                Err(e) => tracing::warn!("ws: inbound batch dropped, cannot decode it: {e}"),
            }
        }
        qcloud::ERROR => match ErrorMessage::decode(payload) {
            Ok(err) => {
                let code = err.code.unwrap_or(0);
                let descr = err.descr.unwrap_or_default();
                tracing::warn!("ws: cloud error code={code} descr={descr}");
            }
            Err(e) => tracing::warn!("ws: failed to decode ErrorMessage: {e}"),
        },
        qcloud::DISCONNECT => {
            anyhow::bail!("server requested disconnect");
        }
        other => tracing::debug!("ws: ignoring frame type {other}"),
    }
    Ok(())
}

fn next_msg_id(msg_id: &mut u32) -> u32 {
    *msg_id = msg_id.wrapping_add(1);
    *msg_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_with_multibyte_length() {
        let payload = vec![7u8; 300];
        let frame = encode_frame(qcloud::PAYLOAD, &payload);
        assert_eq!(&frame[1..3], &[0xAC, 0x02]); // varint(300)
        let (ty, body) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(ty, qcloud::PAYLOAD);
        assert_eq!(body, payload.as_slice());
    }

    /// A `SrvrRndrSetState` as the Qobuz server sends it, encoded by hand:
    /// track ids are fixed32, and "no item" is queue item -1.
    #[test]
    fn set_state_with_tracks_decodes() {
        fn ld(field: u8, body: &[u8]) -> Vec<u8> {
            let mut v = vec![(field << 3) | 2];
            encode_varint(body.len() as u64, &mut v);
            v.extend_from_slice(body);
            v
        }
        fn track_ref(item: i32, track: u32) -> Vec<u8> {
            let mut v = vec![0x08]; // queueItemId, varint (int32: -1 takes 10 bytes)
            encode_varint(item as i64 as u64, &mut v);
            v.push(0x15); // trackId, fixed32
            v.extend_from_slice(&track.to_le_bytes());
            v
        }
        let mut set_state = vec![0x08, 0x02, 0x10, 0xE8, 0x07]; // playingState, currentPosition 1000
        set_state.extend(ld(4, &track_ref(3, 287_336_643)));
        set_state.extend(ld(5, &track_ref(-1, 0)));
        let mut message = vec![0x08, 41]; // message_type SRVR_RNDR_SET_STATE
        let mut field41 = vec![0xCA, 0x02]; // tag of field 41, length-delimited
        encode_varint(set_state.len() as u64, &mut field41);
        field41.extend(&set_state);
        message.extend(field41);
        let batch = ld(3, &message);
        let envelope = ld(7, &batch);

        let decoded = decode_batch(&envelope).expect("the server's SetState decodes");
        let s = decoded.messages[0].srvr_rndr_set_state.as_ref().unwrap();
        let cur = s.current_queue_item.as_ref().unwrap();
        assert_eq!((cur.queue_item_id, cur.track_id), (Some(3), Some(287_336_643)));
        assert_eq!(s.next_queue_item.as_ref().unwrap().queue_item_id, Some(-1));
        assert_eq!(s.current_position, Some(1000));

        // A varint track id (what the old description expected) is refused
        // with a readable reason.
        let wrong = ld(7, &ld(3, &[vec![0x08, 41, 0xCA, 0x02, 4], ld(4, &[0x10, 0x05])].concat()));
        let err = decode_batch(&wrong).unwrap_err();
        assert!(err.contains("wire type"), "{err}");
    }

    #[test]
    fn truncated_frame_is_none() {
        let frame = encode_frame(qcloud::PAYLOAD, &[1, 2, 3]);
        assert!(decode_frame(&frame[..frame.len() - 1]).unwrap().is_none());
        assert!(decode_frame(&[]).unwrap().is_none());
    }

    fn params() -> WsParams {
        WsParams {
            device: DeviceIdentity { uuid: "6f1c2b9e-1111-4222-8333-944455556666".into() },
            device_name: "Salon".into(),
            brand: "b".into(),
            model: "m".into(),
            max_quality: Quality::HiRes192,
            fixed_volume: true,
        }
    }

    #[test]
    fn account_join_is_a_controller_renderer_join_without_session() {
        let batch = join_batch(&params());
        let m = &batch.messages[0];
        assert_eq!(m.message_type, Some(msgtype::msg::CTRL_SRVR_JOIN_SESSION));
        let j = m.ctrl_srvr_join_session.as_ref().unwrap();
        assert_eq!(j.session_uuid, None);
        let info = j.device_info.as_ref().unwrap();
        assert_eq!(info.friendly_name.as_deref(), Some("Salon"));
        assert_eq!(info.capabilities.unwrap().volume_remote_control, Some(1), "fixed volume = NOT_ALLOWED");
        // Field 61 on the wire, as in the web player.
        assert_eq!(&m.encode_to_vec()[2..4], &[0xEA, 0x03]);
    }

    #[test]
    fn inbound_payload_envelope_is_unwrapped() {
        let batch = QConnectBatch { messages_time: Some(1), messages_id: Some(5), messages: vec![] };
        let envelope = Payload { payload: Some(batch.encode_to_vec()), ..Default::default() };
        let frame = encode_frame(qcloud::PAYLOAD, &envelope.encode_to_vec());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle_binary_frame(&frame, &tx).unwrap();
        match rx.try_recv().unwrap() {
            WsEvent::Batch(b) => assert_eq!(b.messages_id, Some(5)),
            other => panic!("unexpected {other:?}"),
        }
    }
}
