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

The service uses the following defaults (hardcoded constants):

- **Pipe name**: `\\.\pipe\VirimaRemoteAgent`
- **Max concurrent clients**: 100
- **Max concurrent jobs**: 50
- **Idle cleanup**: Jobs cleaned up after 30 minutes of inactivity
- **Quiet cleanup**: All completed jobs cleaned up if system is idle for 2 minutes
- **Cleanup interval**: Checks every 30 seconds

**Note**: These are currently hardcoded in `src/service_host.rs` and `src/pipe_server.rs`. Future versions may support configuration via environment variables or config file.

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

## Usage

### Running the Executable

#### Console Mode (Testing/Development)

Run directly from command line for testing:

```powershell
# Run in console mode (logs to stdout)
.\target\release\virima-remote-agent.exe

# Or with explicit flag
.\target\release\virima-remote-agent.exe --console
```

**Console Mode Features:**
- Logs output to console/stdout
- Press `Ctrl+C` to stop
- Useful for debugging and development
- No Windows Service installation required

#### Windows Service Mode

Install and run as a Windows Service:

```powershell
# 1. Install the service (requires Administrator)
sc.exe create VirimaRemoteAgent binPath="C:\path\to\virima-remote-agent.exe --service" start=auto

# 2. Start the service
sc.exe start VirimaRemoteAgent

# 3. Check status
sc.exe query VirimaRemoteAgent

# 4. View logs
Get-EventLog -LogName Application -Source VirimaRemoteAgent -Newest 50

# 5. Stop the service
sc.exe stop VirimaRemoteAgent

# 6. Uninstall the service
sc.exe delete VirimaRemoteAgent
```

**Service Mode Features:**
- Runs in background as Windows Service
- Auto-starts on boot (if configured)
- Logs to Windows Event Log
- Managed via Service Control Manager (SCM)

### Client Connection

The service listens on named pipe: `\\.\pipe\VirimaRemoteAgent`

**Connection Requirements:**
- Client must run as Administrator (or SYSTEM)
- Client must have network access to the server (named pipes use SMB)
- Protocol version must match (currently version 1)

### Protocol Flow

A client connects and executes commands as follows:

1. **Connect** to named pipe `\\.\pipe\VirimaRemoteAgent`
2. **Send HELLO frame** with protocol version:
   ```json
   {
     "version": 1,
     "client_id": "optional-client-id"
   }
   ```
3. **Receive HELLO_ACK** confirming protocol version
4. **Send RUN frame** with command:
   ```json
   {
     "command": "powershell.exe -Command Write-Host 'Hello World'",
     "working_directory": "C:\\Users\\Administrator",
     "environment": {"VAR1": "value1"},
     "timeout_ms": 30000
   }
   ```
5. **Receive RUN_ACK** with `job_id`
6. **Receive OUTPUT frames** for stdout/stderr (stream_id: 1=stdout, 2=stderr)
7. **Receive EXIT frame** with exit code when process completes:
   ```json
   {
     "exit_code": 0
   }
   ```

### Example: Simple Test Client (PowerShell)

```powershell
# Connect to named pipe and send a simple command
$pipe = New-Object System.IO.Pipes.NamedPipeClientStream(
    ".", 
    "VirimaRemoteAgent", 
    [System.IO.Pipes.PipeDirection]::InOut,
    [System.IO.Pipes.PipeOptions]::None,
    [System.Security.Principal.TokenImpersonationLevel]::Impersonation
)

$pipe.Connect(5000)

# Send HELLO frame (simplified - actual protocol is binary)
# ... implement frame encoding ...

$pipe.Close()
```

### Example: Java Client (Pseudo-code)

```java
// Connect to named pipe
NamedPipeClientStream pipe = new NamedPipeClientStream(
    ".", 
    "VirimaRemoteAgent", 
    PipeDirection.InOut
);
pipe.Connect();

// Send HELLO frame
HelloPayload hello = new HelloPayload(1, "java-client");
Frame helloFrame = Frame.hello(hello);
pipe.write(helloFrame.encode());

// Read HELLO_ACK
Frame ack = Frame.decode(pipe.read());

// Send RUN frame
RunPayload run = new RunPayload();
run.command = "cmd.exe /c echo Hello World";
run.timeout_ms = 30000L;
Frame runFrame = Frame.run(run);
pipe.write(runFrame.encode());

// Read RUN_ACK, OUTPUT frames, and EXIT frame
// ... handle streaming output ...
```

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

