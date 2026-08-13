"""Tests for the ICE configuration that reaches libwebrtc.

These tests assert on native acceptance: each case calls
create_peer_connection, because a configuration can round-trip through the
Python getters perfectly and still be rejected by libwebrtc. The getter-only
tests in test_types.py cannot catch that.
"""

import pytest
import reactor_webrtc as rw

STUN = "stun:stun.l.google.com:19302"


def credentialed(url: str) -> rw.IceServer:
    return rw.IceServer(urls=[url], username="alice", password="secret")


ACCEPTED = [
    ("no ice servers", []),
    ("stun only", [rw.IceServer(urls=[STUN])]),
    ("turn with credentials", [credentialed("turn:turn.example.com:3478")]),
    ("turns with credentials", [credentialed("turns:turn.example.com:443")]),
    (
        "stun plus credentialed turn and turns",
        [
            rw.IceServer(urls=[STUN]),
            credentialed("turn:turn.example.com:3478?transport=udp"),
            credentialed("turns:turn.example.com:443?transport=tcp"),
        ],
    ),
    (
        "two turn servers with distinct credentials",
        [
            rw.IceServer(
                urls=["turn:a.example.com:3478"], username="alice", password="secret-a"
            ),
            rw.IceServer(
                urls=["turn:b.example.com:3478"], username="bob", password="secret-b"
            ),
        ],
    ),
    (
        "one entry holding both turn and turns",
        [
            rw.IceServer(
                urls=["turn:turn.example.com:3478", "turns:turn.example.com:443"],
                username="alice",
                password="secret",
            )
        ],
    ),
]


class TestNativeAcceptance:
    @pytest.mark.parametrize("label,servers", ACCEPTED, ids=[c[0] for c in ACCEPTED])
    def test_libwebrtc_accepts(self, factory, label, servers):
        config = rw.RtcConfiguration(ice_servers=servers)
        assert factory.create_peer_connection(config, rw.PeerConnectionObserver())

    def test_turn_without_credentials_reports_the_reason(self, factory):
        config = rw.RtcConfiguration(
            ice_servers=[rw.IceServer(urls=["turn:turn.example.com:3478"])]
        )
        with pytest.raises(RuntimeError, match="username or password"):
            factory.create_peer_connection(config, rw.PeerConnectionObserver())

    def test_credentials_may_hold_any_characters(self, factory):
        # A credential is never parsed as a URL, so JSON punctuation and a
        # literal "turn:" inside a password are harmless.
        config = rw.RtcConfiguration(
            ice_servers=[
                rw.IceServer(
                    urls=["turns:turn.example.com:443?transport=tcp"],
                    username="1753790400:user",
                    password='p"a\\ss:turn:x',
                )
            ]
        )
        assert factory.create_peer_connection(config, rw.PeerConnectionObserver())


class TestPolicies:
    def test_defaults(self):
        c = rw.RtcConfiguration()
        assert c.ice_transport_type == "all"
        assert c.continual_gathering_policy == "once"

    @pytest.mark.parametrize("value", ["all", "relay", "no_host", "none"])
    def test_ice_transport_type_round_trip(self, value):
        assert rw.RtcConfiguration(ice_transport_type=value).ice_transport_type == value
        c = rw.RtcConfiguration()
        c.ice_transport_type = value
        assert c.ice_transport_type == value

    @pytest.mark.parametrize("value", ["once", "continually"])
    def test_continual_gathering_policy_round_trip(self, value):
        config = rw.RtcConfiguration(continual_gathering_policy=value)
        assert config.continual_gathering_policy == value
        c = rw.RtcConfiguration()
        c.continual_gathering_policy = value
        assert c.continual_gathering_policy == value

    def test_unknown_ice_transport_type_raises(self):
        with pytest.raises(ValueError, match="unknown ice_transport_type"):
            rw.RtcConfiguration(ice_transport_type="relay-only")
        with pytest.raises(ValueError, match="unknown ice_transport_type"):
            rw.RtcConfiguration().ice_transport_type = "relay-only"

    def test_unknown_gathering_policy_raises(self):
        with pytest.raises(ValueError, match="unknown continual_gathering_policy"):
            rw.RtcConfiguration(continual_gathering_policy="always")
        with pytest.raises(ValueError, match="unknown continual_gathering_policy"):
            rw.RtcConfiguration().continual_gathering_policy = "always"

    def test_libwebrtc_accepts_relay_only(self, factory):
        config = rw.RtcConfiguration(
            ice_servers=[credentialed("turn:turn.example.com:3478")],
            ice_transport_type="relay",
            continual_gathering_policy="once",
        )
        assert factory.create_peer_connection(config, rw.PeerConnectionObserver())


class TestPortRange:
    def test_explicit_range_accepted(self, factory):
        config = rw.RtcConfiguration(min_port=10000, max_port=10100)
        assert factory.create_peer_connection(config, rw.PeerConnectionObserver())

    def test_min_only_rejected(self, factory):
        config = rw.RtcConfiguration(min_port=10000)
        with pytest.raises(RuntimeError, match="both min_port and max_port"):
            factory.create_peer_connection(config, rw.PeerConnectionObserver())

    def test_max_only_rejected(self, factory):
        config = rw.RtcConfiguration(max_port=10100)
        with pytest.raises(RuntimeError, match="both min_port and max_port"):
            factory.create_peer_connection(config, rw.PeerConnectionObserver())

    def test_inverted_range_rejected(self, factory):
        config = rw.RtcConfiguration(min_port=10100, max_port=10000)
        with pytest.raises(RuntimeError, match="min_port.*max_port"):
            factory.create_peer_connection(config, rw.PeerConnectionObserver())
