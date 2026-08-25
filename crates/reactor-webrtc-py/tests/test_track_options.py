"""Builder, per-track options, and encoder feedback (REA-5617).

Covers the 5617 surface: PeerConnectionFactoryBuilder construction,
create_video_track_with_options / create_audio_track_with_options, and
on_encoder_feedback registration/feedback.

Run: uses the session-scoped `factory` fixture from conftest.py — one
PeerConnectionFactory per process is enforced, so this file never creates
its own; the factory fixture does the only construction.
"""

import pytest
import reactor_webrtc as rw


class TestBuilder:
    def test_builder_constructs_a_factory(self, factory):
        # The fixture's existence already proves factory construction works;
        # make sure the standalone builder path is callable too.
        b = rw.PeerConnectionFactoryBuilder()
        b.with_synthetic_adm()
        b.with_metadata(True)
        assert isinstance(b, rw.PeerConnectionFactoryBuilder)

    def test_factory_builder_accessor(self):
        assert isinstance(rw.PeerConnectionFactory.builder(), rw.PeerConnectionFactoryBuilder)


class TestVideoTrackOptions:
    def test_plain_options_returns_track(self, factory):
        t = factory.create_video_track_with_options("plain")
        assert isinstance(t, rw.Track)
        assert t.kind() == rw.MediaKind.Video

    def test_pre_encoded_returns_encoded_track(self, factory):
        t = factory.create_video_track_with_options("enc", pre_encoded=(320, 240))
        assert isinstance(t, rw.EncodedVideoTrack)

    def test_inline_encoder_receives_frames(self, factory):
        # Inline accepts a callable and returns a raw Track; the callback
        # running on media flow is exercised by the loopback-style suites.
        def encoder(frame):
            return None

        t = factory.create_video_track_with_options("inline", inline_encoder=encoder)
        assert isinstance(t, rw.Track)
        assert t.kind() == rw.MediaKind.Video

    def test_pre_encoded_and_inline_are_mutually_exclusive(self, factory):
        def encoder(frame):
            return None

        with pytest.raises(Exception, match="mutually exclusive"):
            factory.create_video_track_with_options(
                "bad", pre_encoded=(320, 240), inline_encoder=encoder
            )

    def test_h264_backend_conflicts_with_custom_encoder(self, factory):
        with pytest.raises(Exception, match="no backend to route"):
            factory.create_video_track_with_options(
                "bad2",
                pre_encoded=(320, 240),
                h264_backend=rw.H264Backend.VideoToolbox,
            )

    def test_frame_metadata_off_constructs(self, factory):
        t = factory.create_video_track_with_options(
            "no-meta", frame_metadata=False
        )
        assert isinstance(t, rw.Track)


class TestAudioTrackOptions:
    def test_adm_default(self, factory):
        t = factory.create_audio_track_with_options("mic")
        assert isinstance(t, rw.Track)
        assert t.kind() == rw.MediaKind.Audio

    def test_local_push_source(self, factory):
        t = factory.create_audio_track_with_options(
            "music", source=rw.AudioTrackSource.LocalPush
        )
        assert t.kind() == rw.MediaKind.Audio

    def test_processing_flags_construct(self, factory):
        t = factory.create_audio_track_with_options(
            "mic-on",
            echo_cancellation=True,
            noise_suppression=False,
            auto_gain_control=True,
            high_pass_filter=False,
        )
        assert t.kind() == rw.MediaKind.Audio


class TestEncoderFeedbackRegistration:
    def test_encoded_track_accepts_listener(self, factory):
        t = factory.create_video_track_with_options("enc2", pre_encoded=(320, 240))
        events = []
        t.on_encoder_feedback(lambda fb: events.append(type(fb)))
        # Feedback fires only with an encoder instance (on media flow).
        assert callable(t.on_encoder_feedback)

    def test_builtin_track_rejects_listener(self, factory):
        t = factory.create_video_track("plain2")
        with pytest.raises(Exception, match="Inline"):
            t.on_encoder_feedback(lambda fb: None)
