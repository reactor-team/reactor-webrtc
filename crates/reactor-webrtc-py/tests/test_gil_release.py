"""Native calls that dispatch to a libwebrtc thread must not hold the GIL.

libwebrtc runs its own signalling and worker threads, and this binding's
callbacks re-enter Python from them. Most of the API is a proxy: the call is
posted to one of those threads and the caller blocks until it finishes. A method
that holds the GIL across that dispatch deadlocks the process whenever the target
thread is inside a Python callback — the caller waits for the thread, the thread
waits for the GIL, and neither can be interrupted, because the caller is in
native code and never yields. Releasing a handle is the same dispatch, so a
``Drop`` that runs under the GIL deadlocks just as readily as a method call.

Each case here drives one such operation with a callback deliberately parked in
Python, so the window is guaranteed rather than raced. They run in a subprocess:
the deadlock is not recoverable in-process (a watchdog thread would need the GIL
the stuck caller holds), and a subprocess turns the hang into a failure with a
timeout. It also keeps each case to one ``PeerConnectionFactory``, which
``conftest`` requires.
"""

from __future__ import annotations

import subprocess
import sys
import textwrap

import pytest

# Long enough that a slow machine finishes the work, short enough to fail a
# stuck run inside a normal test session.
_TIMEOUT_S = 45

# ``setup`` runs before ICE gathering starts; ``call`` runs with the signalling
# thread parked inside a Python callback. Splitting them keeps the operation
# under test, and nothing else, inside that window.
_PROGRAM = """
    import asyncio, threading, time
    import reactor_webrtc as rw

    factory = rw.PeerConnectionFactory()

    # Fires on the signalling thread. The sleep releases the GIL, so the main
    # thread is free to take it, and then the callback needs it back to return
    # into native code — exactly the state a deadlocking caller traps it in.
    #
    # Only the first candidate parks the thread, and only long enough for the
    # main thread to reach the operation under test. Parking on every candidate
    # would queue that operation behind all of them, which reads as a stall of
    # its own.
    entered = threading.Event()
    parked = False

    def hold_the_signalling_thread(*_args):
        global parked
        entered.set()
        if not parked:
            parked = True
            time.sleep(2.0)

    async def main():
        observer = rw.PeerConnectionObserver()
        observer.on_ice_candidate = hold_the_signalling_thread
        pc = factory.create_peer_connection(rw.RtcConfiguration(), observer)
        pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.RecvOnly)
        {setup}
        offer = await pc.create_offer()
        await pc.set_local_description(offer)
        assert entered.wait(30), "no ICE candidate was delivered"
        {call}

    asyncio.run(main())
    print("returned")
"""

# name -> (setup, operation under test). The name is what a failure reports, so
# it says which operation held the GIL.
CASES = {
    "create_peer_connection": (
        "",
        'factory.create_peer_connection(rw.RtcConfiguration(), rw.PeerConnectionObserver())',
    ),
    "create_video_track": ("", 'video = factory.create_video_track("probe-video")'),
    "create_audio_track": ("", 'audio = factory.create_audio_track("probe-audio")'),
    "create_audio_track_with_local_source": (
        "",
        'local = factory.create_audio_track_with_local_source("probe-local")',
    ),
    "add_transceiver": (
        "",
        "extra = pc.add_transceiver(rw.MediaKind.Audio, rw.TransceiverDirection.RecvOnly)",
    ),
    "create_data_channel": ("", 'channel = pc.create_data_channel("probe-data")'),
    "set_track": (
        'sender = pc.add_transceiver(rw.MediaKind.Video, rw.TransceiverDirection.SendOnly); '
        'cam = factory.create_video_track("cam")',
        "await sender.set_track(cam)",
    ),
    "on_message": (
        'channel = pc.create_data_channel("probe-messages")',
        "channel.on_message(lambda *_: None)",
    ),
    "on_video_frame": (
        'sink = factory.create_video_track("sink-probe")',
        "sink.on_video_frame(lambda *_: None)",
    ),
    "drop_track": ('doomed = factory.create_video_track("doomed")', "del doomed"),
    "drop_transceiver": (
        "doomed = pc.add_transceiver(rw.MediaKind.Audio, rw.TransceiverDirection.RecvOnly)",
        "del doomed",
    ),
    "drop_peer_connection": (
        "doomed = factory.create_peer_connection(rw.RtcConfiguration(), "
        "rw.PeerConnectionObserver())",
        "del doomed",
    ),
    "drop_data_channel": ('doomed = pc.create_data_channel("doomed")', "del doomed"),
}


def _program(setup: str, call: str) -> str:
    """Render the program. Both fragments are single statements, placed inline."""
    return textwrap.dedent(_PROGRAM).format(setup=setup, call=call)


@pytest.mark.parametrize("name", list(CASES))
def test_the_operation_completes_while_a_callback_holds_the_signalling_thread(name: str) -> None:
    """Run *name* with the signalling thread parked in Python; it must complete."""
    setup, call = CASES[name]
    try:
        done = subprocess.run(
            [sys.executable, "-c", _program(setup, call)],
            capture_output=True,
            text=True,
            timeout=_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        pytest.fail(
            f"{name} did not complete in {_TIMEOUT_S}s: it holds the GIL across a "
            f"dispatch to a libwebrtc thread that is waiting for the GIL"
        )
    assert done.returncode == 0, f"{name} exited {done.returncode}:\n{done.stderr}"
    assert "returned" in done.stdout
