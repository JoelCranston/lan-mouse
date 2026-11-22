use async_trait::async_trait;
use futures::{Stream, StreamExt, future::select_all};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    collections::{HashMap, VecDeque},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::crypto;

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let authorized = authorized_keys.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    assert!(certs.len() == 1);
                    let fingerprints = certs
                        .iter()
                        .map(|c| crypto::generate_fingerprint(c))
                        .collect::<Vec<_>>();
                    if authorized
                        .read()
                        .expect("lock")
                        .contains_key(&fingerprints[0])
                    {
                        Ok(())
                    } else {
                        let fingerprint = fingerprints.into_iter().next().expect("fingerprint");
                        connection_attempts
                            .lock()
                            .expect("lock")
                            .push_back(fingerprint);
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let mut listener = listen_dual_stack(port, cfg.clone()).await?;

        let conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>> =
            Rc::new(AsyncMutex::new(Vec::new()));

        let conns_clone = conns.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let connection_attempts = connection_attempts.clone();
            spawn_local(async move {
                loop {
                    let sleep = tokio::time::sleep(Duration::from_secs(2));
                    tokio::select! {
                        /* workaround for https://github.com/webrtc-rs/webrtc/issues/614 */
                        _ = sleep => continue,
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                log::info!("dtls client connected, ip: {addr}");
                                let mut conns = conns_clone.lock().await;
                                conns.push((addr, conn.clone()));
                                let dtls_conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
                                let certs = dtls_conn.connection_state().await.peer_certificates;
                                let cert = certs.first().expect("cert");
                                let fingerprint = crypto::generate_fingerprint(cert);
                                listen_tx.send(ListenEvent::Accept { addr, fingerprint }).expect("channel closed");
                                spawn_local(read_loop(conns_clone.clone(), addr, conn, listen_tx.clone()));
                            },
                            Err(e) => {
                                if let Error::Std(ref e) = e {
                                    if let Some(e) = e.0.downcast_ref::<webrtc_dtls::Error>() {
                                        match e {
                                            webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                                if let Some(fingerprint) = connection_attempts.lock().expect("lock").pop_front() {
                                                    listen_tx.send(ListenEvent::Rejected { fingerprint }).expect("channel closed");
                                                }
                                            }
                                            _ => log::warn!("accept: {e}"),
                                        }
                                    } else {
                                        log::warn!("accept: {e:?}");
                                    }
                                } else {
                                    log::warn!("accept: {e:?}");
                                }
                            }
                        },
                        port = request_port_change_rx.recv() => {
                            let port = port.expect("channel closed");
                            match listen_dual_stack(port, cfg.clone()).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    port_changed_tx.send(Ok(port)).expect("channel closed");
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    port_changed_tx.send(Err(e)).expect("channel closed");
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for (_, conn) in conns.iter() {
            let _ = conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let conns = self.conns.lock().await;
        for (a, conn) in conns.iter() {
            if *a == addr {
                let _ = conn.send(&buf[..len]).await;
            }
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        if let Some(conn) = self
            .conns
            .lock()
            .await
            .iter()
            .find(|(a, _)| *a == addr)
            .map(|(_, c)| c.clone())
        {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
    }
}

type BoxedListener = Box<dyn Listener + Send + Sync>;

/// Listens for DTLS connections on both IPv6 and IPv4.
///
/// Depending on the platform, a socket bound to `[::]` either accepts
/// IPv4 connections as well (dual-stack, the default on Linux and macOS)
/// or only IPv6 (the default on Windows). To support both cases, we
/// bind `[::]` first and additionally bind `0.0.0.0` unless the IPv6
/// socket already covers IPv4.
struct DualStackListener {
    listeners: Vec<BoxedListener>,
}

/// Returns whether an IPv6 socket bound to `[::]` accepts IPv4
/// connections as well on this system (i.e. `IPV6_V6ONLY` is off by default).
fn ipv6_dual_stack_default() -> bool {
    // `[::]` is IPv6-only by default on Windows. Don't probe there:
    // Windows can return `IPV6_V6ONLY` as a 1-byte value, which trips a
    // size assertion in socket2's `only_v6()` (a panic in debug builds).
    if cfg!(windows) {
        return false;
    }
    socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, None)
        .and_then(|s| s.only_v6())
        .map(|only_v6| !only_v6)
        .unwrap_or(false)
}

async fn listen_dual_stack(
    port: u16,
    cfg: Config,
) -> Result<DualStackListener, ListenerCreationError> {
    let mut listeners: Vec<BoxedListener> = Vec::new();
    let mut port = port;

    match listen(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), cfg.clone()).await {
        Ok(l) => {
            // if port 0 was requested, use the same port for IPv4
            if let Ok(addr) = l.addr().await {
                port = addr.port();
            }
            listeners.push(Box::new(l));
        }
        Err(e) => log::warn!("could not listen on IPv6 (port {port}): {e}"),
    }

    let v6_covers_v4 = !listeners.is_empty() && ipv6_dual_stack_default();
    match listen(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), cfg).await {
        Ok(l) => listeners.push(Box::new(l)),
        // expected on dual-stack systems: the IPv6 socket already owns the port
        Err(e) if v6_covers_v4 => {
            log::debug!("IPv4 is handled by the dual-stack IPv6 socket ({e})")
        }
        Err(e) => {
            for l in listeners.iter() {
                let _ = l.close().await;
            }
            return Err(e.into());
        }
    }

    Ok(DualStackListener { listeners })
}

/// Converts IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`), as reported by
/// dual-stack sockets for IPv4 peers, back into plain IPv4 addresses, so
/// that they match the IPv4 addresses configured for a client.
fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(a) => match a.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::new(ip.into(), a.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

#[async_trait]
impl Listener for DualStackListener {
    async fn accept(&self) -> Result<(ArcConn, SocketAddr), Error> {
        let accepts = self.listeners.iter().map(|l| l.accept());
        let (res, _, _) = select_all(accepts).await;
        res.map(|(conn, addr)| (conn, canonical_addr(addr)))
    }

    async fn close(&self) -> Result<(), Error> {
        let mut res = Ok(());
        for l in self.listeners.iter() {
            if let Err(e) = l.close().await {
                res = Err(e);
            }
        }
        res
    }

    async fn addr(&self) -> Result<SocketAddr, Error> {
        self.listeners[0].addr().await
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.listen_rx.poll_next_unpin(cx)
    }
}

async fn read_loop(
    conns: Rc<AsyncMutex<Vec<(SocketAddr, ArcConn)>>>,
    addr: SocketAddr,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
) -> Result<(), Error> {
    let mut b = [0u8; MAX_EVENT_SIZE];

    while conn.recv(&mut b).await.is_ok() {
        match b.try_into() {
            Ok(event) => dtls_tx
                .send(ListenEvent::Msg { event, addr })
                .expect("channel closed"),
            Err(e) => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer Lan Mouse
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable event from {addr}: {e}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    let mut conns = conns.lock().await;
    let index = conns
        .iter()
        .position(|(a, _)| *a == addr)
        .expect("connection not found");
    conns.remove(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    #[test]
    fn ipv4_mapped_addresses_are_canonicalized() {
        let mapped: SocketAddr = "[::ffff:192.168.1.5]:4242".parse().unwrap();
        assert_eq!(
            canonical_addr(mapped),
            "192.168.1.5:4242".parse::<SocketAddr>().unwrap()
        );
        let v6: SocketAddr = "[fd00::1]:4242".parse().unwrap();
        assert_eq!(canonical_addr(v6), v6);
        let v4: SocketAddr = "10.0.0.1:4242".parse().unwrap();
        assert_eq!(canonical_addr(v4), v4);
    }

    fn cert() -> Certificate {
        Certificate::generate_self_signed(vec!["ignored".to_owned()]).unwrap()
    }

    async fn dtls_connect(local: &str, target: SocketAddr) -> DTLSConn {
        let sock = Arc::new(UdpSocket::bind(local).await.unwrap());
        sock.connect(target).await.unwrap();
        let config = Config {
            certificates: vec![cert()],
            server_name: "ignored".to_owned(),
            insecure_skip_verify: true,
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        };
        DTLSConn::new(sock, config, true, None).await.unwrap()
    }

    /// accepts DTLS connections over both IPv4 and IPv6 on the same port
    #[tokio::test]
    async fn dual_stack_accepts_ipv4_and_ipv6() {
        let cfg = Config {
            certificates: vec![cert()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            insecure_skip_verify: true,
            ..Default::default()
        };
        let listener = listen_dual_stack(0, cfg).await.unwrap();
        let port = listener.addr().await.unwrap().port();

        let mut targets = vec![("127.0.0.1:0", SocketAddr::from((Ipv4Addr::LOCALHOST, port)))];
        // some environments (e.g. containers) have no IPv6 support at all
        if std::net::UdpSocket::bind("[::1]:0").is_ok() {
            targets.push(("[::1]:0", SocketAddr::from((Ipv6Addr::LOCALHOST, port))));
        } else {
            eprintln!("IPv6 unavailable, only testing IPv4");
        }
        for (local, target) in targets {
            let (accepted, client) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(5), listener.accept()),
                dtls_connect(local, target),
            );
            let (conn, addr) = accepted.expect("timeout").expect("accept");
            assert_eq!(addr.ip(), target.ip(), "peer address must be canonical");
            assert_eq!(addr.port(), client.local_addr().unwrap().port());

            client.send(b"hello").await.unwrap();
            let mut buf = [0u8; 16];
            let n = conn.recv(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"hello");
        }
        listener.close().await.unwrap();
    }
}
