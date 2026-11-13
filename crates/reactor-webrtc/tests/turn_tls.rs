//! TURN over TLS asks the connection's certificate verifier.
//!
//! libwebrtc checks a `turns:` server against its built-in roots, which do not
//! know the test CA here (nor ISRG, which signs Let's Encrypt). The test runs a
//! local TLS server with a certificate from its own CA and gathers relay
//! candidates against it:
//!
//! - a verifier that trusts the CA gets the server chain, libwebrtc finishes the
//!   handshake, and its TURN client sends an Allocate request;
//! - a verifier that refuses, and no verifier at all, both fail the handshake.
//!
//! ```sh
//! REACTOR_WEBRTC_LIB_DIR=... cargo test -p reactor-webrtc --test turn_tls
//! ```
#![cfg(have_libwebrtc)]

use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use reactor_webrtc::{
    IceServer, IceTransportsType, PeerConnectionFactory, PeerConnectionObserver, RtcConfiguration,
    TlsCertVerifier, WebPkiVerifier,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection};

/// STUN message type of an Allocate request (RFC 8656).
const ALLOCATE_REQUEST: u16 = 0x0003;

/// A test CA and a `localhost` certificate it signed.
struct TestPki {
    ca: CertificateDer<'static>,
    leaf: CertificateDer<'static>,
    leaf_key: Vec<u8>,
}

impl TestPki {
    fn new() -> Self {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "reactor-webrtc test CA");
        let ca_key = KeyPair::generate().expect("CA key");
        let ca = ca_params.self_signed(&ca_key).expect("CA certificate");

        let mut leaf_params =
            CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf = leaf_params
            .signed_by(&leaf_key, &ca, &ca_key)
            .expect("leaf certificate");

        Self {
            ca: ca.der().clone(),
            leaf: leaf.der().clone(),
            leaf_key: leaf_key.serialize_der(),
        }
    }

    fn server_config(&self) -> Arc<ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![self.leaf.clone(), self.ca.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key.clone())),
            )
            .expect("server certificate");
        Arc::new(config)
    }
}

/// What the server saw on one TLS connection.
#[derive(Debug)]
struct Seen {
    handshake_ok: bool,
    /// The STUN message type of the first message, after a handshake.
    first_message: Option<u16>,
}

/// A TLS server on a free loopback port that reports each connection.
///
/// It listens on 127.0.0.1 and, where the host has IPv6, on [::1] with the same
/// port, so `localhost` reaches it whichever address it resolves to. Each test
/// case starts its own server: a connection that reports late goes to the
/// channel of its own case, which nobody reads any more.
struct TlsServer {
    port: u16,
    seen: Receiver<Seen>,
    stop: Arc<AtomicBool>,
    addrs: Vec<SocketAddr>,
    acceptors: Vec<thread::JoinHandle<()>>,
}

impl TlsServer {
    fn start(config: Arc<ServerConfig>) -> Self {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind IPv4");
        let port = v4.local_addr().expect("address").port();
        let mut listeners = vec![v4];
        if let Ok(v6) = TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
            listeners.push(v6);
        }

        let (tx, seen) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut addrs = Vec::new();
        let mut acceptors = Vec::new();
        for listener in listeners {
            addrs.push(listener.local_addr().expect("address"));
            let (config, tx, stop) = (config.clone(), tx.clone(), stop.clone());
            acceptors.push(thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let (config, tx) = (config.clone(), tx.clone());
                        thread::spawn(move || serve(stream, config, tx));
                    }
                }
            }));
        }
        Self {
            port,
            seen,
            stop,
            addrs,
            acceptors,
        }
    }

    /// The first connection reported within `timeout`.
    fn next(&self, timeout: Duration) -> Option<Seen> {
        self.seen.recv_timeout(timeout).ok()
    }
}

impl Drop for TlsServer {
    /// Stop accepting: set the flag, then connect once to each listener so that
    /// its blocked `accept` returns and sees it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for addr in &self.addrs {
            let _ = TcpStream::connect_timeout(addr, Duration::from_secs(1));
        }
        for acceptor in self.acceptors.drain(..) {
            let _ = acceptor.join();
        }
    }
}

fn serve(mut stream: TcpStream, config: Arc<ServerConfig>, tx: Sender<Seen>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut conn = ServerConnection::new(config).expect("server connection");
    while conn.is_handshaking() {
        if conn.complete_io(&mut stream).is_err() {
            let _ = tx.send(Seen {
                handshake_ok: false,
                first_message: None,
            });
            return;
        }
    }
    let mut header = [0u8; 2];
    let first_message = rustls::Stream::new(&mut conn, &mut stream)
        .read_exact(&mut header)
        .ok()
        .map(|()| u16::from_be_bytes(header));
    let _ = tx.send(Seen {
        handshake_ok: true,
        first_message,
    });
}

/// Records each call and answers with `inner`, or refuses without one.
#[derive(Debug, Default)]
struct Recorder {
    inner: Option<WebPkiVerifier>,
    calls: AtomicUsize,
    leaf: Mutex<Option<Vec<u8>>>,
}

impl TlsCertVerifier for Recorder {
    fn verify_chain(&self, chain: &[&[u8]]) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.leaf.lock().unwrap() = chain.first().map(|leaf| leaf.to_vec());
        self.inner.as_ref().is_some_and(|v| v.verify_chain(chain))
    }
}

/// Gather relay candidates with `verifier` against a new server for `pki`, and
/// return the first TLS connection that server reports.
fn gather(
    factory: &PeerConnectionFactory,
    pki: &TestPki,
    verifier: Option<Arc<dyn TlsCertVerifier>>,
) -> Option<Seen> {
    let server = TlsServer::start(pki.server_config());
    let config = RtcConfiguration {
        ice_servers: vec![IceServer {
            urls: vec![format!("turns:localhost:{}?transport=tcp", server.port)],
            username: "user".into(),
            password: "secret".into(),
        }],
        ice_transport_type: IceTransportsType::Relay,
        tls_cert_verifier: verifier,
        ..Default::default()
    };
    let pc = factory
        .create_peer_connection(&config, PeerConnectionObserver::new())
        .expect("peer connection");
    pc.create_data_channel("probe").expect("data channel");
    let offer = pc.create_offer().expect("offer");
    pc.set_local_description(&offer).expect("local description");

    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = server.next(deadline.saturating_duration_since(Instant::now()));
    drop(pc);
    drop(server);
    seen
}

/// Every case runs on one factory: libwebrtc starts process-global threads on
/// factory creation, so repeated factory churn in one process is unsafe.
#[test]
fn turns_asks_the_connection_verifier() {
    let pki = TestPki::new();
    let factory = PeerConnectionFactory::builder().build().expect("factory");

    // A verifier that trusts the test CA.
    let trusting = Arc::new(Recorder {
        inner: Some(WebPkiVerifier::from_der_roots([pki.ca.as_ref()]).expect("CA root")),
        ..Default::default()
    });
    let seen = gather(&factory, &pki, Some(trusting.clone())).expect("a TLS connection");
    assert!(seen.handshake_ok, "the trusted handshake failed: {seen:?}");
    assert_eq!(seen.first_message, Some(ALLOCATE_REQUEST), "{seen:?}");
    assert!(trusting.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(
        trusting.leaf.lock().unwrap().as_deref(),
        Some(pki.leaf.as_ref()),
        "the verifier did not get the server leaf first"
    );

    // A verifier that refuses.
    let refusing = Arc::new(Recorder::default());
    let seen = gather(&factory, &pki, Some(refusing.clone())).expect("a TLS connection");
    assert!(!seen.handshake_ok, "a refused chain finished the handshake");
    assert!(refusing.calls.load(Ordering::SeqCst) > 0);

    // No verifier: the built-in roots do not know the test CA.
    let seen = gather(&factory, &pki, None).expect("a TLS connection");
    assert!(
        !seen.handshake_ok,
        "the built-in roots accepted the test CA"
    );
}
