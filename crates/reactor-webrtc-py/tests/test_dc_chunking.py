"""Data-channel chunking through the Python bindings.

The factory here is built with ``with_dc_chunking``; this file overrides the
session ``factory`` fixture, so (run one file per process, as the mise task
does) it is the only factory alive. A "plain" peer is a connection that opts
out with ``RtcConfiguration(dc_chunking=False)``.
"""

import asyncio
import threading
import time

import pytest
import reactor_webrtc as rw

MIB = 1024 * 1024
TIMEOUT = 20.0


@pytest.fixture(scope="session")
def factory() -> rw.PeerConnectionFactory:
    builder = rw.PeerConnectionFactoryBuilder()
    builder.with_dc_chunking()
    return builder.build()


class Peer:
    def __init__(self, factory: rw.PeerConnectionFactory, config: rw.RtcConfiguration):
        self.ice: list = []
        self.channels: list = []
        obs = rw.PeerConnectionObserver()
        obs.on_ice_candidate = self.ice.append
        obs.on_data_channel = self.channels.append
        self.pc = factory.create_peer_connection(config, obs)


async def wait_for(condition, timeout: float = TIMEOUT) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        await asyncio.sleep(0.02)


async def connect(factory, chunk_a: bool = True, chunk_b: bool = True):
    """Two peers with one data channel; returns (a, b, channel_a, channel_b)."""
    a = Peer(factory, rw.RtcConfiguration(dc_chunking=chunk_a))
    b = Peer(factory, rw.RtcConfiguration(dc_chunking=chunk_b))
    dc_a = a.pc.create_data_channel("data")
    offer = await a.pc.create_offer()
    await a.pc.set_local_description(offer)
    await b.pc.set_remote_description(offer)
    answer = await b.pc.create_answer()
    await b.pc.set_local_description(answer)
    await a.pc.set_remote_description(answer)

    def trickle() -> bool:
        for src, dst in ((a, b), (b, a)):
            while src.ice:
                asyncio.ensure_future(dst.pc.add_ice_candidate(src.ice.pop(0)))
        return (
            dc_a.state() == rw.DataChannelState.Open
            and bool(b.channels)
            and b.channels[0].state() == rw.DataChannelState.Open
        )

    await wait_for(trickle)
    return a, b, dc_a, b.channels[0]


def inbox(dc: rw.DataChannel) -> list:
    got: list = []
    dc.on_message(lambda data, binary: got.append((data, binary)))
    return got


def pattern(seed: int, n: int) -> bytes:
    block = bytes((seed * 31 + i * 7) & 0xFF for i in range(256))
    return (block * (n // 256 + 1))[:n]


def test_with_dc_chunking_rejects_settings_that_cannot_work():
    builder = rw.PeerConnectionFactoryBuilder()
    builder.with_dc_chunking(chunk_size=1)
    with pytest.raises(RuntimeError, match="chunk_size"):
        builder.build()


async def test_the_offer_declares_chunking_unless_the_connection_opts_out(factory):
    for opt_in, expected in ((True, True), (False, False)):
        p = Peer(factory, rw.RtcConfiguration(dc_chunking=opt_in))
        p.pc.create_data_channel("probe")
        offer = await p.pc.create_offer()
        assert ("a=x-reactor-dc-chunking:1 max-message-size=" in offer.sdp) is expected


async def test_a_large_message_arrives_whole_on_a_chunked_channel(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    assert dc_a.is_chunked() and dc_b.is_chunked()
    assert dc_a.ordered() and dc_a.reliable()
    got = inbox(dc_b)
    msg = pattern(1, 20 * MIB)
    dc_a.send(msg)
    await wait_for(lambda: got, timeout=60)
    data, binary = got[0]
    assert binary and data == msg


async def test_text_split_across_frames_arrives_as_text(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    got = inbox(dc_b)
    text = "€uro ünïcödé ✓ " * 40_000
    dc_a.send(text.encode(), binary=False)
    await wait_for(lambda: got, timeout=30)
    data, binary = got[0]
    assert not binary and data.decode() == text


async def test_a_peer_that_opts_out_keeps_both_ends_plain(factory):
    _a, _b, dc_a, dc_b = await connect(factory, chunk_a=True, chunk_b=False)
    assert not dc_a.is_chunked() and not dc_b.is_chunked()
    got = inbox(dc_b)
    dc_a.send(b"plain")
    await wait_for(lambda: got)
    assert got[0] == (b"plain", True)


async def test_drain_waits_for_the_queue_and_send_releases_the_gil(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    got = inbox(dc_b)
    ticks = 0
    stop = threading.Event()

    def ticker():
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0.001)

    t = threading.Thread(target=ticker)
    t.start()
    # 50 MiB is past libwebrtc's 16 MiB buffer; how much of it is still
    # queued when send returns depends on how fast the link drains (Windows
    # loopback keeps up with the sender), so only the outcome is asserted.
    dc_a.send(pattern(2, 50 * MIB))
    assert await dc_a.drain(timeout=60) is True
    stop.set()
    t.join()
    assert dc_a.buffered_amount() == 0
    assert ticks > 10, "a Python thread kept running while the transfer drained"
    await wait_for(lambda: got, timeout=60)


async def test_a_full_queue_raises_queue_full_and_low_threshold_fires(factory):
    # A connection of this factory has a 128 MiB queue; fill it past that.
    # From one thread a fast link (Windows loopback) can drain each send as it
    # is pumped, so the queue never fills. Several threads send at once: while
    # one pumps, the others' sends only queue, which fills it at any speed.
    _a, _b, dc_a, dc_b = await connect(factory)
    inbox(dc_b)
    fired = threading.Event()
    dc_a.set_buffered_amount_low_threshold(1 * MIB)
    dc_a.on_buffered_amount_low(fired.set)
    msg = pattern(3, 60 * MIB)
    refused = threading.Event()

    def sender() -> None:
        for _ in range(20):
            if refused.is_set():
                return
            try:
                dc_a.send(msg)
            except rw.DataChannelQueueFull:
                refused.set()
                return

    threads = [threading.Thread(target=sender) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        await asyncio.get_running_loop().run_in_executor(None, t.join)
    assert refused.is_set(), "eight threads sending 60 MiB never filled a 128 MiB queue"
    assert issubclass(rw.DataChannelQueueFull, RuntimeError)
    await wait_for(fired.is_set, timeout=120)
    assert dc_a.state() == rw.DataChannelState.Open


async def test_an_oversized_message_raises_message_too_large(factory):
    _a, _b, dc_a, _dc_b = await connect(factory)
    with pytest.raises(rw.DataChannelMessageTooLarge):
        dc_a.send(b"\0" * (64 * MIB + 1))
    assert dc_a.state() == rw.DataChannelState.Open


async def test_close_sends_what_was_queued_first(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    got = inbox(dc_b)
    msg = pattern(4, 30 * MIB)
    dc_a.send(msg)
    await asyncio.get_running_loop().run_in_executor(None, dc_a.close, 60.0)
    await wait_for(lambda: got, timeout=60)
    assert got[0][0] == msg


async def test_on_close_and_on_state_change_both_fire(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    closed = threading.Event()
    states: list = []
    dc_b.on_state_change(states.append)
    dc_b.on_close(closed.set)
    dc_a.close()
    await wait_for(lambda: closed.is_set() and rw.DataChannelState.Closed in states)


async def test_drain_and_close_take_inf_and_refuse_nan(factory):
    _a, _b, dc_a, dc_b = await connect(factory)
    got = inbox(dc_b)
    dc_a.send(pattern(5, 2 * MIB))
    assert await dc_a.drain(timeout=float("inf")) is True
    with pytest.raises(ValueError, match="timeout"):
        await dc_a.drain(timeout=float("nan"))
    with pytest.raises(ValueError, match="drain_timeout"):
        dc_a.close(float("nan"))
    assert dc_a.state() == rw.DataChannelState.Open
    dc_a.close(float("inf"))
    await wait_for(lambda: got)


async def test_a_held_message_reaches_a_callback_that_reads_the_channel(factory):
    """Messages held until on_message is set are flushed while it registers;
    the callback can still use the channel it was set on."""
    _a, _b, dc_a, dc_b = await connect(factory)
    dc_a.send(b"held for later")
    await asyncio.sleep(0.5)
    seen: list = []

    def on_message(data: bytes, binary: bool) -> None:
        seen.append((data, binary, dc_b.state()))

    dc_b.on_message(on_message)
    await wait_for(lambda: seen)
    assert seen == [(b"held for later", True, rw.DataChannelState.Open)]


async def test_stats_count_each_channels_messages_and_bytes(factory):
    a, b, dc_a, dc_b = await connect(factory, chunk_a=False, chunk_b=False)
    got = inbox(dc_b)

    dc_a.send(b"ping")
    await wait_for(lambda: got)

    report = await a.pc.get_stats()
    [data] = [c for c in report.data_channels if c.label == "data"]
    assert data.state == rw.DataChannelState.Open
    assert data.id is not None
    assert (data.messages_sent, data.bytes_sent) == (1, 4)
    assert (data.messages_received, data.bytes_received) == (0, 0)


async def test_a_chunked_channel_times_its_messages(factory):
    a, b, dc_a, dc_b = await connect(factory)
    got = inbox(dc_b)
    msg = pattern(5, 4 * MIB)

    dc_a.send(msg)
    await wait_for(lambda: got)

    sent = dc_a.chunking_stats()
    assert sent.messages_sent == 1
    assert sent.frames_sent > 1
    assert sent.send_s > 0
    received = dc_b.chunking_stats()
    assert received.messages_received == 1
    assert received.frames_received == sent.frames_sent
    assert received.reassembly_s > 0


async def test_a_plain_channel_has_no_chunking_stats(factory):
    _, _, dc_a, _ = await connect(factory, chunk_a=False, chunk_b=False)
    assert dc_a.chunking_stats() is None
