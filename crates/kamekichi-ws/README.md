# kamekichi-ws

A lightweight, robust, low-level WebSocket (RFC 6455) client library for Rust.

- Bring your own stream — works with blocking, non-blocking, TCP and TLS streams alike. No async runtime needed.
- Zero-copy message reads in the common (non-fragmented) case.
- Conservative with allocations — buffers are front-loaded and reused throughout. No allocation happens on hot paths.
- Thoroughly tested with 100% line coverage: [![Line Coverage](https://github.com/golddranks/kamekichi-bot/actions/workflows/coverage.yml/badge.svg)](https://github.com/golddranks/kamekichi-bot/actions/workflows/coverage.yml)
- No `unsafe`.
- Minimal, carefully vetted dependencies: `base64`, `ring`, and optionally `rand_core`.

**Note: Read author's stance [about AI, code quality and contributing](#authors-stance-about-ai-and-code-quality) below.**

## Usage

```rust,ignore
use std::net::TcpStream;
use kamekichi_ws::{WebSocket, Message};
use rand_chacha::ChaCha8Rng;

// Bring your own stream (TcpStream, TLS, etc.) and RNG (see below).
let stream = TcpStream::connect("example.com:80")?;
let rng = ChaCha8Rng::from_os_rng();

let ws = WebSocket::new(rng);
let mut ws = ws.connect(stream, "example.com", "/ws")?;

ws.send_text(r#"{"op":1}"#)?;

loop {
    match ws.read_message()? {
        Some(Message::Text(t)) => println!("{t}"),
        Some(Message::Binary(b)) => println!("{} bytes", b.len()),
        Some(Message::Close(_, _)) => break,
        None => { /* WouldBlock / TimeOut / frame budget exhausted */ }
    }
}
```

### Random Number Generators

WebSockets need random numbers for tasks such as frame masking. User has to
bring their own RNG. Enable the `rand` feature to use any RNG from the `rand`
project:

```toml
kamekichi-ws = { version = "0.1", features = ["rand"] }
rand_chacha = "0.10.0"
```

```rust
use rand_chacha::ChaCha8Rng;
let ws = WebSocket::new(ChaCha8Rng::from_seed(Default::default()));
```

## Author's stance about AI and code quality

**TL;DR: This crate contains some AI generated code, all of which was generated
under a tight feedback loop of human supervision and directions, and 100%
carefully human-reviewed and hand-edited. But especially the test suite
is completely AI-made. The author strives for exceptional code quality, and
is steadfast of protecting that even in the AI age.**

I have used AI, namely Claude Code extensively as a tool developing this crate.
I used to have a distaste for generative AI, and I still generally do. However,
as I have been experimenting on how to create high-quality code with the new
tools and under the new circumstances of what year 2026 has brought us to, I
think I've managed to strike a reasonable balance between getting a boost in
productivity while not sacrificing code quality.

I much appreciate Andrew Kelley words ["Software Should be Perfect"](https://www.youtube.com/watch?v=Z4oYSByyRak) and try to live by them when developing
software – including this crate.
It seems like using AI tools is odds with this goal, as the code quality,
and especially higher-level decisions made by AI are often questionable.
However, the I think I've managed to get it work with this crate.

Here are some principles I'm following to keep the quality up.

- Don't let the AI take the wheel. The author should be opinionated about
  architectural, API etc. decisions and enforce them. To give a sampler, my opinions on code are reflected in this crate include the following:
  - Keep dependencies minimal enough that you can have a grasp of the whole
    dependency tree, and preferably review the actual code.
  - Be conscious about allocations, memory layout and have "mechanical sympathy".
    Always strive for the best performance that is reasonable given the scope
    and the API.
  - Don't add features for the sake of it. Always have a downstream needs to
    guide the scope of the crate.
- Employ all the usual tools to keep the code quality up. Tests,
  internal & external documentation, code reviews, code coverage, fuzzing etc.
- Consider two cases separately:
  1. when directing an AI to do targeted edits with
     a clear scope, you can consider it as just a writing tool.
  2. However, when you are asking it to produce larger amounts of code
     (prototyping, generating tests etc.), you should consider it like a 3rd
     party contributor – verify and review like you would get a PR from a
     stranger.

Besides these, I'm exploring on how to make use of AI to improve the code quality
even further than what was feasible back then when everything was done manually
as volunteer work.

- I'm experimenting with targeted code reviews and generating reports about
  allocations, integer overflows, panics, unsafe, error paths etc. with 100%
  coverage.
- In the future, I'm thinking of dabbling with static verifiers, theorem provers
  etc.

## Current state of the crate

The crate is feature-complete and thoroughly tested.

### Features considered but not implemented

All of these are plausible features, but I'm not sure if the need is there,
and they wouldn't pull their weigh without an actual downstream use / need.
Many are out of scope for 1.0, but not rejected outright. It might be that
I want to have a working prototype of each before 1.0, just to be sure that
I don't have to have breaking changes later, before committing to a stable
API.

- no_std support
- async support
- compression support
- sending fragmented messages
- server mode
- splitting reader and writer

### TODO for releasing 1.0

- [ ] Review against RFC 6455
- [ ] Review allocations
- [ ] Review integer over/underflows
- [ ] Review panics
- [ ] Review error paths
- [ ] Review performance concerns
- [ ] Review docs
- [ ] Review naming, code understandability etc.
- [ ] Implement fuzzing.

## License

MIT / Apache-2.0
