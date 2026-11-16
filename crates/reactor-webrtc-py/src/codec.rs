//! Python bindings for `reactor_webrtc::codec`: standalone video encoders and
//! decoders, without a peer connection.
//!
//! Frames cross the boundary as numpy `uint8` arrays of shape `(H, W, 3)`
//! (RGB). numpy is imported on first use, so `import reactor_webrtc` does not
//! need it. Every encode and decode runs with the GIL released; the native
//! encoder or decoder sits behind a mutex, so two Python threads calling the
//! same one take turns instead of failing.

use ::reactor_webrtc as rw;
use pyo3::exceptions::{PyImportError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes};
use rw::codec as rc;
use std::sync::Mutex;

use crate::VideoCodec;

// pyo3's create_exception! expands to a cfg check on its own `gil-refs`
// feature, which this crate does not declare; keep that lint local to it.
#[allow(unexpected_cfgs)]
mod exceptions {
    use pyo3::exceptions::PyRuntimeError;

    pyo3::create_exception!(
        reactor_webrtc,
        CodecError,
        PyRuntimeError,
        "A standalone codec call failed in the backend. An encoder makes its next \
         frame a key frame; a decoder waits for a key frame."
    );
    pyo3::create_exception!(
        reactor_webrtc,
        CodecUnsupported,
        CodecError,
        "No backend in this process runs this codec with these settings. \
         `VideoCodecs.capabilities()` lists the ones that do."
    );
    pyo3::create_exception!(
        reactor_webrtc,
        CodecNeedsKeyFrame,
        CodecError,
        "The decoder needs a key frame and was given another frame. Nothing was \
         decoded, and the decoder still waits for a key frame."
    );
}
pub use exceptions::{CodecError, CodecNeedsKeyFrame, CodecUnsupported};

fn codec_err(e: rc::CodecError) -> PyErr {
    match e {
        rc::CodecError::Unsupported(_) => CodecUnsupported::new_err(e.to_string()),
        rc::CodecError::InvalidInput(_) => PyValueError::new_err(e.to_string()),
        rc::CodecError::NeedKeyFrame => CodecNeedsKeyFrame::new_err(e.to_string()),
        _ => CodecError::new_err(e.to_string()),
    }
}

impl From<rw::VideoCodec> for VideoCodec {
    fn from(c: rw::VideoCodec) -> Self {
        match c {
            rw::VideoCodec::Vp8 => VideoCodec::Vp8,
            rw::VideoCodec::Vp9 => VideoCodec::Vp9,
            rw::VideoCodec::Av1 => VideoCodec::Av1,
            rw::VideoCodec::H264 => VideoCodec::H264,
            rw::VideoCodec::H265 => VideoCodec::H265,
        }
    }
}

fn numpy(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    py.import_bound("numpy").map_err(|e| {
        PyImportError::new_err(format!(
            "the standalone video codecs exchange frames as numpy arrays; \
             install numpy (pip install 'reactor-webrtc[numpy]'): {e}"
        ))
    })
}

/// Run `f` on a codec with the GIL released, so a thread that holds the
/// codec through a long encode never waits on this one for the GIL.
fn with_codec<T: Send, R: Send>(
    py: Python<'_>,
    codec: &Mutex<T>,
    f: impl FnOnce(&mut T) -> R + Send,
) -> R {
    py.allow_threads(|| {
        // A panic inside a codec call leaves the codec in the state the Rust
        // API documents for a failed call, so a poisoned lock is still usable.
        let mut guard = codec.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    })
}

/// Which backends a codec may be created on.
#[pyclass(eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CodecBackend {
    /// The hardware backend when one accepts the settings, a software one
    /// otherwise.
    Auto,
    /// Hardware only: creation raises `CodecUnsupported` without one.
    Hardware,
    /// Software only.
    Software,
}

impl From<CodecBackend> for rc::Backend {
    fn from(b: CodecBackend) -> Self {
        match b {
            CodecBackend::Auto => rc::Backend::Auto,
            CodecBackend::Hardware => rc::Backend::Hardware,
            CodecBackend::Software => rc::Backend::Software,
        }
    }
}

/// One backend for one codec, in `CodecCapabilities`.
#[pyclass(frozen, get_all)]
pub struct CodecSupport {
    pub codec: VideoCodec,
    /// Whether the backend is the platform's hardware codec.
    pub hardware: bool,
    /// The backend's name, e.g. `"VideoToolbox"`, `"libvpx"`, `"dav1d"`.
    pub implementation: String,
}

#[pymethods]
impl CodecSupport {
    fn __repr__(&self) -> String {
        format!(
            "CodecSupport(codec=VideoCodec.{:?}, hardware={}, implementation={:?})",
            rw::VideoCodec::from(self.codec),
            if self.hardware { "True" } else { "False" },
            self.implementation
        )
    }
}

fn supports(py: Python<'_>, list: Vec<rc::CodecSupport>) -> PyResult<Vec<Py<CodecSupport>>> {
    list.into_iter()
        .map(|s| {
            Py::new(
                py,
                CodecSupport {
                    codec: s.codec.into(),
                    hardware: s.hardware,
                    implementation: s.implementation,
                },
            )
        })
        .collect()
}

/// The codecs this process can encode and decode, one entry per backend,
/// hardware first within each codec.
#[pyclass(frozen, get_all)]
pub struct CodecCapabilities {
    pub encoders: Vec<Py<CodecSupport>>,
    pub decoders: Vec<Py<CodecSupport>>,
}

#[pymethods]
impl CodecCapabilities {
    /// Whether some backend encodes `codec`.
    fn can_encode(&self, py: Python<'_>, codec: VideoCodec) -> bool {
        self.encoders
            .iter()
            .any(|s| s.bind(py).get().codec == codec)
    }

    /// Whether some backend decodes `codec`.
    fn can_decode(&self, py: Python<'_>, codec: VideoCodec) -> bool {
        self.decoders
            .iter()
            .any(|s| s.bind(py).get().codec == codec)
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let names = |list: &[Py<CodecSupport>]| {
            list.iter()
                .map(|s| s.bind(py).get().__repr__())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "CodecCapabilities(encoders=[{}], decoders=[{}])",
            names(&self.encoders),
            names(&self.decoders)
        )
    }
}

/// The video codec backends this process can run, and the place to create
/// standalone encoders and decoders on them.
#[pyclass(frozen)]
pub struct VideoCodecs {
    inner: rc::VideoCodecs,
}

#[pymethods]
impl VideoCodecs {
    /// The platform's own codecs plus the software ones built into libwebrtc.
    #[new]
    fn new(py: Python<'_>) -> PyResult<Self> {
        let inner = py.allow_threads(rc::VideoCodecs::new).map_err(codec_err)?;
        Ok(Self { inner })
    }

    /// The same, plus Cisco's OpenH264 loaded from `lib_path` as the software
    /// H.264 backend (attribution requirements apply). Raises when the
    /// library does not load.
    #[cfg(feature = "openh264")]
    #[staticmethod]
    fn with_openh264(py: Python<'_>, lib_path: &str) -> PyResult<Self> {
        let path = std::path::PathBuf::from(lib_path);
        let inner = py
            .allow_threads(|| rc::VideoCodecs::with_openh264(&path))
            .map_err(codec_err)?;
        Ok(Self { inner })
    }

    /// The codecs this process can encode and decode, and on which backends.
    fn capabilities(&self, py: Python<'_>) -> PyResult<CodecCapabilities> {
        let caps = py.allow_threads(|| self.inner.capabilities());
        Ok(CodecCapabilities {
            encoders: supports(py, caps.encoders)?,
            decoders: supports(py, caps.decoders)?,
        })
    }

    /// Create an encoder for `codec`, starting at `width` x `height` (both
    /// even). Raises `CodecUnsupported` when no backend `backend` allows
    /// accepts it, `ValueError` for a bad size or rate.
    #[pyo3(signature = (codec, width, height, *, bitrate_bps = 2_000_000, framerate = 30.0, backend = CodecBackend::Auto))]
    #[allow(clippy::too_many_arguments)]
    fn encoder(
        &self,
        py: Python<'_>,
        codec: VideoCodec,
        width: u32,
        height: u32,
        bitrate_bps: u32,
        framerate: f64,
        backend: CodecBackend,
    ) -> PyResult<VideoEncoder> {
        let mut config = rc::VideoEncoderConfig::new(codec.into(), width, height);
        config.bitrate_bps = bitrate_bps;
        config.framerate = framerate;
        config.backend = backend.into();
        let inner = py
            .allow_threads(|| self.inner.encoder(&config))
            .map_err(codec_err)?;
        Ok(VideoEncoder {
            codec,
            implementation: inner.implementation().to_owned(),
            hardware: inner.is_hardware(),
            inner: Mutex::new(inner),
        })
    }

    /// Create a decoder for `codec`. Raises `CodecUnsupported` when no
    /// backend `backend` allows has one.
    #[pyo3(signature = (codec, *, backend = CodecBackend::Auto))]
    fn decoder(
        &self,
        py: Python<'_>,
        codec: VideoCodec,
        backend: CodecBackend,
    ) -> PyResult<VideoDecoder> {
        let inner = py
            .allow_threads(|| self.inner.decoder_on(codec.into(), backend.into()))
            .map_err(codec_err)?;
        Ok(VideoDecoder {
            codec,
            implementation: inner.implementation().to_owned(),
            hardware: inner.is_hardware(),
            inner: Mutex::new(inner),
        })
    }
}

/// One encoded frame, as `VideoEncoder.encode` returns it.
#[pyclass(frozen, get_all)]
pub struct EncodedVideoFrame {
    /// The frame in its codec's bitstream format.
    pub data: Py<PyBytes>,
    /// Whether a decoder can start from this frame.
    pub key_frame: bool,
    pub width: u32,
    pub height: u32,
    /// The timestamp given to `encode`, unchanged.
    pub timestamp_us: i64,
}

#[pymethods]
impl EncodedVideoFrame {
    fn __repr__(&self, py: Python<'_>) -> String {
        format!(
            "EncodedVideoFrame({} bytes, key_frame={}, {}x{}, timestamp_us={})",
            self.data.bind(py).as_bytes().len(),
            if self.key_frame { "True" } else { "False" },
            self.width,
            self.height,
            self.timestamp_us
        )
    }
}

/// A video encoder that runs outside a peer connection.
#[pyclass(frozen)]
pub struct VideoEncoder {
    inner: Mutex<rc::VideoEncoder>,
    codec: VideoCodec,
    implementation: String,
    hardware: bool,
}

/// Check `frame` is a numpy `uint8` array of shape `(H, W, 3)` and return
/// `(width, height)` and its pixels in C order.
fn rgb_frame<'py>(frame: &Bound<'py, PyAny>) -> PyResult<(u32, u32, Bound<'py, PyBytes>)> {
    let py = frame.py();
    let np = numpy(py)?;
    if !frame.is_instance(&np.getattr("ndarray")?)? {
        return Err(PyTypeError::new_err(format!(
            "frame must be a numpy array of shape (H, W, 3) and dtype uint8, not {}",
            frame.get_type().name()?
        )));
    }
    if !frame.getattr("dtype")?.eq(np.getattr("uint8")?)? {
        return Err(PyValueError::new_err(format!(
            "frame must have dtype uint8, not {}",
            frame.getattr("dtype")?.str()?
        )));
    }
    let shape: Vec<usize> = frame.getattr("shape")?.extract()?;
    let [h, w, 3] = shape[..] else {
        return Err(PyValueError::new_err(format!(
            "frame must have shape (H, W, 3), not {shape:?}"
        )));
    };
    let (Ok(width), Ok(height)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(PyValueError::new_err(format!(
            "frame size {w}x{h} is too large"
        )));
    };
    // `tobytes()` copies in C order whatever the array's strides; it also
    // gives an immutable buffer that is safe to read with the GIL released.
    let pixels = frame.call_method0("tobytes")?.downcast_into::<PyBytes>()?;
    Ok((width, height, pixels))
}

#[pymethods]
impl VideoEncoder {
    /// The codec this encoder produces.
    #[getter]
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The backend this encoder runs on, e.g. `"VideoToolbox"` or `"libvpx"`.
    #[getter]
    fn implementation(&self) -> &str {
        &self.implementation
    }

    /// Whether that backend is the platform's hardware codec.
    #[getter]
    fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// The target bitrate, in bits per second.
    #[getter]
    fn bitrate_bps(&self, py: Python<'_>) -> u32 {
        with_codec(py, &self.inner, |e| e.bitrate_bps())
    }

    /// The frame rate the bitrate is spread over.
    #[getter]
    fn framerate(&self, py: Python<'_>) -> f64 {
        with_codec(py, &self.inner, |e| e.framerate())
    }

    /// Make the next frame a key frame.
    fn request_key_frame(&self, py: Python<'_>) {
        with_codec(py, &self.inner, |e| e.request_key_frame())
    }

    /// Change the target bitrate and the frame rate it is spread over, from
    /// the next frame on. Raises `ValueError` for 0 or a non-positive rate.
    fn set_rates(&self, py: Python<'_>, bitrate_bps: u32, framerate: f64) -> PyResult<()> {
        with_codec(py, &self.inner, |e| e.set_rates(bitrate_bps, framerate)).map_err(codec_err)
    }

    /// Encode one RGB frame and return its encoded frame, or `None` when the
    /// backend dropped it. Runs with the GIL released.
    #[pyo3(signature = (frame, timestamp_us = 0))]
    fn encode(
        &self,
        py: Python<'_>,
        frame: &Bound<'_, PyAny>,
        timestamp_us: i64,
    ) -> PyResult<Option<EncodedVideoFrame>> {
        let (width, height, pixels) = rgb_frame(frame)?;
        let pixels = pixels.as_bytes();
        let encoded = with_codec(py, &self.inner, |e| {
            e.encode(pixels, width, height, rw::PixelFormat::Rgb24, timestamp_us)
        })
        .map_err(codec_err)?;
        Ok(encoded.map(|f| EncodedVideoFrame {
            data: PyBytes::new_bound(py, &f.data).unbind(),
            key_frame: f.key_frame,
            width: f.width,
            height: f.height,
            timestamp_us: f.timestamp_us,
        }))
    }

    fn __repr__(&self) -> String {
        format!(
            "VideoEncoder(codec=VideoCodec.{:?}, implementation={:?})",
            rw::VideoCodec::from(self.codec),
            self.implementation
        )
    }
}

/// A video decoder that runs outside a peer connection. It keeps its
/// reference frames between calls.
#[pyclass(frozen)]
pub struct VideoDecoder {
    inner: Mutex<rc::VideoDecoder>,
    codec: VideoCodec,
    implementation: String,
    hardware: bool,
}

#[pymethods]
impl VideoDecoder {
    /// The codec this decoder reads.
    #[getter]
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The backend this decoder runs on, e.g. `"VideoToolbox"` or `"dav1d"`.
    #[getter]
    fn implementation(&self) -> &str {
        &self.implementation
    }

    /// Whether that backend is the platform's hardware codec.
    #[getter]
    fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Whether the next frame must be a key frame.
    #[getter]
    fn needs_key_frame(&self, py: Python<'_>) -> bool {
        with_codec(py, &self.inner, |d| d.needs_key_frame())
    }

    /// Forget every frame decoded so far: the next frame must be a key frame.
    fn reset(&self, py: Python<'_>) -> PyResult<()> {
        with_codec(py, &self.inner, |d| d.reset()).map_err(codec_err)
    }

    /// Decode one encoded frame and return its picture as a numpy `uint8`
    /// array of shape `(H, W, 3)` (RGB), or `None` when the frame gave no
    /// picture. Runs with the GIL released.
    fn decode<'py>(
        &self,
        py: Python<'py>,
        data: &Bound<'py, PyBytes>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let np = numpy(py)?;
        let bytes = data.as_bytes();
        let decoded = with_codec(py, &self.inner, |d| {
            d.decode(bytes, 0, rw::PixelFormat::Rgb24)
        })
        .map_err(codec_err)?;
        let Some(picture) = decoded else {
            return Ok(None);
        };
        // A bytearray, not bytes, so the array numpy wraps around it without
        // another copy is writable.
        let buffer = PyByteArray::new_bound(py, &picture.data);
        let array = np
            .call_method1("frombuffer", (buffer, np.getattr("uint8")?))?
            .call_method1(
                "reshape",
                (picture.height as usize, picture.width as usize, 3usize),
            )?;
        Ok(Some(array))
    }

    fn __repr__(&self) -> String {
        format!(
            "VideoDecoder(codec=VideoCodec.{:?}, implementation={:?})",
            rw::VideoCodec::from(self.codec),
            self.implementation
        )
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<CodecBackend>()?;
    m.add_class::<CodecSupport>()?;
    m.add_class::<CodecCapabilities>()?;
    m.add_class::<VideoCodecs>()?;
    m.add_class::<EncodedVideoFrame>()?;
    m.add_class::<VideoEncoder>()?;
    m.add_class::<VideoDecoder>()?;
    m.add("CodecError", py.get_type_bound::<CodecError>())?;
    m.add("CodecUnsupported", py.get_type_bound::<CodecUnsupported>())?;
    m.add(
        "CodecNeedsKeyFrame",
        py.get_type_bound::<CodecNeedsKeyFrame>(),
    )?;
    Ok(())
}
