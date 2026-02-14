# moblin-assistant

A Rust implementation of the Moblin remote control assistant server. This server allows Moblin streamers to connect via WebSocket and provides remote control functionality.

## Features

- WebSocket server for Moblin remote control protocol
- Connection setup with challenge-response authentication (SHA256-based)
- Identification using password hashing with salt and challenge
- Ping/pong keep-alive mechanism
- Hardcoded chat message sending to connected streamers

## Building

```bash
cargo build --release
```

## Usage

Start the server with a password:

```bash
cargo run -- --password my-secure-password
```

Or specify a custom port:

```bash
cargo run -- --password my-secure-password --port 2345
```

For release builds:

```bash
./target/release/moblin-assistant --password my-secure-password
```

## Protocol

The server implements the Moblin remote control protocol:

1. **Hello**: Server sends authentication challenge and salt
2. **Identify**: Client sends hashed password for authentication
3. **Identified**: Server confirms authentication status
4. **Ping/Pong**: Keep-alive mechanism
5. **Chat Messages**: Server can send chat messages to the streamer

## Example

1. Start the server:
   ```bash
   cargo run -- --password test123
   ```

2. Connect your Moblin app to the server using the IP address and password

3. The server will send hardcoded chat messages after successful authentication

## Reference Implementation

This implementation is based on:
- https://github.com/eerimoq/moblin/tree/main/Moblin/RemoteControl
- https://github.com/eerimoq/moblin_assistant
