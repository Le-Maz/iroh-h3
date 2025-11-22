# Iroh HTTP/3 Client

`iroh-h3-client` is a Rust library for building and sending HTTP/3 requests using the QUIC-based HTTP/3 protocol over Iroh. It provides a high-level, ergonomic API for constructing requests, managing state, and receiving responses asynchronously.

## Features

- **HTTP/3 Requests:** Build requests with custom headers, extensions, and bodies.
- **Connection Reuse:** Connections are automatically reused across multiple requests to the same server, improving performance.
- **Flexible Body Types:** Send plain text, binary data, or JSON-serialized payloads.
- **Streaming Responses:** Read response bodies incrementally without buffering the entire content.
- **Full Body Consumption:** Convenient methods to read response bodies as `Bytes` or `String`.
- **JSON Integration:** (Optional) Serialize request bodies and deserialize response bodies using `serde`.
- **Server-Sent Events (SSE):** Native support for consuming real-time event streams.
- **Middleware & Interceptors:** Inject custom logic to modify requests and responses in the lifecycle.
- **Redirect Handling:** Automatic support for following HTTP redirect jumps.
- **Cookie Jar:** Stateful cookie management for handling sessions across requests.

## Optional Features

- **`json`**: Enables `RequestBuilder::json` and `Response::json` for working with JSON payloads.

## Error Handling

All operations return a crate-specific `Error` type that represents connection, stream, serialization, or protocol-level errors.

## License

This project is licensed under the **MIT License**.
