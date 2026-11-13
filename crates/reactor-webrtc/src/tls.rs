//! Which TURN-over-TLS servers a connection trusts.
//!
//! libwebrtc checks the certificate chain of a `turns:` server against a root
//! list compiled into it (`rtc_base/ssl_roots.h`). That list comes from Google's
//! small `pki.goog/roots.pem` of May 2023 and has no ISRG root, so it refuses
//! every Let's Encrypt certificate. When libwebrtc's own check fails, it asks
//! the connection's [`TlsCertVerifier`] instead.
//! [`RtcConfiguration`](crate::RtcConfiguration) sets one by default:
//! [`WebPkiVerifier::mozilla`], the Mozilla root store of the `webpki-roots`
//! crate.
//!
//! A verifier decides only whether the chain leads to a trusted root for TLS
//! server use. libwebrtc checks the host name itself afterwards.

use std::borrow::Cow;
use std::fmt;
use std::os::raw::{c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use rustls_pki_types::{CertificateDer, TrustAnchor, UnixTime};
use webpki::{EndEntityCert, KeyUsage, ALL_VERIFICATION_ALGS};

use crate::{Error, Result};

/// Decides whether the certificate chain of a TURN-over-TLS server is trusted.
///
/// libwebrtc calls it on its network thread, during the TLS handshake, so it
/// must not block. It runs only when libwebrtc's built-in roots do not verify
/// the chain.
pub trait TlsCertVerifier: fmt::Debug + Send + Sync {
    /// `chain` holds DER certificates as the server sent them: the leaf first,
    /// then the intermediates. Return `true` when the chain leads to a trusted
    /// root and the leaf may serve TLS.
    fn verify_chain(&self, chain: &[&[u8]]) -> bool;
}

/// A [`TlsCertVerifier`] that checks the chain with `rustls-webpki`.
#[derive(Clone)]
pub struct WebPkiVerifier {
    roots: Cow<'static, [TrustAnchor<'static>]>,
}

impl fmt::Debug for WebPkiVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebPkiVerifier")
            .field("roots", &self.roots.len())
            .finish()
    }
}

impl WebPkiVerifier {
    /// Trust the Mozilla root store, as the `webpki-roots` crate ships it.
    pub fn mozilla() -> Self {
        Self {
            roots: Cow::Borrowed(webpki_roots::TLS_SERVER_ROOTS),
        }
    }

    /// Trust the given DER root certificates, for example the platform store
    /// from `rustls-native-certs`.
    pub fn from_der_roots<I, C>(roots: I) -> Result<Self>
    where
        I: IntoIterator<Item = C>,
        C: AsRef<[u8]>,
    {
        let roots = roots
            .into_iter()
            .map(|der| {
                webpki::anchor_from_trusted_cert(&CertificateDer::from(der.as_ref()))
                    .map(|anchor| anchor.to_owned())
                    .map_err(|e| Error::Webrtc(format!("TLS root certificate: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        if roots.is_empty() {
            return Err(Error::Webrtc("no TLS root certificate".into()));
        }
        Ok(Self {
            roots: Cow::Owned(roots),
        })
    }

    /// Verify `chain` at `time` for TLS server use.
    fn verify_at(&self, chain: &[&[u8]], time: UnixTime) -> std::result::Result<(), webpki::Error> {
        let Some((leaf, rest)) = chain.split_first() else {
            return Err(webpki::Error::BadDer);
        };
        let leaf = CertificateDer::from(*leaf);
        let intermediates: Vec<CertificateDer<'_>> =
            rest.iter().map(|der| CertificateDer::from(*der)).collect();
        EndEntityCert::try_from(&leaf)?.verify_for_usage(
            ALL_VERIFICATION_ALGS,
            &self.roots,
            &intermediates,
            time,
            KeyUsage::server_auth(),
            None,
            None,
        )?;
        Ok(())
    }
}

impl TlsCertVerifier for WebPkiVerifier {
    fn verify_chain(&self, chain: &[&[u8]]) -> bool {
        self.verify_at(chain, UnixTime::now()).is_ok()
    }
}

/// What the glue holds as `tls_userdata`: one strong reference to the verifier.
type Shared = Arc<dyn TlsCertVerifier>;

/// Box one reference to `verifier` for the glue, which releases it through
/// [`tls_release`].
pub(crate) fn into_userdata(verifier: &Shared) -> *mut c_void {
    Box::into_raw(Box::new(Arc::clone(verifier))).cast()
}

/// The glue's `tls_verify`: rebuild the chain and ask the verifier.
pub(crate) extern "C" fn tls_verify(
    userdata: *mut c_void,
    certs: *const *const u8,
    lens: *const usize,
    count: usize,
) -> c_int {
    if userdata.is_null() || certs.is_null() || lens.is_null() || count == 0 {
        return 0;
    }
    // SAFETY: `userdata` comes from `into_userdata` and stays alive until the
    // glue calls `tls_release`. The glue passes `count` valid pointers and
    // lengths that it keeps alive for this call.
    let verifier = unsafe { &*userdata.cast::<Shared>() };
    let chain: Vec<&[u8]> = (0..count)
        .map(|i| unsafe { std::slice::from_raw_parts(*certs.add(i), *lens.add(i)) })
        .collect();
    // A panic must not cross into C++: treat it as a refusal.
    catch_unwind(AssertUnwindSafe(|| verifier.verify_chain(&chain))).unwrap_or(false) as c_int
}

/// The glue's `tls_release`: drop the reference from `into_userdata`.
pub(crate) extern "C" fn tls_release(userdata: *mut c_void) {
    if !userdata.is_null() {
        // SAFETY: the glue calls this exactly once per `into_userdata`.
        drop(unsafe { Box::from_raw(userdata.cast::<Shared>()) });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    // The chain of turn.sa-east-1.aws.dev.rea.live as coturn sends it: the
    // Let's Encrypt leaf, the YR2 intermediate, and ISRG Root YR cross-signed
    // by ISRG Root X1. The leaf is valid from 2026-10-18 to 2027-01-16.
    const LEAF: &[u8] = include_bytes!("../tests/fixtures/turn-le/leaf.der");
    const YR2: &[u8] = include_bytes!("../tests/fixtures/turn-le/yr2.der");
    const ROOT_YR: &[u8] = include_bytes!("../tests/fixtures/turn-le/root-yr.der");

    /// 2026-11-13, inside the leaf's validity.
    fn valid_time() -> UnixTime {
        UnixTime::since_unix_epoch(Duration::from_secs(1_794_528_000))
    }

    /// 2027-02-01, after the leaf expired.
    fn expired_time() -> UnixTime {
        UnixTime::since_unix_epoch(Duration::from_secs(1_801_440_000))
    }

    #[test]
    fn mozilla_roots_accept_a_lets_encrypt_chain() {
        let verifier = WebPkiVerifier::mozilla();
        assert_eq!(
            verifier.verify_at(&[LEAF, YR2, ROOT_YR], valid_time()),
            Ok(())
        );
    }

    #[test]
    fn a_leaf_without_its_intermediates_is_refused() {
        let verifier = WebPkiVerifier::mozilla();
        assert!(verifier.verify_at(&[LEAF], valid_time()).is_err());
    }

    #[test]
    fn an_expired_leaf_is_refused() {
        let verifier = WebPkiVerifier::mozilla();
        assert!(verifier
            .verify_at(&[LEAF, YR2, ROOT_YR], expired_time())
            .is_err());
    }

    #[test]
    fn an_empty_chain_is_refused() {
        assert!(!WebPkiVerifier::mozilla().verify_chain(&[]));
    }

    #[test]
    fn custom_roots_decide_the_trust() {
        let root_yr = WebPkiVerifier::from_der_roots([ROOT_YR]).expect("root");
        assert_eq!(root_yr.verify_at(&[LEAF, YR2], valid_time()), Ok(()));

        // A root that did not issue the chain.
        let other = WebPkiVerifier::from_der_roots([LEAF]).expect("root");
        assert!(other
            .verify_at(&[LEAF, YR2, ROOT_YR], valid_time())
            .is_err());
    }

    #[test]
    fn no_roots_is_an_error() {
        assert!(WebPkiVerifier::from_der_roots(Vec::<Vec<u8>>::new()).is_err());
    }

    #[derive(Debug)]
    struct Counting {
        answer: bool,
        calls: AtomicUsize,
        leaf_len: AtomicUsize,
    }

    impl TlsCertVerifier for Counting {
        fn verify_chain(&self, chain: &[&[u8]]) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.leaf_len.store(chain[0].len(), Ordering::SeqCst);
            self.answer
        }
    }

    #[test]
    fn the_glue_callbacks_pass_the_chain_and_release_the_reference() {
        let counting = Arc::new(Counting {
            answer: true,
            calls: AtomicUsize::new(0),
            leaf_len: AtomicUsize::new(0),
        });
        let shared: Shared = counting.clone();
        let userdata = into_userdata(&shared);
        assert_eq!(Arc::strong_count(&counting), 3);

        let certs = [LEAF.as_ptr(), YR2.as_ptr()];
        let lens = [LEAF.len(), YR2.len()];
        assert_eq!(tls_verify(userdata, certs.as_ptr(), lens.as_ptr(), 2), 1);
        assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
        assert_eq!(counting.leaf_len.load(Ordering::SeqCst), LEAF.len());
        assert_eq!(tls_verify(userdata, certs.as_ptr(), lens.as_ptr(), 0), 0);

        tls_release(userdata);
        assert_eq!(Arc::strong_count(&counting), 2);
    }

    #[derive(Debug)]
    struct Panicking;

    impl TlsCertVerifier for Panicking {
        fn verify_chain(&self, _chain: &[&[u8]]) -> bool {
            panic!("verifier bug");
        }
    }

    #[test]
    fn a_panicking_verifier_refuses_the_chain() {
        let shared: Shared = Arc::new(Panicking);
        let userdata = into_userdata(&shared);
        let certs = [LEAF.as_ptr()];
        let lens = [LEAF.len()];
        assert_eq!(tls_verify(userdata, certs.as_ptr(), lens.as_ptr(), 1), 0);
        tls_release(userdata);
    }
}
