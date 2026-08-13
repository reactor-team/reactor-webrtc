"""Capture timestamps on the outbound push paths.

Audio and video are synchronised by sharing a capture time, not by reaching the
encoder at the same moment, so `time_micros()` and the `capture_time_us`
arguments have to be usable together from Python: one clock read, stamped onto
every track a unit of media spans.
"""

import time

import reactor_webrtc as rw

SAMPLE_RATE = 48_000
FRAME_SAMPLES = 480  # 10 ms at 48 kHz
SILENCE = b"\x00\x00" * FRAME_SAMPLES


def bgra(width: int = 8, height: int = 8) -> bytes:
    return b"\x00\x00\x00\xff" * (width * height)


class TestTimeMicros:
    def test_returns_an_integer_microsecond_clock(self):
        assert isinstance(rw.time_micros(), int)

    def test_does_not_run_backwards(self):
        readings = [rw.time_micros() for _ in range(100)]
        assert readings == sorted(readings)

    def test_ticks_in_microseconds(self):
        """The unit has to be right, or a caller's stamps land on a wrong scale."""
        before = rw.time_micros()
        time.sleep(0.02)
        elapsed = rw.time_micros() - before
        # 20 ms is 20_000 µs; the upper bound is loose enough for a busy host.
        assert 10_000 < elapsed < 2_000_000


class TestAudioCaptureTime:
    def test_push_pcm_accepts_a_capture_time(self, factory):
        track = factory.create_audio_track_with_local_source("a")
        track.push_pcm(SILENCE, SAMPLE_RATE, 1, capture_time_us=rw.time_micros())

    def test_push_pcm_capture_time_is_optional(self, factory):
        track = factory.create_audio_track_with_local_source("a")
        track.push_pcm(SILENCE, SAMPLE_RATE, 1)

    def test_push_pcm_accepts_the_capture_time_positionally(self, factory):
        track = factory.create_audio_track_with_local_source("a")
        track.push_pcm(SILENCE, SAMPLE_RATE, 1, rw.time_micros())

    def test_push_pcm_still_validates_its_input(self, factory):
        track = factory.create_audio_track_with_local_source("a")
        try:
            track.push_pcm(b"\x00", SAMPLE_RATE, 1, capture_time_us=rw.time_micros())
        except RuntimeError:
            return
        raise AssertionError("an odd byte length must still be rejected")


class TestVideoCaptureTime:
    def test_push_video_frame_accepts_a_capture_time(self, factory):
        track = factory.create_video_track("v")
        track.push_video_frame(bgra(), 8, 8, capture_time_us=rw.time_micros())

    def test_a_capture_time_composes_with_metadata(self, factory):
        """Carrying a trailer must not cost the frame its timestamp."""
        track = factory.create_video_track("v")
        track.push_video_frame(
            bgra(), 8, 8, user_data=b"frame-1", capture_time_us=rw.time_micros()
        )

    def test_neither_argument_is_required(self, factory):
        track = factory.create_video_track("v")
        track.push_video_frame(bgra(), 8, 8)

    def test_metadata_alone_still_works(self, factory):
        track = factory.create_video_track("v")
        track.push_video_frame(bgra(), 8, 8, user_data=b"frame-1")

    def test_frames_stamped_inside_one_millisecond_are_accepted(self, factory):
        """The trailer key is nudged rather than the push refused."""
        track = factory.create_video_track("v")
        now = rw.time_micros()
        for index in range(5):
            track.push_video_frame(
                bgra(), 8, 8, user_data=f"frame-{index}".encode(), capture_time_us=now
            )

    def test_push_video_frame_still_validates_its_buffer(self, factory):
        track = factory.create_video_track("v")
        try:
            track.push_video_frame(b"\x00", 8, 8, capture_time_us=rw.time_micros())
        except RuntimeError:
            return
        raise AssertionError("a short buffer must still be rejected")


class TestOneClockReadStampsBothTracks:
    def test_audio_and_video_take_the_same_capture_time(self, factory):
        """The shape the runtime is expected to use, exercised end to end."""
        video = factory.create_video_track("v")
        audio = factory.create_audio_track_with_local_source("a")

        now = rw.time_micros()
        video.push_video_frame(bgra(), 8, 8, capture_time_us=now)
        audio.push_pcm(SILENCE, SAMPLE_RATE, 1, capture_time_us=now)
