# reactor-webrtc-dc-chunking

Chunking, reassembly and send pacing for WebRTC data channels, shared by
[reactor-webrtc](../reactor-webrtc) (native peers) and reactor-wasm (browsers).

A large data-channel message is slow over dcsctp — it ramps 4 → 8 → 16 packets
per round trip — and libwebrtc closes a channel when more than 16 MiB wait in
its send buffer. With this crate a sender splits a message into frames and
feeds them only as fast as the channel drains; the receiver reassembles them.

It is **sans-I/O**: no threads, no sockets, no libwebrtc, no dependencies. It
builds for native targets and for `wasm32-unknown-unknown`.

| Module | What it does |
|---|---|
| `frame` | The one-byte header on every frame: `MORE`, `TEXT`, version bits. |
| `sdp` | `a=x-reactor-dc-chunking:1 max-message-size=N`: declare, parse, effective limit. |
| `send` | `SendQueue`: messages in, frames out, gated by the native buffer's high-water mark. |
| `recv` | `Reassembler`: frames in, whole messages out; oversized messages dropped. |

`tests/vectors.txt` holds the golden vectors for the wire format. Any other
implementation must produce and accept exactly those bytes.
