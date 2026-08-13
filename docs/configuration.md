# Configuration reference

Everything you can tune on a `PeerConnection`: what you pass to
`create_peer_connection` via `RtcConfiguration`, and what you can change
afterwards via `set_bitrate`. Examples are given in Rust and Python;
the fields and defaults are identical across both bindings. Each snippet
is self-contained (imports included) except where noted that it assumes
an existing `pc`.

- [ICE servers](#ice-servers)
- [ICE transport policy](#ice-transport-policy)
- [Continual gathering policy](#continual-gathering-policy)
- [Port range](#port-range)
- [Bundle policy](#bundle-policy)
- [TCP candidates](#tcp-candidates)
- [ICE timeouts](#ice-timeouts)
- [Congestion-control bitrate limits](#congestion-control-bitrate-limits)

## ICE servers

STUN and/or TURN servers offered for candidate gathering. A `turn:`/`turns:`
URL needs both `username` and `password` — libwebrtc rejects the whole
configuration if a credentialed TURN entry is missing either.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{IceServer, RtcConfiguration};

let config = RtcConfiguration {
    ice_servers: vec![
        IceServer { urls: vec!["stun:stun.l.google.com:19302".into()], ..Default::default() },
        IceServer {
            urls: vec!["turn:turn.example.com:3478".into()],
            username: "alice".into(),
            password: "secret".into(),
        },
    ],
    ..Default::default()
};
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(ice_servers=[
    rw.IceServer(urls=["stun:stun.l.google.com:19302"]),
    rw.IceServer(urls=["turn:turn.example.com:3478"], username="alice", password="secret"),
])
```

</details>

Default: empty (no STUN/TURN — only host candidates are gathered).

## ICE transport policy

Which candidate types libwebrtc is allowed to use.

| Value | Meaning |
|-------|---------|
| `All` (default) | Host, server-reflexive, and relay candidates |
| `Relay` | Relay (TURN) candidates only — forces every flow through a TURN server |
| `NoHost` | Server-reflexive and relay, no host candidates |
| `None` | No candidates gathered at all |

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{IceTransportsType, RtcConfiguration};

let config = RtcConfiguration { ice_transport_type: IceTransportsType::Relay, ..Default::default() };
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(ice_transport_type="relay")
```

</details>

Note the asymmetry: the Rust binding takes the `IceTransportsType` enum, but
the Python binding takes the plain lowercase string (`"all"`, `"relay"`,
`"no_host"`, `"none"`) — there is no `IceTransportsType` class on the Python
side.

## Continual gathering policy

| Value | Meaning |
|-------|---------|
| `GatherOnce` (default) | Gathering reaches a `Complete` state once — the libwebrtc default, and required if your code waits on gathering completion |
| `GatherContinually` | Keeps gathering for the connection's life and never reports `Complete` — an explicit choice, not a drop-in replacement |

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{ContinualGatheringPolicy, RtcConfiguration};

let config = RtcConfiguration {
    continual_gathering_policy: ContinualGatheringPolicy::GatherContinually,
    ..Default::default()
};
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(continual_gathering_policy="continually")
```

</details>

Same asymmetry as above: a Rust enum, a Python string (`"once"` or
`"continually"`).

## Port range

Confines the UDP ports ICE may allocate — for punching a hole in a firewall
that only forwards a fixed range, or complying with a network policy.

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::RtcConfiguration;

let config = RtcConfiguration { min_port: Some(10_000), max_port: Some(10_100), ..Default::default() };
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(min_port=10_000, max_port=10_100)
```

</details>

Default: unset (`0`/`0` on the wire) — the OS assigns ephemeral ports.
`min_port` and `max_port` must both be set together, and `min_port <= max_port`;
otherwise the binding raises before ever reaching libwebrtc.

## Bundle policy

How m-sections (audio, video, data) are bundled onto ICE/DTLS transports.

| Value | Meaning |
|-------|---------|
| `Balanced` (default) | libwebrtc's own default |
| `MaxBundle` | All m-sections share one transport — **recommended for real-time streaming**: one DTLS+SRTP association instead of one per track, fewer ICE pairs that need to succeed, less per-packet overhead |
| `MaxCompat` | One transport per m-section — maximum compatibility with legacy stacks, at the cost of the above |

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{BundlePolicy, RtcConfiguration};

let config = RtcConfiguration { bundle_policy: BundlePolicy::MaxBundle, ..Default::default() };
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(bundle_policy=rw.BundlePolicy.MaxBundle)
```

</details>

## TCP candidates

Whether libwebrtc gathers TCP ICE candidates alongside UDP.

Disabled by default — TCP adds latency. Enable it only when UDP is actually
blocked (corporate firewalls, symmetric NATs without a TURN relay).

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::{RtcConfiguration, TcpCandidatePolicy};

let config = RtcConfiguration { tcp_candidate_policy: TcpCandidatePolicy::Enabled, ..Default::default() };
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(tcp_candidate_policy=rw.TcpCandidatePolicy.Enabled)
```

</details>

## ICE timeouts

Two independent knobs, both `None`/`0` (libwebrtc default) unless set:

- **`ice_connection_receiving_timeout_ms`** — how long ICE waits for a
  response before declaring a path failed. libwebrtc's default is
  conservative (~30 s in practice) — fine for a call that tolerates a long
  hang before reconnecting, much too slow for real-time streaming. `2000`–
  `4000` detects a dead path fast enough to trigger a reconnect before a
  viewer notices a freeze.
- **`ice_check_interval_strong_connectivity_ms`** — the interval between ICE
  connectivity checks on an already-healthy path (libwebrtc default ~500 ms).
  Lowering it (e.g. `250`) makes keepalives more frequent, trading a little
  bandwidth for faster detection of a path change (a client roaming from
  Wi-Fi to cellular, say).

<details>
<summary>🦀 Example using Rust</summary>

```rust
use reactor_webrtc::RtcConfiguration;

let config = RtcConfiguration {
    ice_connection_receiving_timeout_ms: Some(3000),
    ice_check_interval_strong_connectivity_ms: Some(250),
    ..Default::default()
};
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
import reactor_webrtc as rw

config = rw.RtcConfiguration(
    ice_connection_receiving_timeout_ms=3000,
    ice_check_interval_strong_connectivity_ms=250,
)
```

</details>

## Congestion-control bitrate limits

`set_bitrate` is a `PeerConnection` method, not an `RtcConfiguration` field —
call it any time after the connection is created, including after
negotiation, and call it again to change the limits mid-call. Both snippets
below assume `pc` is an already-created peer connection (see
[`architecture.md`](architecture.md) for how a `PeerConnectionFactory`
creates one). In Python it's one of the awaitable methods (see
[`architecture.md`](architecture.md#threading-model)) — the Rust API stays
synchronous.

<details>
<summary>🦀 Example using Rust</summary>

```rust
pc.set_bitrate(Some(200_000), Some(500_000), Some(2_000_000))?;
```

</details>

<details>
<summary>🐍 Example using Python</summary>

```python
await pc.set_bitrate(min_bps=200_000, start_bps=500_000, max_bps=2_000_000)
```

</details>

All three arguments are optional bits-per-second values; passing `None`
leaves that value at its libwebrtc default. Each argument does something
different:

- **`min_bps`** — a floor handed to the congestion controller (GCC). The
  sender will not drop below this even when the network estimate is very
  low. Setting a floor above what a constrained link can actually sustain
  trades graceful quality degradation for a fixed minimum send rate —
  choose it deliberately, not just because a nonzero value felt safer than
  `None`.
- **`start_bps`** — the initial encoder target. libwebrtc's own default is
  ~300 kbps, which produces a visible quality ramp-up on a fresh connection.
  Setting this close to your expected steady-state bitrate reaches good
  quality immediately instead of ramping into it.
- **`max_bps`** — a ceiling; GCC will not allocate above this regardless of
  how good the estimated path looks.

libwebrtc enforces `0 <= min_bps <= start_bps <= max_bps` on whichever of the
three you pass, and `set_bitrate` returns an error (`RuntimeError` in Python)
if that ordering doesn't hold — validate your inputs before calling it
rather than relying on the error to catch a misconfiguration at connection
time.
