//! The session-level SDP attribute both peers declare.
//!
//! ```text
//! a=x-reactor-dc-chunking:1 max-message-size=67108864
//! ```
//!
//! The prefix has the same shape as frame metadata's
//! `a=x-reactor-frame-metadata:<version>`. After the version comes a parameter
//! list in the style of `a=fmtp`: `key=value` pairs separated by `;`.
//!
//! - `max-message-size` is the advertising side's receive limit. A sender's
//!   effective limit is the smaller of its own and the peer's value
//!   ([`effective_max_message_size`]); a missing parameter means
//!   [`LEGACY_MAX_MESSAGE_SIZE`](crate::LEGACY_MAX_MESSAGE_SIZE).
//! - Unknown keys are ignored, so a later version can add parameters without
//!   bumping the version. A line this build cannot parse counts as not
//!   declared, which leaves both peers on the plain path.
//!
//! libwebrtc drops `a=` lines it does not recognise when it parses a
//! description, so the attribute is only ever read from the SDP string.

/// Attribute name, without the `a=` prefix or the `:` separator.
pub const DC_CHUNKING_ATTRIBUTE: &str = "x-reactor-dc-chunking";
/// The wire-format version this build speaks.
pub const DC_CHUNKING_VERSION: u32 = 1;
/// The receive-limit parameter.
pub const MAX_MESSAGE_SIZE_PARAM: &str = "max-message-size";

/// What one side declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub version: u32,
    /// This side's receive limit; `None` when the parameter is absent.
    pub max_message_size: Option<u64>,
}

impl Params {
    /// The parameters this build declares, advertising `max_message_size`.
    pub fn local(max_message_size: u64) -> Self {
        Self {
            version: DC_CHUNKING_VERSION,
            max_message_size: Some(max_message_size),
        }
    }

    fn line(&self) -> String {
        match self.max_message_size {
            Some(n) => format!(
                "a={DC_CHUNKING_ATTRIBUTE}:{} {MAX_MESSAGE_SIZE_PARAM}={n}",
                self.version
            ),
            None => format!("a={DC_CHUNKING_ATTRIBUTE}:{}", self.version),
        }
    }
}

/// The largest message a sender may send to a peer that declared `remote`.
pub fn effective_max_message_size(local: u64, remote: &Params) -> u64 {
    local.min(
        remote
            .max_message_size
            .unwrap_or(crate::LEGACY_MAX_MESSAGE_SIZE),
    )
}

/// Whether `sdp` carries the attribute at all, whatever its contents.
pub fn has_attribute(sdp: &str) -> bool {
    sdp.lines()
        .any(|l| l.starts_with(&format!("a={DC_CHUNKING_ATTRIBUTE}:")))
}

/// Return `sdp` declaring `local` at session level.
///
/// The line goes immediately before the first `m=` line, which is the end of
/// the session section (RFC 8866 §5), or at the end of a description with no
/// media section. Idempotent: a description that already carries the
/// attribute comes back unchanged, as does one with no lines at all.
pub fn declare(sdp: &str, local: &Params) -> String {
    if has_attribute(sdp) || sdp.lines().next().is_none() {
        return sdp.to_owned();
    }
    let declaration = format!("{}\r\n", local.line());
    let mut out = String::with_capacity(sdp.len() + declaration.len());
    let mut inserted = false;
    for line in sdp.lines() {
        if !inserted && line.starts_with("m=") {
            out.push_str(&declaration);
            inserted = true;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !inserted {
        out.push_str(&declaration);
    }
    out
}

/// The peer's parameters, when `sdp` declares a version this build speaks.
///
/// `None` when the attribute is absent, names another version, or does not
/// parse: in every one of those cases the connection stays on the plain path.
pub fn parse(sdp: &str) -> Option<Params> {
    let prefix = format!("a={DC_CHUNKING_ATTRIBUTE}:");
    let value = sdp
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))?
        .trim();
    let (version, params) = match value.split_once(char::is_whitespace) {
        Some((v, rest)) => (v, rest.trim()),
        None => (value, ""),
    };
    let version: u32 = version.parse().ok()?;
    if version != DC_CHUNKING_VERSION {
        return None;
    }
    let mut max_message_size = None;
    for pair in params.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, val) = pair.split_once('=')?;
        if key.trim() == MAX_MESSAGE_SIZE_PARAM {
            max_message_size = Some(val.trim().parse().ok()?);
        }
        // Unknown keys are ignored on purpose.
    }
    Some(Params {
        version,
        max_message_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\n";

    #[test]
    fn declares_at_session_level_before_the_first_media_section() {
        let out = declare(OFFER, &Params::local(1024));
        let at = |needle: &str| out.find(needle).unwrap();
        assert!(at("t=0 0") < at("a=x-reactor-dc-chunking:1 max-message-size=1024"));
        assert!(at("a=x-reactor-dc-chunking:") < at("m=application"));
    }

    #[test]
    fn declare_is_idempotent() {
        let once = declare(OFFER, &Params::local(1024));
        assert_eq!(declare(&once, &Params::local(2048)), once);
        assert_eq!(declare("", &Params::local(1024)), "");
    }

    #[test]
    fn declares_at_the_end_without_a_media_section() {
        let out = declare("v=0\r\ns=-\r\n", &Params::local(7));
        assert!(out.ends_with("a=x-reactor-dc-chunking:1 max-message-size=7\r\n"));
    }

    #[test]
    fn parses_what_it_declares() {
        let out = declare(OFFER, &Params::local(67108864));
        assert_eq!(parse(&out), Some(Params::local(67108864)));
    }

    #[test]
    fn absent_attribute_means_not_declared() {
        assert_eq!(parse(OFFER), None);
    }

    #[test]
    fn missing_size_parameter_parses_as_none_and_limits_to_legacy() {
        let p = parse("a=x-reactor-dc-chunking:1\r\n").unwrap();
        assert_eq!(p.max_message_size, None);
        assert_eq!(
            effective_max_message_size(u64::MAX, &p),
            crate::LEGACY_MAX_MESSAGE_SIZE
        );
    }

    #[test]
    fn effective_limit_is_the_smaller_side() {
        let remote = Params::local(1000);
        assert_eq!(effective_max_message_size(500, &remote), 500);
        assert_eq!(effective_max_message_size(5000, &remote), 1000);
    }

    #[test]
    fn ignores_unknown_parameters() {
        let p = parse("a=x-reactor-dc-chunking:1 future-key=abc;max-message-size=42; other=1\r\n")
            .unwrap();
        assert_eq!(p.max_message_size, Some(42));
    }

    #[test]
    fn other_versions_and_malformed_lines_count_as_not_declared() {
        for line in [
            "a=x-reactor-dc-chunking:2 max-message-size=42",
            "a=x-reactor-dc-chunking:one",
            "a=x-reactor-dc-chunking:1 max-message-size=lots",
            "a=x-reactor-dc-chunking:1 max-message-size",
            "a=x-reactor-dc-chunking:",
        ] {
            assert_eq!(parse(&format!("{line}\r\n")), None, "{line}");
        }
    }
}
