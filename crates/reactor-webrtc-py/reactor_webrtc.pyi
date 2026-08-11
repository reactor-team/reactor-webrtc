"""Type stubs for reactor_webrtc — WebRTC primitives for Python."""

from __future__ import annotations

from typing import Callable, Optional, Union

# ── Config ────────────────────────────────────────────────────────────────────

class IceServer:
    urls: list[str]
    username: str
    password: str
    def __init__(
        self,
        urls: list[str] = ...,
        username: str = ...,
        password: str = ...,
    ) -> None: ...

class BundlePolicy:
    """Controls how media tracks are bundled onto ICE transports.

    `Balanced` (default): one BUNDLE group per media type.
    `MaxBundle`: all tracks share a single transport — recommended for streaming.
    `MaxCompat`: one transport per track; maximum compatibility with legacy endpoints."""

    Balanced: BundlePolicy
    MaxBundle: BundlePolicy
    MaxCompat: BundlePolicy

class TcpCandidatePolicy:
    """Whether TCP ICE candidates are gathered.

    `Disabled` (default): UDP only.
    `Enabled`: also collect TCP candidates (useful when UDP is blocked)."""

    Disabled: TcpCandidatePolicy
    Enabled: TcpCandidatePolicy

class RtcConfiguration:
    ice_servers: list[IceServer]
    ice_transport_type: str  # "all" | "relay" | "no_host" | "none"
    continual_gathering_policy: str  # "once" | "continually"
    min_port: int  # 0 = use OS default
    max_port: int  # 0 = use OS default
    bundle_policy: BundlePolicy
    ice_connection_receiving_timeout_ms: int  # 0 = libwebrtc default (~30 000 ms)
    ice_check_interval_strong_connectivity_ms: int  # 0 = libwebrtc default
    tcp_candidate_policy: TcpCandidatePolicy
    frame_metadata: bool
    """Whether this connection takes part in per-frame metadata.

    `True` by default. When on, offers advertise the capability, answers
    mirror an offer that asked for it, and the metadata steps are wired into the
    video transceivers once the peer agrees.

    Set `False` to keep the capability out of the SDP entirely: offers do not
    advertise it, answers stay silent even when the offer declared it, the gate
    never opens, and `user_data` passed to a push is dropped. Nothing about the
    connection differs from one built before the capability existed.

    Reasons to: a peer whose encoded payloads must be byte-identical to what the
    encoder produced, a deployment that has not rolled the capability out to both
    ends yet, or ruling frame metadata out while bisecting something else."""
    def __init__(
        self,
        ice_servers: list[IceServer] = ...,
        ice_transport_type: str = "all",
        continual_gathering_policy: str = "once",
        min_port: int = 0,
        max_port: int = 0,
        bundle_policy: BundlePolicy = ...,
        ice_connection_receiving_timeout_ms: int = 0,
        ice_check_interval_strong_connectivity_ms: int = 0,
        tcp_candidate_policy: TcpCandidatePolicy = ...,
        frame_metadata: bool = True,
    ) -> None: ...

# ── Signaling types ───────────────────────────────────────────────────────────

class IceCandidate:
    candidate: str
    sdp_mid: Optional[str]
    sdp_mline_index: Optional[int]
    def __init__(
        self,
        candidate: str,
        sdp_mid: Optional[str] = None,
        sdp_mline_index: Optional[int] = None,
    ) -> None: ...

class SessionDescription:
    kind: str  # "offer" | "answer" | "pranswer" | "rollback"
    sdp: str
    def __init__(self, kind: str, sdp: str) -> None: ...
    def ice_ufrags(self) -> list[str]: ...
    def with_ice_credentials(self, ufrag: str, pwd: str) -> SessionDescription: ...
    def declares_frame_metadata(self) -> bool:
        """Whether this description declares frame-metadata support.

        True when it carries a session-level `a=x-reactor-frame-metadata:<version>`
        at a version this build understands, so a peer speaking a different trailer
        format reads as unsupported rather than as a partial match.

        `set_remote_description` arms the connection's `FrameMetadataGate` from
        exactly this."""
        ...
    def with_frame_metadata(self) -> SessionDescription:
        """Return a copy declaring frame-metadata support, as a session-level
        attribute inserted before the first media section.

        `create_offer` already applies this to every offer and `create_answer`
        mirrors the offer, so callers using this library's signalling path never
        need it. Public for callers that assemble or rewrite SDP themselves.
        Idempotent."""
        ...

# ── Enums ─────────────────────────────────────────────────────────────────────

class PeerConnectionState:
    New: PeerConnectionState
    Connecting: PeerConnectionState
    Connected: PeerConnectionState
    Disconnected: PeerConnectionState
    Failed: PeerConnectionState
    Closed: PeerConnectionState

class IceGatheringState:
    New: IceGatheringState
    Gathering: IceGatheringState
    Complete: IceGatheringState

class DataChannelState:
    Connecting: DataChannelState
    Open: DataChannelState
    Closing: DataChannelState
    Closed: DataChannelState

class MediaKind:
    Audio: MediaKind
    Video: MediaKind
    Unknown: MediaKind

class TransceiverDirection:
    SendRecv: TransceiverDirection
    SendOnly: TransceiverDirection
    RecvOnly: TransceiverDirection
    Inactive: TransceiverDirection

# ── Stats ─────────────────────────────────────────────────────────────────────

class IceCandidatePairState:
    Waiting: IceCandidatePairState
    InProgress: IceCandidatePairState
    Failed: IceCandidatePairState
    Succeeded: IceCandidatePairState
    Cancelled: IceCandidatePairState

class InboundRtpStats:
    ssrc: int
    packets_received: int
    bytes_received: int
    jitter_s: float
    packets_lost: int
    nack_count: int
    total_decode_time_s: float

class OutboundRtpStats:
    ssrc: int
    packets_sent: int
    bytes_sent: int
    target_bitrate_bps: float
    round_trip_time_s: float
    retransmitted_packets_sent: int

class IceCandidatePairStats:
    current_round_trip_time_s: float
    priority: int
    state: IceCandidatePairState

class StatsReport:
    inbound_rtp: list[InboundRtpStats]
    outbound_rtp: list[OutboundRtpStats]
    candidate_pairs: list[IceCandidatePairStats]

# ── Frame metadata ────────────────────────────────────────────────────────────

FRAME_METADATA_ATTRIBUTE: str
"""The SDP attribute peers declare frame-metadata support with.

Emitted at session level as `a=x-reactor-frame-metadata:<version>`, before the
first media section. Session level because support is a property of a peer's
code, not of one of its tracks.

Unregistered, hence the `x-` prefix. RFC 8866 requires a receiver to ignore an
attribute it does not recognise, which is what makes it safe to send
unconditionally. Note that libwebrtc — and browsers — drop unrecognised `a=`
lines when parsing, so read this from the signalled SDP string rather than from
anything the stack hands back."""

FRAME_METADATA_VERSION: int
"""Wire version of the trailer format this build speaks.

A peer declaring a different version reads as unsupported: an incompatible
change to the trailer bumps this, and old and new then never agree."""

class FrameMetadataGate:
    """What the remote peer declared about frame-metadata support.

    Available from `PeerConnection.frame_metadata_gate()`. It starts closed and
    is armed by `set_remote_description` from whether that description declares
    the capability. Every renegotiation re-arms it, so a peer that drops support
    closes it again.

    It drives three things, all inside the library: `create_answer` mirrors an
    offer that declared the capability; `set_remote_description` wires the
    metadata steps into the video transceivers once it is open; and the sender
    step appends nothing while it is closed, because handing a trailer to a peer
    that will not strip it hands the extra bytes to its decoder.

    Callers do not have to consult it — pass `user_data` whenever it is
    meaningful. Reading it is useful for diagnostics, since "did this peer
    agree?" is otherwise invisible."""

    def __init__(self) -> None:
        """A closed gate, not attached to any peer connection. Useful in tests."""
        ...
    def is_open(self) -> bool:
        """Whether trailers may be appended."""
        ...

class FrameMetadata:
    """Metadata attached to a video frame via the RTP packet trailer.

    All fields are zero / empty when not set by the sender."""

    frame_id: int
    timestamp: int
    user_data: bytes
    def __init__(
        self,
        frame_id: int = 0,
        timestamp: int = 0,
        user_data: bytes = b"",
    ) -> None: ...

# ── Encoded-frame transform ───────────────────────────────────────────────────

class FrameAction:
    """What a FrameTransform callback should do with the frame."""

    Forward: FrameAction
    Drop: FrameAction

class EncodedFrame:
    """Snapshot of an encoded frame passed to a FrameTransform callback.

    Call `replace_data(new_bytes)` inside the callback to substitute the
    payload; the new bytes are forwarded when the callback returns
    `FrameAction.Forward`."""

    data: bytes
    is_key_frame: bool
    ssrc: int
    timestamp: int
    capture_time_ms: int
    def replace_data(self, new_data: bytes) -> None: ...

class FrameTransform:
    """An encoded-frame callback attached to a transceiver sender or receiver.

    Create from a Python callable with `FrameTransform(callback)`.

    This is a registration, not a native object: attaching it does not take
    libwebrtc's single frame-transformer slot. The library owns that slot and
    composes this callback with its own frame-metadata step, so encoded-frame
    access and per-frame metadata work on the same transceiver.

    Callback signature: `callback(frame: EncodedFrame) -> FrameAction`"""

    def __init__(self, callback: Callable[[EncodedFrame], FrameAction]) -> None: ...

# ── Media ─────────────────────────────────────────────────────────────────────

class Track:
    def kind(self) -> MediaKind: ...
    def push_video_frame(
        self,
        bgra: bytes,
        width: int,
        height: int,
        user_data: Optional[bytes] = None,
    ) -> None:
        """Push a raw BGRA video frame into a local video track.

        Pass `user_data` (bytes) to embed per-frame metadata in the encoded
        packet trailer. Nothing else has to be arranged: the trailer is appended
        once the peer has declared that it strips them, and `user_data` is
        silently dropped while it has not (see `FrameMetadataGate`)."""
        ...
    def on_video_frame(
        self,
        callback: Union[
            Callable[[bytes, int, int, Optional[FrameMetadata]], None],
            Callable[[bytes, int, int], None],
        ],
    ) -> None:
        """Register a callback for decoded video frames from a remote track.

        Preferred signature: `callback(bgra: bytes, width: int, height: int, metadata: FrameMetadata | None)`.
        Legacy 3-arg signature `callback(bgra, width, height)` is also accepted.

        `metadata` is `None` when no receiver transform is attached or when the
        sender did not include a trailer. When `metadata` is `None` and the
        4-arg call raises any exception, it is retried as a 3-arg call.

        Note: all-zero frames (empty jitter buffer from peers with no incoming
        RTP) are suppressed and will not trigger this callback."""
        ...
    def push_pcm(self, pcm: bytes, sample_rate: int, channels: int) -> None:
        """Push interleaved signed 16-bit little-endian PCM to a local audio track
        created with `PeerConnectionFactory.create_audio_track_with_local_source`.
        `pcm` must have even byte length and `len(pcm) // 2` must be a multiple of
        `channels`. No-op for ADM-backed or remote tracks."""
        ...
    def on_audio_frame(
        self,
        callback: Callable[[bytes, int, int, int], None],
    ) -> None:
        """Register `callback(pcm: bytes, sample_rate: int, channels: int, frames: int)`.

        Note: all-zero frames (empty jitter buffer from peers with no incoming RTP)
        are suppressed and will not trigger this callback."""
        ...

class EncodedVideoTrack:
    def push_encoded_frame(
        self,
        data: bytes,
        is_key_frame: bool = False,
        width: int = 0,
        height: int = 0,
        rtp_timestamp: int = 0,
        user_data: Optional[bytes] = None,
    ) -> None:
        """Push a compressed video frame (Annex-B H.264 or VP8/VP9).

        Pass `user_data` (bytes) to embed per-frame metadata in the encoded
        packet trailer. Nothing else has to be arranged: the trailer is appended
        once the peer has declared that it strips them, and `user_data` is
        silently dropped while it has not (see `FrameMetadataGate`)."""
        ...
    def add_to_peer_connection(self, pc: PeerConnection) -> None: ...
    def add_transceiver(
        self, pc: PeerConnection, direction: TransceiverDirection
    ) -> Transceiver: ...

class Transceiver:
    def mid(self) -> Optional[str]: ...
    def kind(self) -> MediaKind: ...
    def set_track(self, track: Union[Track, EncodedVideoTrack]) -> None:
        """Attach a local track to this transceiver's sender.

        The track joins the one MediaStream this peer publishes under, which is what
        lets the remote sync a published audio track against a published video
        track. It reaches the wire in the next offer or answer."""
    def set_direction(self, direction: TransceiverDirection) -> None: ...
    def set_sender_transform(self, transform: FrameTransform) -> None:
        """Attach a FrameTransform to the sender path of this transceiver.
        The transform runs after the encoder, before RTP packetization.

        Composes rather than replaces: the callback runs first, on the bytes the
        encoder produced, and the frame-metadata trailer is appended after. Calling
        this again replaces the callback."""
        ...
    def set_receiver_transform(self, transform: FrameTransform) -> None:
        """Attach a FrameTransform to the receiver path of this transceiver.
        The transform runs after RTP depacketization, before the decoder.

        Composes rather than replaces, as on the sender. The callback runs before
        the metadata trailer is stripped, so it sees exactly the bytes that
        arrived."""
        ...

# ── Data channel ──────────────────────────────────────────────────────────────

class DataChannel:
    def label(self) -> str: ...
    def state(self) -> DataChannelState: ...
    def buffered_amount(self) -> int: ...
    def send(self, data: bytes, binary: bool = True) -> None: ...
    def on_message(self, callback: Callable[[bytes, bool], None]) -> None: ...
    def on_state_change(self, callback: Callable[[DataChannelState], None]) -> None: ...
    def on_open(self, callback: Callable[[], None]) -> None: ...
    def on_close(self, callback: Callable[[], None]) -> None: ...

# ── Observer ──────────────────────────────────────────────────────────────────

class PeerConnectionObserver:
    on_connection_state_change: Optional[Callable[[PeerConnectionState], None]]
    on_ice_gathering_change: Optional[Callable[[IceGatheringState], None]]
    on_ice_candidate: Optional[Callable[[IceCandidate], None]]
    on_track: Optional[Callable[[MediaKind, Track], None]]
    on_data_channel: Optional[Callable[[DataChannel], None]]
    def __init__(self) -> None: ...

# ── Peer connection ───────────────────────────────────────────────────────────

class PeerConnection:
    async def create_offer(self) -> SessionDescription:
        """Create an offer.

        Every offer advertises frame-metadata support as a session-level
        `a=x-reactor-frame-metadata:<version>`, because this library supports it. A
        peer that does not understand the attribute ignores it. The declaration is
        what lets the answerer tell us it strips trailers, which is what opens this
        connection's `FrameMetadataGate`."""
        ...
    async def create_answer(self) -> SessionDescription:
        """Create an answer.

        Mirrors the offer on frame metadata: the capability is declared only when
        the offer declared it. Requires `set_remote_description` to have been
        called with the offer first, which is already the only valid order."""
        ...
    async def set_local_description(self, sdp: SessionDescription) -> None: ...
    async def set_remote_description(self, sdp: SessionDescription) -> None:
        """Apply the remote description, and arm this connection's
        `FrameMetadataGate` from it.

        The gate opens when `sdp` declares the capability and closes when
        it does not, on every call — so a renegotiation in which the peer drops
        support closes it again."""
        ...
    def frame_metadata_gate(self) -> FrameMetadataGate:
        """What the remote peer declared about frame-metadata support.

        Diagnostic: the library already consults it when answering, when
        installing the transforms, and when appending a trailer, so a caller does
        not need to. It stays closed until `set_remote_description` sees a remote
        description that declares support."""
        ...
    async def add_ice_candidate(self, candidate: IceCandidate) -> None: ...
    def add_track(self, track: Track) -> None: ...
    def add_transceiver(
        self, kind: MediaKind, direction: TransceiverDirection
    ) -> Transceiver: ...
    def transceivers(self) -> list[Transceiver]: ...
    def create_data_channel(self, label: str) -> DataChannel: ...
    async def get_stats(self) -> StatsReport: ...
    async def set_bitrate(
        self,
        min_bps: Optional[int] = None,
        start_bps: Optional[int] = None,
        max_bps: Optional[int] = None,
    ) -> None:  # raises RuntimeError if libwebrtc rejects the settings
        """Adjust the bandwidth estimate limits and starting point.

        All arguments are optional; pass `None` to leave a value at its
        libwebrtc default. Units are bits-per-second.

        Typical streaming values: `min_bps=200_000, start_bps=500_000, max_bps=2_000_000`."""
        ...

# ── Factory ───────────────────────────────────────────────────────────────────

class PeerConnectionFactory:
    def __init__(
        self,
        platform_adm: bool = False,
        echo_canceller: bool = False,
        noise_suppression: bool = False,
        agc: bool = False,
        high_pass_filter: bool = False,
    ) -> None: ...
    def create_peer_connection(
        self,
        config: RtcConfiguration,
        observer: PeerConnectionObserver,
    ) -> PeerConnection: ...
    def create_video_track(self, id: str) -> Track: ...
    def create_audio_track(self, id: str) -> Track: ...
    def create_audio_track_with_local_source(self, id: str) -> Track:
        """Create a local audio track with a per-track audio source independent of
        the factory ADM. Feed samples via `track.push_pcm(pcm, sample_rate, channels)`.
        Each call returns an independent track, so different audio can be pushed to
        different peer connections."""
        ...
    def push_audio_frame(
        self, pcm: bytes, sample_rate: int, channels: int
    ) -> None:
        """Feed interleaved signed 16-bit little-endian PCM to the synthetic ADM."""
        ...
    @staticmethod
    def with_encoded_video_track(
        track_id: str, width: int, height: int
    ) -> tuple[PeerConnectionFactory, EncodedVideoTrack]: ...
    def set_adm_playout_enabled(self, enabled: bool) -> None: ...
