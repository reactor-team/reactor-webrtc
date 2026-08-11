"""Integration tests for reactor_webrtc using in-process loopback.

All tests require the module to be built (`maturin develop`).  Run with:

    pytest crates/reactor-webrtc-py/tests/ -v
"""

import asyncio
import threading
import time
from dataclasses import dataclass, field

import pytest
import reactor_webrtc as rw

TIMEOUT = 20.0  # seconds — generous for slow CI machines
POLL = 0.025


# ── Helpers ───────────────────────────────────────────────────────────────────


async def wait_for(condition, timeout: float = TIMEOUT, poll: float = POLL) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        await asyncio.sleep(poll)
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


async def negotiate(p1: Peer, p2: Peer) -> None:
    """Run the offer/answer exchange.

    Nothing here mentions frame metadata: create_offer advertises it and
    create_answer mirrors it.
    """
    offer = await p1.pc.create_offer()
    await p1.pc.set_local_description(offer)
    await p2.pc.set_remote_description(offer)
    answer = await p2.pc.create_answer()
    await p2.pc.set_local_description(answer)
    await p1.pc.set_remote_description(answer)


def strip_frame_metadata(sdp: rw.SessionDescription) -> rw.SessionDescription:
    """Drop the frame-metadata declaration.

    Turns a reactor-webrtc description into what a peer built before the
    capability existed would have produced.
    """
    prefix = f"a={rw.FRAME_METADATA_ATTRIBUTE}:"
    kept = "".join(
        f"{line}\r\n"
        for line in sdp.sdp.splitlines()
        if not line.startswith(prefix)
    )
    return rw.SessionDescription(sdp.kind, kept)


async def negotiate_with_legacy_peer(p1: Peer, p2: Peer) -> None:
    """Negotiate against a peer that does not understand frame metadata."""
    offer = await p1.pc.create_offer()
    await p1.pc.set_local_description(offer)
    await p2.pc.set_remote_description(strip_frame_metadata(offer))
    answer = await p2.pc.create_answer()
    assert not answer.declares_frame_metadata()
    await p2.pc.set_local_description(answer)
    await p1.pc.set_remote_description(answer)


async def trickle(src: Peer, dst: Peer) -> None:
    for cand in list(src.ice):
        await dst.pc.add_ice_candidate(cand)
    src.ice.clear()


async def connect(
    p1: Peer,
    p2: Peer,
    *,
    open_event: threading.Event | None = None,
    legacy_peer: bool = False,
) -> bool:
    """Negotiate and trickle ICE until both peers are connected."""
    if legacy_peer:
        await negotiate_with_legacy_peer(p1, p2)
    else:
        await negotiate(p1, p2)
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        await trickle(p1, p2)
        await trickle(p2, p1)
        if open_event is not None and open_event.is_set():
            return True
        if open_event is None and p1.connected.is_set() and p2.connected.is_set():
            return True
        await asyncio.sleep(POLL)
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
    async def test_create_offer_returns_offer_sdp(self, factory):
        p = make_peer(factory)
        dc = p.pc.create_data_channel("probe")  # need an m-section for a non-empty offer
        offer = await p.pc.create_offer()
        assert offer.kind == "offer"
        assert "v=0" in offer.sdp

    async def test_create_answer_after_remote_offer(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")
        offer = await p1.pc.create_offer()
        await p1.pc.set_local_description(offer)
        await p2.pc.set_remote_description(offer)
        answer = await p2.pc.create_answer()
        assert answer.kind == "answer"
        assert "v=0" in answer.sdp

    async def test_invalid_sdp_kind_raises(self, factory):
        p = make_peer(factory)
        bad_sdp = rw.SessionDescription("bogus", "v=0\r\n")
        with pytest.raises(RuntimeError, match="unknown SDP kind"):
            await p.pc.set_remote_description(bad_sdp)

    def test_add_transceiver_unknown_kind_raises(self, factory):
        p = make_peer(factory)
        with pytest.raises(RuntimeError, match="Audio or Video"):
            p.pc.add_transceiver(rw.MediaKind.Unknown, rw.TransceiverDirection.SendRecv)

    async def test_add_transceiver_mid_set_after_sdp(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        t = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        assert t.mid() is None  # not yet negotiated
        await negotiate(p1, p2)
        assert t.mid() is not None  # SDP exchange assigns the mid

    async def test_set_codec_preferences_reorders_video_codecs(self, factory):
        p = make_peer(factory)
        t = p.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendRecv)

        def first_video_codec(sdp: str) -> str:
            video_line = next(l for l in sdp.splitlines() if l.startswith("m=video"))
            first_pt = video_line.split()[3]
            rtpmap = next(
                l for l in sdp.splitlines() if l.startswith(f"a=rtpmap:{first_pt} ")
            )
            return rtpmap.split(" ", 1)[1].split("/")[0]

        default_first = first_video_codec((await p.pc.create_offer()).sdp)
        preferred = (
            rw.VideoCodec.Vp8 if default_first.upper() == "VP9" else rw.VideoCodec.Vp9
        )
        preferred_name = "VP8" if preferred == rw.VideoCodec.Vp8 else "VP9"

        await t.set_codec_preferences([preferred])
        reordered_sdp = (await p.pc.create_offer()).sdp
        assert first_video_codec(reordered_sdp) == preferred_name
        # Reordered, not dropped: the old default is still offered somewhere.
        assert f" {default_first}/" in reordered_sdp

    async def test_set_codec_preferences_rejects_audio_transceiver(self, factory):
        p = make_peer(factory)
        t = p.pc.add_transceiver(rw.MediaKind.Audio, rw.TransceiverDirection.SendRecv)
        with pytest.raises(RuntimeError, match="set_codec_preferences"):
            await t.set_codec_preferences([rw.VideoCodec.Vp8])

    def test_add_video_track(self, factory):
        p = make_peer(factory)
        track = factory.create_video_track("cam")
        p.pc.add_track(track)  # must not raise

    async def test_ice_gathering_change_fires(self, factory):
        p = make_peer(factory)
        p.pc.create_data_channel("probe")
        # ICE gathering starts after set_local_description, not create_offer
        offer = await p.pc.create_offer()
        await p.pc.set_local_description(offer)
        ok = await wait_for(lambda: rw.IceGatheringState.Gathering in p.gathering_states)
        assert ok, "on_ice_gathering_change(Gathering) never fired"

    async def test_ice_candidates_collected(self, factory):
        p = make_peer(factory)
        p.pc.create_data_channel("probe")
        offer = await p.pc.create_offer()
        await p.pc.set_local_description(offer)
        ok = await wait_for(lambda: len(p.ice) > 0)
        assert ok, "no ICE candidates gathered within timeout"
        for c in p.ice:
            assert isinstance(c, rw.IceCandidate)
            assert c.candidate.startswith("candidate:")

    async def test_set_bitrate_is_awaitable(self, factory):
        p = make_peer(factory)
        await p.pc.set_bitrate(100_000, 500_000, 2_000_000)

    async def test_set_bitrate_rejects_inconsistent_bounds(self, factory):
        p = make_peer(factory)
        with pytest.raises(RuntimeError):
            await p.pc.set_bitrate(2_000_000, 500_000, 100_000)  # min above max


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

    async def test_send_receive_binary(self, factory):
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

        ok = await connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        dc1.send(b"hello", True)
        dc1.send(b"world", True)

        assert await wait_for(lambda: len(received) >= 2), "messages not received"
        payloads = {r[0] for r in received}
        assert payloads == {b"hello", b"world"}
        assert all(binary for _, binary in received)

    async def test_send_receive_text(self, factory):
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

        ok = await connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        dc1.send(b"ping", False)  # binary=False → text SCTP message

        assert await wait_for(lambda: len(received) >= 1)
        assert received[0][0] == b"ping"
        # text messages arrive with binary=False
        assert received[0][1] is False

    async def test_on_state_change_fires_open(self, factory):
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

        ok = await connect(p1, p2, open_event=dc1_open)
        assert ok, "data channel did not open within timeout"

        assert await wait_for(lambda: rw.DataChannelState.Open in states), (
            "on_state_change(Open) never fired on the remote data channel"
        )

    async def test_multiple_channels(self, factory):
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

        ok = await connect(p1, p2, open_event=both_open)
        assert ok, "data channels did not open within timeout"

        dca.send(b"from-a", True)
        dcb.send(b"from-b", True)

        assert await wait_for(lambda: received_a and received_b), (
            "messages not received on both channels"
        )
        assert received_a == [b"from-a"]
        assert received_b == [b"from-b"]


# ── Stats ─────────────────────────────────────────────────────────────────────


class TestStats:
    async def test_get_stats_returns_report(self, factory):
        """get_stats() always returns a StatsReport, even before connection."""
        p = make_peer(factory)
        report = await p.pc.get_stats()
        assert isinstance(report, rw.StatsReport)

    async def test_stats_empty_before_negotiation(self, factory):
        """No RTP streams without an offer/answer exchange."""
        p = make_peer(factory)
        report = await p.pc.get_stats()
        assert report.inbound_rtp == []
        assert report.outbound_rtp == []

    async def test_stats_candidate_pairs_after_connection(self, factory):
        """At least one candidate pair exists once peers are connected."""
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")

        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        report1 = await p1.pc.get_stats()
        assert isinstance(report1, rw.StatsReport)
        assert len(report1.candidate_pairs) > 0, "expected at least one candidate pair"

    async def test_candidate_pair_stats_fields(self, factory):
        """IceCandidatePairStats fields have sane types and values."""
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.create_data_channel("probe")

        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        report = await p1.pc.get_stats()
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

    async def test_stats_report_repr(self, factory):
        p = make_peer(factory)
        r = await p.pc.get_stats()
        assert "StatsReport" in repr(r)

    async def test_inbound_rtp_stats_fields_after_receive(self, factory):
        """After receiving RTP audio, inbound stats are populated."""
        received_audio = threading.Event()
        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=lambda kind, t: (
            t.on_audio_frame(lambda *_: received_audio.set())
        ))

        audio = factory.create_audio_track("mic")
        p1.pc.add_track(audio)

        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"

        # Push enough PCM so at least one RTP packet crosses the loopback
        pcm = b"\x00" * 960  # 10 ms at 48 kHz mono
        for _ in range(20):
            factory.push_audio_frame(pcm, sample_rate=48000, channels=1)
            await asyncio.sleep(0.01)

        await wait_for(lambda: received_audio.is_set(), timeout=10.0)

        report = await p2.pc.get_stats()
        assert len(report.inbound_rtp) > 0, "expected inbound RTP stats after receiving audio"
        s = report.inbound_rtp[0]
        assert s.ssrc > 0
        assert s.packets_received >= 0
        assert s.bytes_received >= 0
        assert s.jitter_s >= 0.0


# ── Frame metadata ────────────────────────────────────────────────────────────


class TestFrameMetadata:
    """End-to-end tests for per-frame metadata embedded via the packet trailer."""

    async def test_frame_metadata_roundtrip(self, factory):
        """Metadata pushed by the sender arrives in on_video_frame as FrameMetadata."""
        recv_track_ref: list = []  # keeps the Track alive — Drop removes the sink
        received_meta: list[rw.FrameMetadata] = []

        def on_track(kind, track):
            if kind == rw.MediaKind.Video:
                recv_track_ref.append(track)  # prevent GC → Drop → RemoveSink
                track.on_video_frame(
                    lambda bgra, w, h, meta: received_meta.append(meta) if meta is not None else None
                )

        p1 = make_peer(factory)
        p2 = make_peer(factory, on_track=on_track)

        video = factory.create_video_track("meta-video")
        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        tx1.set_track(video)
        # Nothing attached by hand: create_offer advertises, the answer mirrors,
        # and set_remote_description installs both transforms.
        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"
        assert p1.pc.frame_metadata_gate().is_open()

        user_data = b"reactor-py-e2e"
        bgra = bytes(320 * 240 * 4)

        for _ in range(90):
            if received_meta:
                break
            video.push_video_frame(bgra, 320, 240, user_data=user_data)
            await asyncio.sleep(0.033)

        ok = await wait_for(lambda: len(received_meta) > 0)
        assert ok, "no metadata received within timeout"

        meta = received_meta[0]
        assert bytes(meta.user_data) == user_data, f"user_data mismatch: {meta.user_data!r}"
        assert meta.frame_id > 0, "frame_id must be non-zero"
        assert meta.timestamp > 0, "timestamp must be non-zero"

    async def test_legacy_peer_gets_no_trailer(self, factory):
        """Against a peer that never declares, no trailer reaches the wire at all.

        Asserted on the encoded bytes rather than on the absence of decoded
        metadata: with no declaration there is no strip transform either, so
        "metadata is None" would hold whether or not a trailer was appended.
        """
        recv_track_ref: list = []
        p1 = make_peer(factory)
        p2 = make_peer(
            factory,
            on_track=lambda kind, track: recv_track_ref.append(track),
        )

        video = factory.create_video_track("legacy-video")
        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        tx1.set_track(video)

        ok = await connect(p1, p2, legacy_peer=True)
        assert ok, "peers did not connect within timeout"
        assert not p1.pc.frame_metadata_gate().is_open()

        seen: list[bool] = []

        # A transform in the receiver slot only ever sees ingress frames, so no
        # direction check is needed.
        def inspect_recv(frame):
            seen.append(bytes(frame.data).endswith(b"RXMT"))
            return rw.FrameAction.Forward

        inspect_tf = rw.FrameTransform(inspect_recv)
        for t in await p2.pc.transceivers():
            if t.kind() == rw.MediaKind.Video:
                t.set_receiver_transform(inspect_tf)
                break

        bgra = bytes(320 * 240 * 4)
        for _ in range(90):
            if len(seen) >= 5:
                break
            video.push_video_frame(bgra, 320, 240, user_data=b"must-not-ship")
            await asyncio.sleep(0.033)

        assert await wait_for(lambda: len(seen) > 0), "no encoded frames arrived"
        assert not any(seen), (
            f"{sum(seen)} of {len(seen)} encoded frames carried a trailer "
            "with the gate closed"
        )

    async def test_caller_transform_and_metadata_compose(self, factory):
        """A caller's sender transform and the trailer share one transceiver.

        libwebrtc gives a sender one frame-transformer slot, so this is only
        expressible because the library owns it and composes. The callback runs
        first, on the encoder's output, so it must not see a trailer.
        """
        p1 = make_peer(factory)
        p2 = make_peer(factory)

        video = factory.create_video_track("claimed-video")
        tx1 = p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        tx1.set_track(video)

        ran: list[bool] = []
        saw_trailer: list[bool] = []

        def mine_cb(frame):
            ran.append(True)
            saw_trailer.append(bytes(frame.data).endswith(b"RXMT"))
            return rw.FrameAction.Forward

        tx1.set_sender_transform(rw.FrameTransform(mine_cb))

        ok = await connect(p1, p2)
        assert ok, "peers did not connect within timeout"
        assert p1.pc.frame_metadata_gate().is_open()

        bgra = bytes(320 * 240 * 4)
        for _ in range(90):
            if ran:
                break
            video.push_video_frame(bgra, 320, 240, user_data=b"composed")
            await asyncio.sleep(0.033)

        assert await wait_for(lambda: len(ran) > 0), (
            "the caller's sender transform never ran"
        )
        assert not any(saw_trailer), (
            "the caller's transform saw a trailer — it must run before the "
            "metadata step"
        )



class TestFrameMetadataNegotiation:
    """Declaring and reading the SDP capability, without any media."""

    async def test_every_offer_advertises_the_capability(self, factory):
        p = make_peer(factory)
        p.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)

        offer = await p.pc.create_offer()
        assert offer.declares_frame_metadata(), (
            "create_offer must advertise the capability"
        )
        declaration = (
            f"a={rw.FRAME_METADATA_ATTRIBUTE}:{rw.FRAME_METADATA_VERSION}"
        )
        assert declaration in offer.sdp
        assert offer.sdp.count(declaration) == 1, "session level, so exactly once"

        # libwebrtc tolerates the injected line in a *local* description: it drops
        # attributes it does not recognise rather than erroring.
        await p.pc.set_local_description(offer)

    async def test_answer_mirrors_the_offer(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)

        offer = await p1.pc.create_offer()
        await p1.pc.set_local_description(offer)
        await p2.pc.set_remote_description(offer)
        answer = await p2.pc.create_answer()
        assert answer.declares_frame_metadata(), (
            "the answer must mirror the offer's declaration"
        )

    async def test_answer_stays_silent_for_a_legacy_offer(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)

        offer = await p1.pc.create_offer()
        await p1.pc.set_local_description(offer)
        # Introducing the capability in an answer that was not offered it is not
        # something offer/answer can express.
        await p2.pc.set_remote_description(strip_frame_metadata(offer))
        answer = await p2.pc.create_answer()
        assert not answer.declares_frame_metadata()
        assert not p2.pc.frame_metadata_gate().is_open()

    async def test_gate_tracks_the_remote_declaration(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        gate = p1.pc.frame_metadata_gate()

        assert not gate.is_open()
        await negotiate(p1, p2)
        assert gate.is_open(), "an answer declaring support must open the gate"

    async def test_gate_stays_closed_against_a_legacy_peer(self, factory):
        p1 = make_peer(factory)
        p2 = make_peer(factory)
        p1.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        gate = p1.pc.frame_metadata_gate()

        await negotiate_with_legacy_peer(p1, p2)
        assert not gate.is_open()

    async def test_audio_only_offer_still_advertises(self, factory):
        # The declaration is session level, so there is nothing about video to
        # condition it on — and a renegotiation that adds video must not have to
        # introduce the capability mid-session.
        p = make_peer(factory)
        p.pc.add_transceiver(rw.MediaKind.Audio, rw.TransceiverDirection.SendOnly)
        offer = await p.pc.create_offer()
        assert offer.declares_frame_metadata()
        await p.pc.set_local_description(offer)

    async def test_manual_helper_is_idempotent(self, factory):
        # create_offer already declared, so with_frame_metadata is a no-op here.
        p = make_peer(factory)
        p.pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)
        offer = await p.pc.create_offer()
        assert offer.with_frame_metadata().sdp == offer.sdp
        assert offer.with_frame_metadata().kind == offer.kind

    async def test_disabled_frame_metadata_never_negotiates(self, factory):
        """frame_metadata=False keeps the capability out of the SDP entirely."""
        off = rw.RtcConfiguration(frame_metadata=False)
        assert off.frame_metadata is False
        assert rw.RtcConfiguration().frame_metadata is True

        obs1 = rw.PeerConnectionObserver()
        pc1 = factory.create_peer_connection(off, obs1)
        obs2 = rw.PeerConnectionObserver()
        pc2 = factory.create_peer_connection(rw.RtcConfiguration(), obs2)
        pc1.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)

        offer = await pc1.create_offer()
        assert not offer.declares_frame_metadata(), (
            "a disabled connection must not advertise the capability"
        )
        await pc1.set_local_description(offer)
        await pc2.set_remote_description(offer)
        answer = await pc2.create_answer()
        assert not answer.declares_frame_metadata()
        await pc2.set_local_description(answer)
        await pc1.set_remote_description(answer)
        assert not pc1.frame_metadata_gate().is_open()
        assert not pc2.frame_metadata_gate().is_open()

    async def test_disabled_answerer_does_not_mirror(self, factory):
        """A disabled answerer stays silent even when the offer declared."""
        obs3 = rw.PeerConnectionObserver()
        pc3 = factory.create_peer_connection(rw.RtcConfiguration(), obs3)
        obs4 = rw.PeerConnectionObserver()
        pc4 = factory.create_peer_connection(
            rw.RtcConfiguration(frame_metadata=False), obs4
        )
        pc3.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly)

        offer = await pc3.create_offer()
        assert offer.declares_frame_metadata()
        await pc3.set_local_description(offer)
        await pc4.set_remote_description(offer)
        answer = await pc4.create_answer()
        assert not answer.declares_frame_metadata()
        assert not pc4.frame_metadata_gate().is_open()
        await pc4.set_local_description(answer)
        await pc3.set_remote_description(answer)
        assert not pc3.frame_metadata_gate().is_open(), (
            "an unmirrored offer must leave the offerer's gate shut"
        )
