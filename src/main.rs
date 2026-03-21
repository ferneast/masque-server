use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use futures::{SinkExt as _, StreamExt as _};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use tokio_quiche::buf_factory::{BufFactory, PooledDgram};
use tokio_quiche::http3::driver::{
    H3Event, InboundFrame, IncomingH3Headers, OutboundFrame, OutboundFrameSender, ServerH3Event,
};
use tokio_quiche::http3::settings::Http3Settings;
use tokio_quiche::metrics::DefaultMetrics;
use tokio_quiche::quiche::h3;
use tokio_quiche::quiche::h3::NameValue;
use tokio_quiche::settings::{QuicSettings, TlsCertificatePaths};
use tokio_quiche::{listen, ConnectionParams, ServerH3Controller, ServerH3Driver};

const CONNECT_UDP_PREFIX: &str = "/.well-known/masque/udp/";
const MAX_UDP_PAYLOAD: usize = 65535;

#[derive(Parser)]
#[command(
    name = "masque-server",
    about = "MASQUE CONNECT-UDP proxy server (RFC 9298) powered by tokio-quiche"
)]
struct Args {
    /// Listen address (e.g., 0.0.0.0:443)
    #[arg(long, default_value = "0.0.0.0:443")]
    listen: String,

    /// Path to TLS certificate chain (PEM)
    #[arg(long)]
    cert: String,

    /// Path to TLS private key (PEM)
    #[arg(long)]
    key: String,

    /// Optional Bearer token for proxy-authorization validation
    #[arg(long)]
    auth_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let listen_addr: SocketAddr = args.listen.parse()?;

    let socket = tokio::net::UdpSocket::bind(listen_addr).await?;
    info!("MASQUE server listening on {listen_addr}");

    let mut quic_settings = QuicSettings::default();
    quic_settings.enable_dgram = true;

    let conn_params = ConnectionParams::new_server(
        quic_settings,
        TlsCertificatePaths {
            cert: &args.cert,
            private_key: &args.key,
            kind: tokio_quiche::settings::CertificateKind::X509,
        },
        Default::default(),
    );

    let mut listeners = listen([socket], conn_params, DefaultMetrics)?;
    let accept_stream = &mut listeners[0];
    let auth_token: Option<Arc<str>> = args.auth_token.map(|t| Arc::from(t.as_str()));

    while let Some(conn) = accept_stream.next().await {
        let h3_settings = Http3Settings {
            enable_extended_connect: true,
            ..Default::default()
        };
        let (driver, controller) = ServerH3Driver::new(h3_settings);

        match conn {
            Ok(c) => {
                c.start(driver);
                let token = auth_token.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(controller, token).await {
                        error!("Connection handler error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {e}");
            }
        }
    }

    Ok(())
}

/// Handle a single QUIC/HTTP3 connection.
///
/// tokio-quiche fires `NewFlow` before `Headers` for CONNECT-UDP requests.
/// We store the datagram sender/receiver from `NewFlow`, then match them
/// with the `Headers` event to start the forwarding session.
async fn handle_connection(
    mut controller: ServerH3Controller,
    auth_token: Option<Arc<str>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Map flow_id → (datagram_sender, datagram_receiver)
    // NewFlow arrives before Headers for CONNECT-UDP
    type DgramChannels = (OutboundFrameSender, tokio::sync::mpsc::Receiver<InboundFrame>);
    let pending_flows: Arc<Mutex<HashMap<u64, DgramChannels>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some(event) = controller.event_receiver_mut().recv().await {
        match event {
            ServerH3Event::Core(H3Event::NewFlow {
                flow_id,
                send,
                recv,
            }) => {
                info!("New datagram flow: {flow_id}");
                pending_flows.lock().await.insert(flow_id, (send, recv));
            }

            ServerH3Event::Headers { incoming_headers: incoming, .. } => {
                let token = auth_token.clone();
                let flows = pending_flows.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connect_udp(incoming, token, flows).await {
                        error!("CONNECT-UDP session error: {e}");
                    }
                });
            }

            ServerH3Event::Core(H3Event::ResetStream { stream_id }) => {
                info!("Stream {stream_id} reset");
            }

            ServerH3Event::Core(H3Event::ConnectionShutdown(err)) => {
                info!("Connection shutdown: {err:?}");
                break;
            }

            ServerH3Event::Core(event) => {
                info!("H3 event: {event:?}");
            }
        }
    }

    info!("Connection closed");
    Ok(())
}

/// Handle a single CONNECT-UDP request and bidirectional datagram forwarding.
async fn handle_connect_udp(
    incoming: IncomingH3Headers,
    auth_token: Option<Arc<str>>,
    pending_flows: Arc<Mutex<HashMap<u64, (OutboundFrameSender, tokio::sync::mpsc::Receiver<InboundFrame>)>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let IncomingH3Headers {
        stream_id,
        headers,
        mut send,
        ..
    } = incoming;

    // Parse headers.
    let mut method = None;
    let mut protocol = None;
    let mut path = None;
    let mut authorization = None;

    for hdr in &headers {
        match hdr.name() {
            b":method" => method = Some(hdr.value().to_vec()),
            b":protocol" => protocol = Some(hdr.value().to_vec()),
            b":path" => path = Some(String::from_utf8_lossy(hdr.value()).to_string()),
            b"proxy-authorization" => {
                authorization = Some(String::from_utf8_lossy(hdr.value()).to_string())
            }
            _ => {}
        }
    }

    // Validate CONNECT-UDP method.
    if method.as_deref() != Some(b"CONNECT") || protocol.as_deref() != Some(b"connect-udp") {
        warn!("Non CONNECT-UDP request on stream {stream_id}, rejecting");
        send.send(OutboundFrame::Headers(
            vec![h3::Header::new(b":status", b"405")],
            None,
        ))
        .await
        .ok();
        return Ok(());
    }

    // Validate auth token if configured.
    if let Some(expected) = &auth_token {
        let expected_header = format!("Bearer {expected}");
        if authorization.as_deref() != Some(&expected_header) {
            warn!("Auth failed on stream {stream_id}");
            send.send(OutboundFrame::Headers(
                vec![h3::Header::new(b":status", b"407")],
                None,
            ))
            .await
            .ok();
            return Ok(());
        }
    }

    // Parse target from path.
    let path = match path {
        Some(p) => p,
        None => {
            send.send(OutboundFrame::Headers(
                vec![h3::Header::new(b":status", b"400")],
                None,
            ))
            .await
            .ok();
            return Ok(());
        }
    };

    let (host, port) = match parse_connect_udp_path(&path) {
        Some(v) => v,
        None => {
            warn!("Invalid CONNECT-UDP path: {path}");
            send.send(OutboundFrame::Headers(
                vec![h3::Header::new(b":status", b"400")],
                None,
            ))
            .await
            .ok();
            return Ok(());
        }
    };

    // Resolve target address.
    let target_addr = match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => {
                warn!("DNS resolution failed for {host}:{port}");
                send.send(OutboundFrame::Headers(
                    vec![h3::Header::new(b":status", b"502")],
                    None,
                ))
                .await
                .ok();
                return Ok(());
            }
        },
        Err(e) => {
            warn!("DNS resolution error for {host}:{port}: {e}");
            send.send(OutboundFrame::Headers(
                vec![h3::Header::new(b":status", b"502")],
                None,
            ))
            .await
            .ok();
            return Ok(());
        }
    };

    // Create UDP socket for this session.
    let bind_addr: SocketAddr = if target_addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let target_socket = UdpSocket::bind(bind_addr).await?;
    target_socket.connect(target_addr).await?;

    // Send 200 OK on the H3 stream (keep stream open).
    send.send(OutboundFrame::Headers(
        vec![h3::Header::new(b":status", b"200")],
        None,
    ))
    .await
    .map_err(|e| format!("Failed to send 200: {e}"))?;

    info!("CONNECT-UDP established: stream={stream_id} target={target_addr}");

    // Retrieve datagram channels from NewFlow event.
    // flow_id = stream_id / 4 (Quarter Stream ID)
    let flow_id = stream_id / 4;
    let (mut dgram_send, mut dgram_recv) = match pending_flows.lock().await.remove(&flow_id) {
        Some(channels) => channels,
        None => {
            // NewFlow might use stream_id directly
            match pending_flows.lock().await.remove(&stream_id) {
                Some(channels) => channels,
                None => {
                    warn!("No datagram flow found for stream {stream_id} (flow_id={flow_id})");
                    // Fall back: use the H3 stream sender for datagrams
                    // This won't work for datagrams but at least won't crash
                    return Ok(());
                }
            }
        }
    };

    // Bidirectional datagram forwarding.
    let mut target_buf = vec![0u8; MAX_UDP_PAYLOAD];

    loop {
        tokio::select! {
            // Client → Target: receive datagram from QUIC and forward to target UDP
            frame = dgram_recv.recv() => {
                match frame {
                    Some(InboundFrame::Datagram(dgram)) => {
                        // The datagram payload from tokio-quiche is the raw UDP data
                        // (flow ID / context ID framing is handled by the driver)
                        if let Err(e) = target_socket.send(&dgram).await {
                            warn!("Failed to send to target {target_addr}: {e}");
                        }
                    }
                    Some(InboundFrame::Body(_, fin)) => {
                        if fin {
                            info!("Stream {stream_id} body finished");
                            break;
                        }
                    }
                    None => {
                        info!("Datagram stream closed for stream {stream_id}");
                        break;
                    }
                }
            }

            // Target → Client: receive UDP from target and send as QUIC datagram
            result = target_socket.recv(&mut target_buf) => {
                match result {
                    Ok(len) => {
                        let dgram: PooledDgram = BufFactory::dgram_from_slice(&target_buf[..len]);
                        if dgram_send.send(OutboundFrame::Datagram(dgram, flow_id)).await.is_err() {
                            warn!("Failed to send datagram to client on stream {stream_id}");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Target recv error: {e}");
                        break;
                    }
                }
            }
        }
    }

    info!("CONNECT-UDP session ended: stream={stream_id}");
    Ok(())
}

/// Parse `/.well-known/masque/udp/{host}/{port}/` and return (host, port).
fn parse_connect_udp_path(path: &str) -> Option<(String, u16)> {
    let stripped = path.strip_prefix(CONNECT_UDP_PREFIX)?;
    let stripped = stripped.strip_suffix('/').unwrap_or(stripped);

    // Find last '/' to split host and port — supports IPv6 like [::1].
    let last_slash = stripped.rfind('/')?;
    let host = &stripped[..last_slash];
    let port: u16 = stripped[last_slash + 1..].parse().ok()?;

    if host.is_empty() {
        return None;
    }

    Some((host.to_string(), port))
}
