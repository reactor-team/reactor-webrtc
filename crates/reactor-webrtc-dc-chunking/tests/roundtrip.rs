//! Wire-level checks across the whole crate: golden vectors, then random
//! round trips through a SendQueue and a Reassembler.

use reactor_webrtc_dc_chunking::{Delivery, Reassembler, SendConfig, SendQueue};

fn hex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn config(chunk_size: usize) -> SendConfig {
    SendConfig {
        chunk_size,
        ..SendConfig::default()
    }
}

fn frames_of(q: &mut SendQueue) -> Vec<Vec<u8>> {
    std::iter::from_fn(|| q.next_frame(0)).collect()
}

#[test]
fn golden_vectors() {
    let cases = include_str!("vectors.txt")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let mut n = 0;
    for line in cases {
        let f: Vec<&str> = line.split('\t').collect();
        let (chunk, binary, message) = (f[0].parse().unwrap(), f[1] == "binary", hex(f[2]));
        let expected: Vec<Vec<u8>> = f[3].split(',').map(hex).collect();

        let mut q = SendQueue::new(config(chunk));
        q.push(message.clone(), binary).unwrap();
        assert_eq!(frames_of(&mut q), expected, "encode: {line}");

        let mut r = Reassembler::new(u64::MAX);
        let mut out = None;
        for frame in &expected {
            if let Delivery::Message { data, binary } = r.push(frame).unwrap() {
                out = Some((data, binary));
            }
        }
        assert_eq!(out, Some((message, binary)), "decode: {line}");
        n += 1;
    }
    assert!(n >= 8);
}

/// xorshift64*: deterministic, so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_round_trips() {
    let mut rng = Rng(0x5eed_cafe_f00d_1234);
    for _ in 0..300 {
        let chunk = 2 + rng.below(9000) as usize;
        let mut q = SendQueue::new(config(chunk));
        let mut r = Reassembler::new(u64::MAX);
        let mut sent = Vec::new();
        for _ in 0..1 + rng.below(4) {
            // Sizes around chunk boundaries as well as arbitrary ones.
            let size = match rng.below(4) {
                0 => rng.below(4) as usize,
                1 => (chunk - 1) * (1 + rng.below(3) as usize),
                2 => (chunk - 1) * (1 + rng.below(3) as usize) + 1,
                _ => rng.below(200_000) as usize,
            };
            let msg: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
            let binary = rng.below(2) == 0;
            q.push(msg.clone(), binary).unwrap();
            sent.push((msg, binary));
        }
        let mut got = Vec::new();
        for frame in frames_of(&mut q) {
            assert!(
                frame.len() <= chunk,
                "frame of {} bytes with chunk_size {chunk}",
                frame.len()
            );
            if let Delivery::Message { data, binary } = r.push(&frame).unwrap() {
                got.push((data, binary));
            }
        }
        assert_eq!(got, sent, "chunk_size {chunk}");
        assert_eq!(q.queued(), 0);
        assert!(!r.mid_message());
    }
}

#[test]
fn utf8_split_across_frames_survives() {
    // 3-byte characters with 2-byte payload frames: every character is split.
    let text = "€uro ünïcödé ✓".repeat(50);
    let mut q = SendQueue::new(config(3));
    q.push(text.as_bytes().to_vec(), false).unwrap();
    let mut r = Reassembler::new(u64::MAX);
    let mut out = None;
    for frame in frames_of(&mut q) {
        if let Delivery::Message { data, binary } = r.push(&frame).unwrap() {
            assert!(!binary);
            out = Some(String::from_utf8(data).unwrap());
        }
    }
    assert_eq!(out.as_deref(), Some(text.as_str()));
}

#[test]
fn large_message_respects_high_water() {
    // 20 MiB through a 64 KiB-frame queue, with a fake native buffer that
    // drains 1 MiB between pumps: the buffer never passes high-water + 1 frame.
    let cfg = SendConfig::default();
    let mut q = SendQueue::new(cfg);
    q.push(vec![7u8; 20 << 20], true).unwrap();
    let mut native: u64 = 0;
    let mut peak = 0;
    let mut r = Reassembler::new(cfg.max_message_size);
    let mut done = false;
    while !done {
        while let Some(frame) = q.next_frame(native) {
            native += frame.len() as u64;
            peak = peak.max(native);
            if let Delivery::Message { data, .. } = r.push(&frame).unwrap() {
                assert_eq!(data.len(), 20 << 20);
                done = true;
            }
        }
        native = native.saturating_sub(1 << 20);
    }
    assert!(
        peak <= cfg.high_water + cfg.chunk_size as u64,
        "peak {peak}"
    );
}
