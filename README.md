# Virima Remote Agent Service

A production-grade Windows service for remote process execution, similar to PAExec/PsExec.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed design documentation.

## Features

- **Multiplexed Protocol**: Single named pipe with frame-based multiplexing for stdout/stderr/stdin
- **Process Management**: Launch processes with full control (timeouts, cancellation, job objects)
- **Output Streaming**: Real-time stdout/stderr streaming with backpressure handling
- **Security**: ACL-based access control (Administrators only), client impersonation, audit logging
- **Reliability**: Graceful shutdown, proper cleanup, no zombie processes

## Building

### Prerequisites

- Rust toolchain (latest stable)
- Windows SDK
- Visual Studio Build Tools (for Windows API bindings)

### Build Commands

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test
```

## Installation

### As Windows Service

1. Build the release binary:
   ```bash
   cargo build --release
   ```

2. Install as Windows Service (requires admin):
   ```powershell
   sc.exe create VirimaRemoteAgent binPath="C:\path\to\virima-remote-agent.exe" start=auto
   sc.exe start VirimaRemoteAgent
   ```

3. Check service status:
   ```powershell
   sc.exe query VirimaRemoteAgent
   ```

### Configuration

The service uses the following defaults (configurable via environment variables or config file):

- Pipe name: `\\.\pipe\VirimaRemoteAgent`
- Max concurrent clients: 64
- Max concurrent jobs: 32
- Graceful shutdown timeout: 30 seconds
- Default job timeout: 3600 seconds (1 hour)

## Protocol

The service uses a binary frame-based protocol over named pipes. See [ARCHITECTURE.md](ARCHITECTURE.md) for protocol details.

### Frame Format

```
[4 bytes: frame_length]
[1 byte: frame_type]
[1 byte: stream_id]
[2 bytes: flags]
[4 bytes: job_id]
[4 bytes: sequence_number]
[variable: payload]
```

### Frame Types

- `HELLO` (0x01): Client announces protocol version
- `HELLO_ACK` (0x02): Server acknowledges
- `RUN` (0x10): Execute command
- `RUN_ACK` (0x11): Job accepted
- `OUTPUT` (0x20): stdout/stderr data
- `STATUS` (0x30): Job state change
- `EXIT` (0x40): Process exited
- `CANCEL` (0x50): Cancel job
- `ERROR` (0xF0): Error frame

## Usage Example

The service listens on `\\.\pipe\VirimaRemoteAgent` and accepts connections from administrators.

A Java client would:
1. Connect to the named pipe
2. Send `HELLO` frame
3. Receive `HELLO_ACK`
4. Send `RUN` frame with command details
5. Receive `RUN_ACK` with job_id
6. Receive `OUTPUT` frames for stdout/stderr
7. Receive `EXIT` frame with exit code

## Security

- **Access Control**: Only SYSTEM and Administrators can connect (enforced via ACL)
- **Client Impersonation**: Service impersonates client to verify identity
- **Audit Logging**: All commands logged with user, command, and result
- **Input Validation**: Command length limits, path validation, environment sanitization

## Logging

Logs are written to:
- Windows Event Log: `Application` log, source `VirimaRemoteAgent`
- File log: `C:\ProgramData\Virima\RemoteAgent\service.log` (optional)

Log levels: ERROR, WARN, INFO, DEBUG, TRACE

## Troubleshooting

### Service won't start

- Check Windows Event Log for errors
- Verify service account has necessary permissions
- Ensure pipe name is not already in use

### Client connection fails

- Verify client is running as Administrator
- Check firewall settings (named pipes use SMB)
- Verify pipe name matches client configuration

### Process launch fails

- Check audit logs for access denied errors
- Verify working directory exists and is accessible
- Check command line length (max 32KB)

## Development

### Project Structure

```
src/
├── main.rs              # Service entry point
├── lib.rs               # Library exports
├── error.rs             # Error types
├── protocol.rs          # Frame protocol
├── job_manager.rs       # Job lifecycle management
├── process_launcher.rs # Process creation
├── io_pump.rs          # Output streaming
├── pipe_server.rs      # Named pipe server
├── service_host.rs     # Windows Service host
├── cancellation.rs      # Timeout/cancellation
└── logger.rs           # Logging/auditing
```

### Testing

```bash
# Unit tests
cargo test

# Integration tests (requires Windows)
cargo test --test integration
```

## License

[Your License Here]

## Contributing

[Your Contributing Guidelines Here]

