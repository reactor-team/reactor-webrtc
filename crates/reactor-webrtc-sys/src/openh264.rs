//! Runtime download, verification, and caching of Cisco's prebuilt OpenH264
//! shared library — the software H.264 codec for platforms with no OS-level
//! hardware codec reachable from this native API (Linux, Windows).
//!
//! This deliberately never compiles OpenH264 from source: Cisco's own
//! arrangement with the MPEG LA AVC patent pool covers *only* Cisco's own
//! compiled binary module, obtained from Cisco and used unmodified — not a
//! build compiled from OpenH264's source by anyone else (confirmed against
//! `openh264.org/BINARY_LICENSE.txt` and its FAQ). So this module downloads
//! Cisco's official prebuilt library the same way Firefox/Chrome do, verifies
//! it against a hash pinned in this source (not the CDN's own
//! `.signed.md5.txt` sidecar — that would just be trusting the same CDN
//! response twice), and hands the resulting file path to the C++ glue, which
//! `dlopen`s it.
//!
//! **License obligation on the integrating application**: per Cisco's binary
//! license, [`OPENH264_ATTRIBUTION`] must be shown in the app's
//! licensing/EULA surface, in the same place other licensing notices are
//! presented to the user. This module cannot enforce that — only remind you.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Pinned OpenH264 release. Bump deliberately: the SHA-1 table below and the
/// vendored `glue/openh264/codec_api.h` ABI declarations move together with
/// this version — OpenH264's ABI is stable within its public C API but the
/// hash obviously isn't.
const OPENH264_VERSION: &str = "2.6.0";

const CISCO_CDN_BASE: &str = "http://ciscobinary.openh264.org";

/// Required attribution text — Cisco's binary license conditions the royalty
/// carve-out on this being shown in the integrating app's licensing surface.
pub const OPENH264_ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Failure modes for [`ensure_available`]. Never panics on any of these —
/// this runs inside a consumer's production application.
#[derive(Debug)]
pub enum OpenH264Error {
    /// No known Cisco prebuilt for this `(os, arch)`.
    UnsupportedPlatform { os: String, arch: String },
    /// The expected-hash table has no entry pinned for this build yet (should
    /// not happen for a released build — see the doc comment on `Target::sha1`).
    MissingExpectedHash { file: &'static str },
    /// Could not resolve a cache directory to store the library in.
    NoCacheDir,
    /// The HTTP download failed.
    Download(String),
    /// The downloaded bytes didn't match the pinned SHA-1.
    HashMismatch { expected: String, actual: String },
    /// bzip2 decompression of the downloaded archive failed.
    Decompress(String),
    /// Filesystem I/O (cache dir creation, write, permissions) failed.
    Io(io::Error),
}

impl fmt::Display for OpenH264Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { os, arch } => {
                write!(f, "no OpenH264 prebuilt for {os}/{arch}")
            }
            Self::MissingExpectedHash { file } => {
                write!(
                    f,
                    "no pinned SHA-1 for {file} — see openh264.rs target_for()"
                )
            }
            Self::NoCacheDir => write!(f, "could not resolve a cache directory"),
            Self::Download(msg) => write!(f, "OpenH264 download failed: {msg}"),
            Self::HashMismatch { expected, actual } => write!(
                f,
                "OpenH264 download hash mismatch: expected {expected}, got {actual}"
            ),
            Self::Decompress(msg) => write!(f, "OpenH264 decompress failed: {msg}"),
            Self::Io(e) => write!(f, "OpenH264 cache I/O failed: {e}"),
        }
    }
}

impl std::error::Error for OpenH264Error {}

impl From<io::Error> for OpenH264Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

struct Target {
    /// Exact filename Cisco publishes under `CISCO_CDN_BASE`, e.g.
    /// `libopenh264-2.6.0-linux64.8.so.bz2`.
    archive_name: &'static str,
    /// Expected SHA-1 of the *compressed* `.bz2`, lowercase hex — computed by
    /// us from a verified download, not copied from a Cisco-published SHA-1
    /// (Cisco doesn't publish one).
    ///
    /// Cisco's actual sidecar is `<archive_name>.signed.md5.txt` (e.g.
    /// `libopenh264-2.6.0-linux64.8.so.signed.md5.txt`), an MD5 digest inside
    /// a signed blob — not `.sha1` as an earlier version of this comment
    /// assumed. We used that sidecar only as a sanity check on the download,
    /// then hashed the verified `.bz2` ourselves with SHA-1, so verification
    /// here doesn't depend on Cisco ever publishing SHA-1/SHA-256.
    ///
    /// `https://ciscobinary.openh264.org` 403s on some networks (looked like
    /// datacenter-IP filtering on the HTTPS vhost specifically); plain
    /// `http://` — what `CISCO_CDN_BASE` already uses — is reachable. To
    /// re-pin on a version bump: fetch `<archive_name>.signed.md5.txt`,
    /// confirm it matches `md5 <archive_name>.bz2`, then
    /// `shasum -a 1 <archive_name>.bz2`.
    sha1: &'static str,
}

/// `(os, arch)` → Cisco's published artifact, for `std::env::consts::OS`/`ARCH`
/// spelling. Linux/Windows only — Apple and Android get H.264 through the
/// OS's own hardware codec instead (VideoToolbox / MediaCodec), not OpenH264.
fn target_for(os: &str, arch: &str) -> Result<Target, OpenH264Error> {
    let t = match (os, arch) {
        ("linux", "x86_64") => Target {
            archive_name: "libopenh264-2.6.0-linux64.8.so.bz2",
            sha1: "1ae7464e33a249c1cd0b6998d09f9ada5937b64d",
        },
        ("linux", "aarch64") => Target {
            archive_name: "libopenh264-2.6.0-linux-arm64.8.so.bz2",
            sha1: "16727ec3b37dbea3c8616418ba0c4bc87e0eb4aa",
        },
        ("windows", "x86_64") => Target {
            archive_name: "openh264-2.6.0-win64.dll.bz2",
            sha1: "78661d7bf890e3c526acae6904679afd05941fbe",
        },
        ("windows", "aarch64") => Target {
            archive_name: "openh264-2.6.0-win-arm64.dll.bz2",
            sha1: "34c17127d00ea0be6f3b96b8e91c667357bef800",
        },
        _ => {
            return Err(OpenH264Error::UnsupportedPlatform {
                os: os.to_string(),
                arch: arch.to_string(),
            })
        }
    };
    Ok(t)
}

fn cache_paths(cache_root: &Path, lib_filename: &str) -> (PathBuf, PathBuf) {
    let lib_path = cache_root.join(lib_filename);
    let sentinel = cache_root.join(format!("{lib_filename}.ok"));
    (lib_path, sentinel)
}

/// Ensure Cisco's OpenH264 shared library for the current platform is
/// present locally, downloading and verifying it on first use. Returns the
/// path to the (already `dlopen`-able) library — pass it to
/// `PeerConnectionFactory::with_openh264`.
///
/// `cache_dir` overrides where the library is stored; `None` uses
/// [`dirs::cache_dir`] (e.g. `~/Library/Caches` on macOS, `~/.cache` on
/// Linux, `%LOCALAPPDATA%` on Windows) under `reactor-webrtc/openh264/<version>/`.
///
/// This is a blocking network call the first time it runs on a given
/// machine; subsequent calls are a cache hit (a present file + a `.ok`
/// sentinel written after a successful verify) and touch no network. Call it
/// explicitly — e.g. at app startup, with your own progress UI — rather than
/// relying on it happening implicitly inside factory construction.
pub fn ensure_available(cache_dir: Option<&Path>) -> Result<PathBuf, OpenH264Error> {
    let target = target_for(std::env::consts::OS, std::env::consts::ARCH)?;
    if target.sha1.is_empty() {
        return Err(OpenH264Error::MissingExpectedHash {
            file: target.archive_name,
        });
    }

    let root = match cache_dir {
        Some(p) => p.to_path_buf(),
        None => dirs::cache_dir().ok_or(OpenH264Error::NoCacheDir)?,
    }
    .join("reactor-webrtc")
    .join("openh264")
    .join(OPENH264_VERSION);
    fs::create_dir_all(&root)?;

    let lib_filename = target
        .archive_name
        .strip_suffix(".bz2")
        .unwrap_or(target.archive_name);
    let (lib_path, sentinel) = cache_paths(&root, lib_filename);

    if lib_path.is_file() && sentinel.is_file() {
        return Ok(lib_path);
    }

    let url = format!("{CISCO_CDN_BASE}/{}", target.archive_name);
    let compressed = download(&url)?;
    verify_sha1(&compressed, target.sha1)?;
    let decompressed = decompress_bz2(&compressed)?;

    fs::write(&lib_path, &decompressed)?;
    // Best-effort: a noexec-mounted cache dir would otherwise block dlopen.
    // Not fatal on its own — if dlopen still fails the C++ side reports that.
    let _ = mark_executable(&lib_path);
    fs::write(&sentinel, b"ok")?;

    Ok(lib_path)
}

fn download(url: &str) -> Result<Vec<u8>, OpenH264Error> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| OpenH264Error::Download(e.to_string()))?;
    let mut buf = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| OpenH264Error::Download(e.to_string()))?;
    Ok(buf)
}

fn verify_sha1(data: &[u8], expected_hex: &str) -> Result<(), OpenH264Error> {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(data);
    let actual_hex = hex_encode(&hasher.finalize());
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(OpenH264Error::HashMismatch {
            expected: expected_hex.to_string(),
            actual: actual_hex,
        });
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn decompress_bz2(data: &[u8]) -> Result<Vec<u8>, OpenH264Error> {
    use bzip2::read::BzDecoder;
    let mut out = Vec::new();
    BzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| OpenH264Error::Decompress(e.to_string()))?;
    Ok(out)
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_in_scope_platform() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let t = target_for(os, arch).unwrap_or_else(|e| panic!("{os}/{arch}: {e}"));
            assert!(t.archive_name.ends_with(".bz2"));
        }
    }

    #[test]
    fn rejects_platforms_handled_by_os_hardware_codecs() {
        // macOS/iOS (VideoToolbox) and Android (MediaCodec) get H.264 via the
        // OS's own hardware codec, not OpenH264 — see Part 2 of the plan.
        for (os, arch) in [("macos", "aarch64"), ("android", "aarch64")] {
            assert!(matches!(
                target_for(os, arch),
                Err(OpenH264Error::UnsupportedPlatform { .. })
            ));
        }
    }

    #[test]
    fn cache_paths_are_sibling_files() {
        let (lib, sentinel) = cache_paths(Path::new("/tmp/x"), "libopenh264.so");
        assert_eq!(lib, Path::new("/tmp/x/libopenh264.so"));
        assert_eq!(sentinel, Path::new("/tmp/x/libopenh264.so.ok"));
    }

    #[test]
    fn sha1_hex_matches_known_vector() {
        // echo -n "abc" | sha1sum -> a9993e364706816aba3e25717850c26c9cd0d89d
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(b"abc");
        assert_eq!(
            hex_encode(&hasher.finalize()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn verify_sha1_rejects_mismatch() {
        let err = verify_sha1(b"abc", "0000000000000000000000000000000000000a").unwrap_err();
        assert!(matches!(err, OpenH264Error::HashMismatch { .. }));
    }

    #[test]
    fn verify_sha1_accepts_match_case_insensitively() {
        verify_sha1(b"abc", "A9993E364706816ABA3E25717850C26C9CD0D89D").unwrap();
    }

    #[test]
    fn all_in_scope_targets_have_pinned_hashes() {
        // `ensure_available` refuses to download with a missing hash (see
        // `OpenH264Error::MissingExpectedHash`) rather than skip
        // verification — confirm every in-scope target is actually pinned,
        // not left as the empty placeholder this table started from.
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            let t = target_for(os, arch).unwrap_or_else(|e| panic!("{os}/{arch}: {e}"));
            assert_eq!(t.sha1.len(), 40, "{os}/{arch}: sha1 not pinned");
            assert!(
                t.sha1.chars().all(|c| c.is_ascii_hexdigit()),
                "{os}/{arch}: sha1 is not hex"
            );
        }
    }
}
