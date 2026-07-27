"""Integration tests for reactor_webrtc using in-process loopback.

All tests require the module to be built (`maturin develop`).  Run with:

    pytest crates/reactor-webrtc-py/tests/ -v
"""

import threading
import time
from dataclasses import dataclass, field

import pytest
import reactor_webrtc as rw

TIMEOUT = 20.0  # seconds — generous for slow CI machines
POLL = 0.025


# ── Helpers ───────────────────────────────────────────────────────────────────


def wait_for(condition, timeout: float = TIMEOUT, poll: float = POLL) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        time.sleep(poll)
    return False


@dataclass
class Peer:
    pc: rw.PeerConnection
    ice: list = field(default_factory=list)
    connected: threading.Event = field(default_factory=threading.Event)
    gathering_states: list = field(default_factory=list)


def make_peer(
    factory: rw.PeerConnectionFactory,
    *,
    on_data_channel=None,
    on_track=None,
) -> Peer:
    peer = Peer(pc=None)  # type: ignore[arg-type]
    obs = rw.PeerConnectionObserver()
    # Capture the individual collections rather than `peer` itself to avoid a
    # reference cycle: peer.pc -> observer closures -> lambda -> peer -> peer.pc.
    # A cycle prevents CPython's refcount from reaching zero when the test frame
    # exits, keeping the old PeerConnection (and its active ICE threads) alive
    # into subsequent tests where they can deadlock signaling-thread operations.
    _ice = peer.ice
    _connected = peer.connected
    _gathering = peer.gathering_states
    obs.on_ice_candidate = lambda c: _ice.append(c)
    obs.on_connection_state_change = (
        lambda s: _connected.set() if s == rw.PeerConnectionState.Connected else None
    )
    obs.on_ice_gathering_change = lambda s: _gathering.append(s)
    if on_data_channel is not None:
        obs.on_data_channel = on_data_channel
    if on_track is not None:
        obs.on_track = on_track
    peer.pc = factory.create_peer_connection(rw.RtcConfiguration(), obs)
    return peer


def negotiate(p1: Peer, p2: Peer) -> None:
    offer = p1.pc.create_offer()
    p1.pc.set_local_description(offer)
    p2.pc.set_remote_description(offer)
    answer = p2.pc.create_answer()
    p2.pc.set_local_description(answer)
    p1.pc.set_remote_description(answer)


def trickle(src: Peer, dst: Peer) -> None:
    for cand in list(src.ice):
        dst.pc.add_ice_candidate(cand)
    src.ice.clear()


def connect(p1: Peer, p2: Peer, *, open_event: threading.Event | None = None) -> bool:
    """Negotiate and trickle ICE until both peers are connected."""
    negotiate(p1, p2)
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        trickle(p1, p2)
        trickle(p2, p1)
        if open_event is not None and open_event.is_set():
            return True
        if open_event is None and p1.connected.is_set() and p2.connected.is_set():
            return True
        time.sleep(POLL)
    return False


# ── Factory and track creation ────────────────────────────────────────────────


class TestFactory:
    def test_creates_video_track(self, factory):
        t = factory.create_video_track("v1")
        assert t.kind() == rw.MediaKind.Video

    def test_creates_audio_track(self, factory):
        t = factory.create_audio_track("a1")
        assert t.kind() == rw.MediaKind.Audio

    def test_push_audio_frame_valid(self, factory):
        # 480 i16 samples × 2 bytes = 960 bytes (10 ms at 48 kHz mono)
        pcm = b"\x00" * 960
        factory.push_audio_frame(pcm, sample_rate=48000, channels=1)

    def test_push_audio_frame_odd_length_raises(self, factory):
        with pytest.raises(RuntimeError, match="even"):
            factory.push_audio_frame(b"\x00" * 3, sample_rate=48000, channels=1)


# ── Peer connection creation ──────────────────────────────────────────────────


class TestPeerConnection:
    def test_create_offer_returns_offer_sdp(self, factory):
        p = make_peer(factory)
        dc = p.pc.create_data_channel("probe")  # need an m-section for a non-empty offer
        offer = p.pc.create_offer()
        assert offer.kind == "offer"
        assert "v=0" in offer.sdp

    def test_create_answer_after_remote_offer(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")
        offer = p1.pc.create_offer()
        p1.pc.set_local_description(offer)
        p2.pc.set_remote_description(offer)
        answer = p2.pc.create_answer()
        assert answer.kind == "answer"
        assert "v=0" in answer.sdp

    def test_invalid_sdp_kind_raises(self, factory):
        p = make_peer(factory)
        bad_sdp = rw.SessionDescription("bogus", "v=0\r\n")
        with pytest.raises(RuntimeError, match="unknown SDP kind"):
            p.pc.set_remote_description(bad_sdp)

    def test_add_transceiver_unknown_kind_raises(self, factory):
        p = make_peer(factory)
        with pytest.raises(RuntimeError, match="Audio or Video"):
            p.pc.add_transceiver(rw.MediaKind.Unknown, rw.TransceiverDirection.SendRecv)

    def test_add_transceiver_mid_set_after_sdp(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        t = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        assert t.mid() is None  # not yet negotiated
        negotiate(p1, p2)
        assert t.mid() is not None  # SDP exchange assigns the mid

    def test_add_video_track(self, factory):
        p = make_peer(factory)
        track = factory.create_video_track("cam")
        p.pc.add_track(track)  # must not raise

    def test_ice_gathering_change_fires(self, factory):
        p = make_peer(factory)
        p.pc.create_data_channel("probe")
        # ICE gathering starts after set_local_description, not create_offer
        offer = p.pc.create_offer()
        p.pc.set_local_description(offer)
        ok = wait_for(lambda: rw.IceGatheringState.Gathering in p.gathering_states)
        assert ok, "on_ice_gathering_change(Gathering) never fired"

    def test_ice_candidates_collected(self, factory):
        p = make_peer(factory)
        p.pc.create_data_channel("probe")
        offer = p.pc.create_offer()
        p.pc.set_local_description(offer)
        ok = wait_for(lambda: len(p.ice) > 0)
        assert ok, "no ICE candidates gathered within timeout"
        for c in p.ice:
            assert isinstance(c, rw.IceCandidate)
            assert c.candidate.startswith("candidate:")


# ── Data channel ─────────────────────────────────────────────────────────────


class TestDataChannel:
    def test_label(self, factory):
        p = make_peer(factory)
        dc = p.pc.create_data_channel("my-channel")
        assert dc.label() == "my-channel"

    def test_initial_state(self, factory):
        p = make_peer(factory)
        dc = p.pc.create_data_channel("ch")
        # Before connection the channel is in Connecting state
        assert dc.state() == rw.DataChannelState.Connecting

    def test_buffered_amount_zero_before_open(self, factory):
        p = make_peer(factory)
        dc = p.pc.create_data_channel("ch")
        assert dc.buffered_amount() == 0

    def test_send_receive_binary(self, factory):
        received: list[tuple[bytes, bool]] = []
        dc2_ref: list[rw.DataChannel] = []

        def on_dc(dc):
            dc2_ref.append(dc)
            dc.on_message(lambda data, binary: received.append((data, binary)))

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_data_channel=on_dc)

        dc1 = p1.pc.create_data_channel("test")
        dc1_open = threading.Event()
        dc1.on_open(dc1_open.set)

        ok = connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        dc1.send(b"hello", True)
        dc1.send(b"world", True)

        assert wait_for(lambda: len(received) >= 2), "messages not received"
        payloads = {r[0] for r in received}
        assert payloads == {b"hello", b"world"}
        assert all(binary for _, binary in received)

    def test_send_receive_text(self, factory):
        received: list[tuple[bytes, bool]] = []
        dc2_ref: list[rw.DataChannel] = []

        def on_dc(dc):
            dc2_ref.append(dc)
            dc.on_message(lambda data, binary: received.append((data, binary)))

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_data_channel=on_dc)

        dc1 = p1.pc.create_data_channel("text-ch")
        dc1_open = threading.Event()
        dc1.on_open(dc1_open.set)

        ok = connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        dc1.send(b"ping", False)  # binary=False → text SCTP message

        assert wait_for(lambda: len(received) >= 1)
        assert received[0][0] == b"ping"
        # text messages arrive with binary=False
        assert received[0][1] is False

    def test_on_state_change_fires_open(self, factory):
        states: list[rw.DataChannelState] = []
        dc2_ref: list[rw.DataChannel] = []

        def on_dc(dc):
            dc2_ref.append(dc)
            dc.on_state_change(lambda s: states.append(s))

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_data_channel=on_dc)

        dc1 = p1.pc.create_data_channel("sc")
        dc1_open = threading.Event()
        dc1.on_open(dc1_open.set)

        ok = connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        assert wait_for(lambda: rw.DataChannelState.Open in states), (
            "on_state_change(Open) never fired on the remote data channel"
        )

    def test_multiple_channels(self, factory):
        received_a: list[bytes] = []
        received_b: list[bytes] = []
        dcs: list[rw.DataChannel] = []

        def on_dc(dc):
            dcs.append(dc)
            if dc.label() == "ch-a":
                dc.on_message(lambda data, _: received_a.append(data))
            else:
                dc.on_message(lambda data, _: received_b.append(data))

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_data_channel=on_dc)

        dca = p1.pc.create_data_channel("ch-a")
        dcb = p1.pc.create_data_channel("ch-b")
        both_open = threading.Event()
        opened = [0]

        def mark_open():
            opened[0] += 1
            if opened[0] == 2:
                both_open.set()

        dca.on_open(mark_open)
        dcb.on_open(mark_open)

        ok = connect(p1, p2, open_event=both_open)
        assert ok, "data channels did not open within timeout"

        dca.send(b"from-a", True)
        dcb.send(b"from-b", True)

        assert wait_for(lambda: received_a and received_b), "messages not received on both channels"
        assert received_a == [b"from-a"]
        assert received_b == [b"from-b"]


# ── Stats ─────────────────────────────────────────────────────────────────────


class TestStats:
    def test_get_stats_returns_report(self, factory):
        """get_stats() always returns a StatsReport, even before connection."""
        p = make_peer(factory)
        report = p.pc.get_stats()
        assert isinstance(report, rw.StatsReport)

    def test_stats_empty_before_negotiation(self, factory):
        """No RTP streams without an offer/answer exchange."""
        p = make_peer(factory)
        report = p.pc.get_stats()
        assert report.inbound_rtp == []
        assert report.outbound_rtp == []

    def test_stats_candidate_pairs_after_connection(self, factory):
        """At least one candidate pair exists once peers are connected."""
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")

        ok = connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        report1 = p1.pc.get_stats()
        assert isinstance(report1, rw.StatsReport)
        assert len(report1.candidate_pairs) > 0, "expected at least one candidate pair"

    def test_candidate_pair_stats_fields(self, factory):
        """IceCandidatePairStats fields have sane types and values."""
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")

        ok = connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        report = p1.pc.get_stats()
        pair = next(
            (cp for cp in report.candidate_pairs if cp.state == rw.IceCandidatePairState.Succeeded),
            None,
        )
        assert pair is not None, "expected a Succeeded candidate pair"
        assert pair.priority > 0
        assert pair.current_round_trip_time_s >= 0.0

    def test_ice_candidate_pair_state_variants_distinct(self):
        variants = [
            rw.IceCandidatePairState.Waiting,
            rw.IceCandidatePairState.InProgress,
            rw.IceCandidatePairState.Failed,
            rw.IceCandidatePairState.Succeeded,
            rw.IceCandidatePairState.Cancelled,
        ]
        for i, a in enumerate(variants):
            for j, b in enumerate(variants):
                assert (a == b) == (i == j)

    def test_stats_report_repr(self, factory):
        p = make_peer(factory)
        r = p.pc.get_stats()
        assert "StatsReport" in repr(r)

    def test_inbound_rtp_stats_fields_after_receive(self, factory):
        """After receiving RTP audio, inbound stats are populated."""
        received_audio = threading.Event()
        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=lambda kind, t: (
            t.on_audio_frame(lambda *_: received_audio.set())
        ))

        audio = factory.create_audio_track("mic")
        p1.pc.add_track(audio)

        ok = connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        # Push enough PCM so at least one RTP packet crosses the loopback
        pcm = b"\x00" * 960  # 10 ms at 48 kHz mono
        for _ in range(20):
            factory.push_audio_frame(pcm, sample_rate=48000, channels=1)
            time.sleep(0.01)

        wait_for(lambda: received_audio.is_set(), timeout=10.0)

        report = p2.pc.get_stats()
        assert len(report.inbound_rtp) > 0, "expected inbound RTP stats after receiving audio"
        s = report.inbound_rtp[0]
        assert s.ssrc > 0
        assert s.packets_received >= 0
        assert s.bytes_received >= 0
        assert s.jitter_s >= 0.0


# ── Frame metadata ────────────────────────────────────────────────────────────


class TestFrameMetadata:
    """End-to-end tests for per-frame metadata embedded via the packet trailer."""

    def test_frame_metadata_roundtrip(self, factory):
        """Metadata pushed by the sender arrives in on_video_frame as FrameMetadata."""
        recv_track_ref: list = []  # keeps the Track alive — Drop removes the sink
        recv_tf_ref: list = []  # keeps the FrameTransform alive
        received_meta: list[rw.FrameMetadata] = []

        def on_track(kind, track):
            if kind == rw.MediaKind.Video:
                recv_track_ref.append(track)  # prevent GC → Drop → RemoveSink
                recv_tf = track.receiver_metadata_transform()
                recv_tf_ref.append(recv_tf)
                track.on_video_frame(
                    lambda bgra, w, h, meta: received_meta.append(meta) if meta is not None else None
                )

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=on_track)

        video = factory.create_video_track("meta-video")
        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        tx1.set_track(video)
        send_tf = video.sender_metadata_transform()  # noqa: F841 — must stay alive
        tx1.set_sender_transform(send_tf)

        ok = connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        # on_track fired during negotiate(); the receiver transform is ready.
        # Wire it to the transceiver now that we have the pc2 handle.
        assert recv_tf_ref, "on_track was not called during SDP negotiation"
        for t in p2.pc.transceivers():
            if t.kind() == rw.MediaKind.Video:
                t.set_receiver_transform(recv_tf_ref[0])
                break

        user_data = b"reactor-py-e2e"
        bgra = bytes(320 * 240 * 4)

        for _ in range(90):
            if received_meta:
                break
            video.push_video_frame(bgra, 320, 240, user_data=user_data)
            time.sleep(0.033)

        ok = wait_for(lambda: len(received_meta) > 0)
        assert ok, "no metadata received within timeout"

        meta = received_meta[0]
        assert bytes(meta.user_data) == user_data, f"user_data mismatch: {meta.user_data!r}"
        assert meta.frame_id > 0, "frame_id must be non-zero"
        assert meta.timestamp > 0, "timestamp must be non-zero"

    def test_no_transform_peer_decodes_cleanly(self, factory):
        """A receiver without receiver_metadata_transform still decodes frames; metadata is None."""
        recv_track_ref: list = []  # keeps the Track alive — Drop removes the sink
        received_frames: list = []

        def on_track(kind, track):
            if kind == rw.MediaKind.Video:
                recv_track_ref.append(track)  # prevent GC → Drop → RemoveSink
                track.on_video_frame(
                    lambda bgra, w, h, meta: received_frames.append((w, h, meta))
                )

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=on_track)

        video = factory.create_video_track("notf-video")
        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        tx1.set_track(video)
        send_tf = video.sender_metadata_transform()  # noqa: F841 — must stay alive
        tx1.set_sender_transform(send_tf)

        ok = connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        bgra = bytes(320 * 240 * 4)
        for _ in range(90):
            if received_frames:
                break
            video.push_video_frame(bgra, 320, 240, user_data=b"dropped")
            time.sleep(0.033)

        ok = wait_for(lambda: len(received_frames) > 0)
        assert ok, "no decoded frame received within timeout"

        _, _, meta = received_frames[0]
        assert meta is None, "expected metadata=None without receiver_metadata_transform"
