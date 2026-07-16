"""Tests for pure value types and the binding-layer API surface.

These tests validate the Python constructor signatures, field access,
mutation, repr, and enum equality that live entirely in the binding
layer — coverage the Rust unit tests cannot provide.
"""

import pytest
import reactor_webrtc as rw


class TestIceServer:
    def test_defaults(self):
        s = rw.IceServer()
        assert s.urls == []
        assert s.username == ""
        assert s.password == ""

    def test_positional_args(self):
        s = rw.IceServer(["stun:a.com:3478"], "user", "pass")
        assert s.urls == ["stun:a.com:3478"]
        assert s.username == "user"
        assert s.password == "pass"

    def test_keyword_args(self):
        s = rw.IceServer(
            urls=["turn:b.com:3478"],
            username="alice",
            password="secret",
        )
        assert s.urls == ["turn:b.com:3478"]
        assert s.username == "alice"
        assert s.password == "secret"

    def test_multiple_urls(self):
        urls = ["stun:a.com:3478", "turn:b.com:3478", "turns:b.com:5349"]
        s = rw.IceServer(urls=urls)
        assert s.urls == urls

    def test_mutation(self):
        s = rw.IceServer()
        s.urls = ["stun:x.com:3478"]
        s.username = "u"
        s.password = "p"
        assert s.urls == ["stun:x.com:3478"]
        assert s.username == "u"
        assert s.password == "p"

    def test_repr_contains_url(self):
        s = rw.IceServer(urls=["stun:example.com:3478"])
        assert "stun:example.com:3478" in repr(s)

    def test_repr_empty(self):
        assert repr(rw.IceServer()) is not None


class TestRtcConfiguration:
    def test_defaults_empty_servers(self):
        c = rw.RtcConfiguration()
        assert c.ice_servers == []

    def test_with_one_server(self):
        server = rw.IceServer(urls=["stun:a.com:3478"])
        c = rw.RtcConfiguration(ice_servers=[server])
        assert len(c.ice_servers) == 1
        assert c.ice_servers[0].urls == ["stun:a.com:3478"]

    def test_with_multiple_servers(self):
        servers = [
            rw.IceServer(urls=["stun:a.com"]),
            rw.IceServer(urls=["turn:b.com"], username="u", password="p"),
        ]
        c = rw.RtcConfiguration(ice_servers=servers)
        assert len(c.ice_servers) == 2

    def test_setter_replaces_servers(self):
        c = rw.RtcConfiguration(ice_servers=[rw.IceServer(urls=["stun:a.com"])])
        c.ice_servers = [rw.IceServer(urls=["stun:b.com"])]
        assert c.ice_servers[0].urls == ["stun:b.com"]

    def test_setter_clears_servers(self):
        c = rw.RtcConfiguration(ice_servers=[rw.IceServer(urls=["stun:a.com"])])
        c.ice_servers = []
        assert c.ice_servers == []


class TestIceCandidate:
    def test_required_candidate_field(self):
        cand = rw.IceCandidate("candidate:0 1 UDP 2122252543 192.0.2.1 12345 typ host")
        assert "candidate:0" in cand.candidate

    def test_optional_fields_default_none(self):
        cand = rw.IceCandidate("candidate:0")
        assert cand.sdp_mid is None
        assert cand.sdp_mline_index is None

    def test_with_sdp_mid(self):
        cand = rw.IceCandidate("candidate:0", sdp_mid="audio")
        assert cand.sdp_mid == "audio"
        assert cand.sdp_mline_index is None

    def test_with_mline_index(self):
        cand = rw.IceCandidate("candidate:0", sdp_mline_index=0)
        assert cand.sdp_mline_index == 0

    def test_with_all_fields(self):
        cand = rw.IceCandidate("candidate:0", sdp_mid="video", sdp_mline_index=1)
        assert cand.candidate == "candidate:0"
        assert cand.sdp_mid == "video"
        assert cand.sdp_mline_index == 1

    def test_repr(self):
        cand = rw.IceCandidate("candidate:0", sdp_mid="data")
        assert "data" in repr(cand)


class TestSessionDescription:
    def test_offer(self):
        sdp = rw.SessionDescription("offer", "v=0\r\n")
        assert sdp.kind == "offer"
        assert sdp.sdp == "v=0\r\n"

    def test_answer(self):
        sdp = rw.SessionDescription("answer", "v=0\r\n")
        assert sdp.kind == "answer"

    def test_pranswer(self):
        sdp = rw.SessionDescription("pranswer", "v=0\r\n")
        assert sdp.kind == "pranswer"

    def test_rollback(self):
        sdp = rw.SessionDescription("rollback", "")
        assert sdp.kind == "rollback"

    def test_repr_contains_kind(self):
        sdp = rw.SessionDescription("offer", "v=0\r\n")
        assert "offer" in repr(sdp)


class TestPeerConnectionState:
    def test_equality(self):
        assert rw.PeerConnectionState.Connected == rw.PeerConnectionState.Connected
        assert rw.PeerConnectionState.New == rw.PeerConnectionState.New

    def test_inequality(self):
        assert rw.PeerConnectionState.New != rw.PeerConnectionState.Connected
        assert rw.PeerConnectionState.Failed != rw.PeerConnectionState.Closed

    def test_all_variants_distinct(self):
        variants = [
            rw.PeerConnectionState.New,
            rw.PeerConnectionState.Connecting,
            rw.PeerConnectionState.Connected,
            rw.PeerConnectionState.Disconnected,
            rw.PeerConnectionState.Failed,
            rw.PeerConnectionState.Closed,
        ]
        for i, a in enumerate(variants):
            for j, b in enumerate(variants):
                assert (a == b) == (i == j)


class TestIceGatheringState:
    def test_equality(self):
        assert rw.IceGatheringState.New == rw.IceGatheringState.New
        assert rw.IceGatheringState.Gathering != rw.IceGatheringState.Complete

    def test_all_variants_distinct(self):
        variants = [
            rw.IceGatheringState.New,
            rw.IceGatheringState.Gathering,
            rw.IceGatheringState.Complete,
        ]
        for i, a in enumerate(variants):
            for j, b in enumerate(variants):
                assert (a == b) == (i == j)


class TestDataChannelState:
    def test_all_variants_distinct(self):
        variants = [
            rw.DataChannelState.Connecting,
            rw.DataChannelState.Open,
            rw.DataChannelState.Closing,
            rw.DataChannelState.Closed,
        ]
        for i, a in enumerate(variants):
            for j, b in enumerate(variants):
                assert (a == b) == (i == j)


class TestMediaKind:
    def test_audio_not_video(self):
        assert rw.MediaKind.Audio != rw.MediaKind.Video

    def test_unknown_distinct(self):
        assert rw.MediaKind.Unknown != rw.MediaKind.Audio
        assert rw.MediaKind.Unknown != rw.MediaKind.Video

    def test_equality(self):
        assert rw.MediaKind.Video == rw.MediaKind.Video
        assert rw.MediaKind.Audio == rw.MediaKind.Audio


class TestTransceiverDirection:
    def test_all_variants_distinct(self):
        variants = [
            rw.TransceiverDirection.SendRecv,
            rw.TransceiverDirection.SendOnly,
            rw.TransceiverDirection.RecvOnly,
            rw.TransceiverDirection.Inactive,
        ]
        for i, a in enumerate(variants):
            for j, b in enumerate(variants):
                assert (a == b) == (i == j)


class TestPeerConnectionObserver:
    def test_all_callbacks_default_none(self):
        obs = rw.PeerConnectionObserver()
        assert obs.on_connection_state_change is None
        assert obs.on_ice_gathering_change is None
        assert obs.on_ice_candidate is None
        assert obs.on_track is None
        assert obs.on_data_channel is None

    def test_assign_and_read_back(self):
        obs = rw.PeerConnectionObserver()
        fn = lambda s: None
        obs.on_connection_state_change = fn
        assert obs.on_connection_state_change is fn

    def test_assign_all_callbacks(self):
        obs = rw.PeerConnectionObserver()
        obs.on_connection_state_change = lambda s: None
        obs.on_ice_gathering_change = lambda s: None
        obs.on_ice_candidate = lambda c: None
        obs.on_track = lambda k, t: None
        obs.on_data_channel = lambda dc: None
        assert obs.on_connection_state_change is not None
        assert obs.on_ice_gathering_change is not None
        assert obs.on_ice_candidate is not None
        assert obs.on_track is not None
        assert obs.on_data_channel is not None

    def test_overwrite_callback(self):
        obs = rw.PeerConnectionObserver()
        fn1 = lambda: None
        fn2 = lambda: None
        obs.on_ice_candidate = fn1
        obs.on_ice_candidate = fn2
        assert obs.on_ice_candidate is fn2
