"""Type stubs for reactor_webrtc — WebRTC primitives for Python."""

from __future__ import annotations

from typing import Callable, Optional

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
    def __init__(self, ice_servers: list[IceServer] = ...) -> None: ...

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

# ── Media ─────────────────────────────────────────────────────────────────────

class Track:
    def kind(self) -> MediaKind: ...
    def push_video_frame(self, bgra: bytes, width: int, height: int) -> None: ...
    def on_video_frame(
        self,
        callback: Callable[[bytes, int, int], None],
    ) -> None:
        """Register `callback(bgra: bytes, width: int, height: int)`."""
        ...
    def on_audio_frame(
        self,
        callback: Callable[[bytes, int, int, int], None],
    ) -> None:
        """Register `callback(pcm: bytes, sample_rate: int, channels: int, frames: int)`."""
        ...

class EncodedVideoTrack:
    def push_encoded_frame(
        self,
        data: bytes,
        is_key_frame: bool = False,
        width: int = 0,
        height: int = 0,
        rtp_timestamp: int = 0,
    ) -> None: ...
    def track(self) -> Track: ...

class Transceiver:
    def mid(self) -> Optional[str]: ...
    def set_track(self, track: Track) -> None: ...

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
    def create_data_channel(self, label: str) -> DataChannel: ...

# ── Factory ───────────────────────────────────────────────────────────────────

class PeerConnectionFactory:
    def __init__(self, platform_adm: bool = False) -> None: ...
    def create_peer_connection(
        self,
        config: RtcConfiguration,
        observer: PeerConnectionObserver,
    ) -> PeerConnection: ...
    def create_video_track(self, id: str) -> Track: ...
    def create_audio_track(self, id: str) -> Track: ...
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
