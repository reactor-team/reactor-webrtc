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

class RtcConfiguration:
    ice_servers: list[IceServer]
    ice_transport_type: str  # "all" | "relay" | "no_host" | "none"
    continual_gathering_policy: str  # "once" | "continually"
    min_port: int  # 0 = use OS default
    max_port: int  # 0 = use OS default
    def __init__(
        self,
        ice_servers: list[IceServer] = ...,
        ice_transport_type: str = "all",
        continual_gathering_policy: str = "once",
        min_port: int = 0,
        max_port: int = 0,
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
    """Encoded-frame transformer attached to a transceiver sender or receiver.

    Create from a Python callable with `FrameTransform(callback)`, or obtain a
    pre-built metadata transform from
    `Track.sender_metadata_transform()` / `receiver_metadata_transform()` or
    `EncodedVideoTrack.sender_metadata_transform()`.

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
        packet trailer. Requires `sender_metadata_transform()` to be attached
        to the sender transceiver beforehand; otherwise `user_data` is
        silently ignored."""
        ...
    def sender_metadata_transform(self) -> FrameTransform:
        """Return a FrameTransform that appends a metadata trailer to encoded
        frames on the send path. Attach to the sender transceiver with
        `Transceiver.set_sender_transform` before the SDP exchange."""
        ...
    def receiver_metadata_transform(self) -> FrameTransform:
        """Return a FrameTransform that strips the metadata trailer from
        received encoded frames. After attachment, `on_video_frame` callbacks
        carry a `FrameMetadata` when the sender included a trailer."""
        ...
    def on_video_frame(
        self,
        callback: Callable[[bytes, int, int, Optional[FrameMetadata]], None],
    ) -> None:
        """Register `callback(bgra: bytes, width: int, height: int, metadata: FrameMetadata | None)`.

        `metadata` is `None` when no receiver transform is attached or when the
        sender did not include a trailer. For backward compatibility with
        3-argument callbacks `callback(bgra, width, height)`, the 4-argument
        call is retried as a 3-argument call on `TypeError` when `metadata` is
        `None`.

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
        packet trailer. Requires `sender_metadata_transform()` to be attached
        to the sender transceiver beforehand; otherwise `user_data` is
        silently ignored."""
        ...
    def sender_metadata_transform(self) -> FrameTransform:
        """Return a sender FrameTransform that embeds per-frame metadata
        trailers. Attach to the sender transceiver with
        `Transceiver.set_sender_transform` before the SDP exchange."""
        ...
    def add_to_peer_connection(self, pc: PeerConnection) -> None: ...
    def add_transceiver(
        self, pc: PeerConnection, direction: TransceiverDirection
    ) -> Transceiver: ...

class Transceiver:
    def mid(self) -> Optional[str]: ...
    def kind(self) -> MediaKind: ...
    def set_track(self, track: Union[Track, EncodedVideoTrack]) -> None: ...
    def set_direction(self, direction: TransceiverDirection) -> None: ...
    def set_sender_transform(self, transform: FrameTransform) -> None:
        """Attach a FrameTransform to the sender path of this transceiver.
        The transform runs after the encoder, before RTP packetization."""
        ...
    def set_receiver_transform(self, transform: FrameTransform) -> None:
        """Attach a FrameTransform to the receiver path of this transceiver.
        The transform runs after RTP depacketization, before the decoder."""
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
    def create_offer(self) -> SessionDescription: ...
    def create_answer(self) -> SessionDescription: ...
    def set_local_description(self, sdp: SessionDescription) -> None: ...
    def set_remote_description(self, sdp: SessionDescription) -> None: ...
    def add_ice_candidate(self, candidate: IceCandidate) -> None: ...
    def add_track(self, track: Track) -> None: ...
    def add_transceiver(
        self, kind: MediaKind, direction: TransceiverDirection
    ) -> Transceiver: ...
    def transceivers(self) -> list[Transceiver]: ...
    def create_data_channel(self, label: str) -> DataChannel: ...
    def get_stats(self) -> StatsReport: ...

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
