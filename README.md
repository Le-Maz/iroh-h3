# Iroh HTTP/3 Toolkit

> A collection of crates enabling HTTP/3 communication over the [Iroh](https://github.com/n0-computer/iroh) peer-to-peer (P2P) transport built on QUIC.

This monorepo contains a set of Rust crates that integrate the [`iroh`](https://github.com/n0-computer/iroh) networking stack — a P2P library built on top of QUIC — with the [`h3`](https://github.com/hyperium/h3) HTTP/3 protocol implementation and compatible async frameworks such as [`axum`](https://github.com/tokio-rs/axum).

The implementation is adapted from the [`hyperium/h3`](https://github.com/hyperium/h3) project and reworked to support Iroh’s peer-to-peer QUIC transport model. All code is released under the **MIT License**.

---

## Repository Structure

| Crate                | Description                                                                                                                                  |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| **`iroh-h3`**        | Core transport adapter bridging `iroh::endpoint::Connection` with `h3::quic::Connection` to support HTTP/3 semantics over Iroh’s QUIC layer. |
| **`iroh-h3-client`** | Client utilities for establishing outbound HTTP/3 streams over Iroh’s P2P connections.                                                       |
| **`iroh-axum`**      | Integration layer allowing Axum applications to serve or consume HTTP/3 traffic over Iroh.                                                   |

Each crate lives within the same Cargo workspace and is designed to interoperate cleanly while remaining usable independently.

---

## Design Goals

- **P2P Integration:** Extend HTTP/3 semantics to work seamlessly over Iroh’s decentralized QUIC-based transport.
- **Interoperability:** Preserve the `h3` trait model (`Connection`, `BidiStream`, etc.) to stay compatible with existing HTTP/3 stacks.
- **Extensible Stack:** Serve as a foundation for higher-level protocols and frameworks like Axum to operate over peer-to-peer links.
- **Correct and Robust:** Carefully translate `iroh`’s error and stream models into the HTTP/3 abstraction layer.

---

## License

All code in this repository is licensed under the **MIT License**.  
It includes portions derived from the `hyperium/h3` project, which is also MIT-licensed.
