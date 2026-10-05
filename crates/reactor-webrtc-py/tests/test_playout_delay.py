"""Video buffering controls: playout delay on the builder, the jitter buffer
minimum delay on a transceiver, and the jitter buffer stats that show both.

Their effect on a running stream is asserted by the Rust suite
(crates/reactor-webrtc/tests/playout_delay.rs), which can build a factory per
setting. Here the session-scoped factory rules that out (see conftest.py), so
these tests cover what the binding itself adds: argument conversion, its
errors, and the new attributes.
"""

import math

import pytest
import reactor_webrtc as rw


class TestBuilderPlayoutDelay:
    @pytest.mark.parametrize("method", ["with_send_playout_delay", "with_receive_playout_delay"])
    @pytest.mark.parametrize("bad", [-0.001, math.nan, math.inf])
    def test_a_value_that_is_not_a_duration_is_refused(self, method, bad):
        b = rw.PeerConnectionFactoryBuilder()
        with pytest.raises(ValueError):
            getattr(b, method)(min_s=0.0, max_s=bad)
        with pytest.raises(ValueError):
            getattr(b, method)(min_s=bad, max_s=1.0)

    @pytest.mark.parametrize("method", ["with_send_playout_delay", "with_receive_playout_delay"])
    def test_inverted_limits_are_refused(self, method):
        b = rw.PeerConnectionFactoryBuilder()
        with pytest.raises(ValueError, match="min_s"):
            getattr(b, method)(min_s=0.2, max_s=0.1)

    @pytest.mark.parametrize("method", ["with_send_playout_delay", "with_receive_playout_delay"])
    def test_limits_beyond_the_extension_are_refused(self, method):
        b = rw.PeerConnectionFactoryBuilder()
        with pytest.raises(ValueError, match="max_s"):
            getattr(b, method)(min_s=0.0, max_s=41.0)
        getattr(b, method)(min_s=0.0, max_s=40.95)

    def test_defaults_ask_for_immediate_playout(self):
        # Accepted without arguments. Never built: that would start a second
        # factory alongside the session one (see conftest.py).
        b = rw.PeerConnectionFactoryBuilder()
        b.with_send_playout_delay()
        b.with_receive_playout_delay()


class TestJitterBufferMinimumDelay:
    def _transceiver(self, factory):
        pc = factory.create_peer_connection(rw.RtcConfiguration(), rw.PeerConnectionObserver())
        return pc, pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.RecvOnly)

    async def test_set_and_clear(self, factory):
        _pc, tx = self._transceiver(factory)
        await tx.set_jitter_buffer_minimum_delay(0.25)
        await tx.set_jitter_buffer_minimum_delay(None)

    @pytest.mark.parametrize("bad", [-1.0, math.nan, 11.0])
    async def test_out_of_range_is_refused(self, factory, bad):
        _pc, tx = self._transceiver(factory)
        with pytest.raises(ValueError):
            await tx.set_jitter_buffer_minimum_delay(bad)


def test_inbound_stats_carry_the_jitter_buffer_fields():
    for name in (
        "jitter_buffer_delay_s",
        "jitter_buffer_target_delay_s",
        "jitter_buffer_minimum_delay_s",
        "jitter_buffer_emitted_count",
        "average_jitter_buffer_delay_s",
    ):
        assert hasattr(rw.InboundRtpStats, name), name
