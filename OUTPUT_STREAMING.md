# Output Streaming Guide

## How Output Streaming Works

This document explains how clients receive stdout/stderr output from executed processes.

## Architecture Overview

```
Process → stdout/stderr pipes → IO Pump → OUTPUT Frames → Named Pipe → Client
```

### Components

1. **Process stdout/stderr**: Standard output streams from the executed process
2. **IO Pump**: Reads from process pipes and creates OUTPUT frames
3. **Frame Writer**: Sends frames to client over named pipe
4. **Client**: Receives and decodes OUTPUT frames

## Frame Format

### OUTPUT Frame Structure

```
[4 bytes: length_prefix]  // Total frame length (not including this field)
[1 byte: frame_type]      // 0x20 = OUTPUT
[1 byte: stream_id]       // 1 = stdout, 2 = stderr
[2 bytes: flags]          // 0x01 = EOS (End of Stream)
[4 bytes: job_id]         // Job identifier
[4 bytes: sequence]       // Sequence number (for ordering)
[variable: payload]       // Actual output data (max 32KB)
```

### Binary Example

For stdout data "Hello World\n":
```
Length: 0x0000001A (26 bytes)
Type: 0x20 (OUTPUT)
Stream: 0x01 (stdout)
Flags: 0x0000
Job ID: 0x00000001
Sequence: 0x00000000
Payload: "Hello World\n" (12 bytes)
```

## How It Works

### 1. Process Launch

When a client sends a `RUN` frame:

```json
{
  "command": "powershell.exe -Command Write-Host 'Hello'; Write-Error 'Error'",
  "timeout_ms": 30000
}
```

The service:
- Launches the process with redirected stdout/stderr
- Creates two separate IO pumps (one for stdout, one for stderr)
- Each pump runs in its own async task

### 2. Output Reading

Each IO pump continuously:
- Reads up to 32KB chunks from the process pipe
- Creates OUTPUT frames with the data
- Sends frames to the client via the named pipe

**Stdout Pump:**
```rust
// Reads from process.stdout_read handle
// Creates frames with stream_id = StreamId::Stdout (1)
// Sequence numbers: 0, 1, 2, ...
```

**Stderr Pump:**
```rust
// Reads from process.stderr_read handle  
// Creates frames with stream_id = StreamId::Stderr (2)
// Sequence numbers: 0, 1, 2, ...
```

### 3. Frame Transmission

Frames are sent over the same named pipe connection:
- All frames (stdout, stderr, control) share the same pipe
- Multiplexed by `stream_id` field
- Client can distinguish stdout vs stderr by `stream_id`

### 4. End of Stream

When a stream ends (process closes stdout/stderr):
- IO pump sends a final OUTPUT frame with `FLAG_EOS` (0x01)
- Payload is empty
- Client knows that stream is complete

## Client Implementation

### Receiving OUTPUT Frames

```java
// Pseudocode for Java client

while (connected) {
    // Read frame length (4 bytes)
    byte[] lenBytes = readExact(pipe, 4);
    int frameLength = ByteBuffer.wrap(lenBytes).order(ByteOrder.BIG_ENDIAN).getInt();
    
    // Read frame data
    byte[] frameData = readExact(pipe, frameLength);
    
    // Decode frame
    Frame frame = Frame.decode(frameData);
    
    if (frame.frameType == FrameType.OUTPUT) {
        // Check stream ID
        if (frame.streamId == StreamId.STDOUT) {
            // This is stdout data
            String output = new String(frame.payload, StandardCharsets.UTF_8);
            System.out.print(output); // or handle as needed
        } else if (frame.streamId == StreamId.STDERR) {
            // This is stderr data
            String error = new String(frame.payload, StandardCharsets.UTF_8);
            System.err.print(error); // or handle as needed
        }
        
        // Check for end of stream
        if (frame.hasFlag(FLAG_EOS)) {
            // This stream (stdout or stderr) is complete
            if (frame.streamId == StreamId.STDOUT) {
                stdoutComplete = true;
            } else {
                stderrComplete = true;
            }
        }
    } else if (frame.frameType == FrameType.EXIT) {
        // Process exited
        ExitPayload exit = parseExitPayload(frame.payload);
        int exitCode = exit.exitCode;
        break;
    }
}
```

### Complete Example Flow

```
1. Client sends RUN frame
   ↓
2. Service sends RUN_ACK with job_id=1
   ↓
3. Service spawns stdout pump (async task)
   ↓
4. Service spawns stderr pump (async task)
   ↓
5. Process writes "Hello\n" to stdout
   ↓
6. Stdout pump reads data, creates OUTPUT frame:
   - type: OUTPUT (0x20)
   - stream_id: STDOUT (1)
   - job_id: 1
   - sequence: 0
   - payload: "Hello\n"
   ↓
7. Frame sent to client over named pipe
   ↓
8. Client receives frame, decodes, prints "Hello\n"
   ↓
9. Process writes "Error\n" to stderr
   ↓
10. Stderr pump creates OUTPUT frame:
    - type: OUTPUT (0x20)
    - stream_id: STDERR (2)
    - job_id: 1
    - sequence: 0
    - payload: "Error\n"
   ↓
11. Client receives frame, decodes, prints to stderr
   ↓
12. Process exits with code 0
   ↓
13. Stdout pump sends EOS frame (FLAG_EOS set)
   ↓
14. Stderr pump sends EOS frame (FLAG_EOS set)
   ↓
15. Service sends EXIT frame with exit_code=0
   ↓
16. Client receives EXIT frame, knows process completed
```

## Flow Control

### Window-Based Flow Control

The service implements flow control to prevent overwhelming slow clients:

- **Initial Window**: 64KB per stream
- **Window Update**: Client sends `WINDOW_UPDATE` frame to increase window
- **Backpressure**: If window exhausted, service waits or drops frames (configurable)

### Window Update Frame

Client can send:
```json
{
  "stream_id": 1,  // stdout
  "bytes_consumed": 32768  // Client processed 32KB
}
```

This increases the send window, allowing more data to be sent.

## Important Notes

### 1. **Concurrent Streams**
- Stdout and stderr are sent **concurrently** over the same pipe
- Frames may arrive **out of order** between streams
- Use `sequence` number and `stream_id` to reconstruct order per stream

### 2. **Frame Ordering**
- Within a stream (stdout or stderr), frames are ordered by `sequence`
- Between streams, frames may interleave
- Example: stdout seq=0, stderr seq=0, stdout seq=1, stderr seq=1

### 3. **Payload Size**
- Maximum payload: 32KB per frame
- Large output is split into multiple frames
- Each frame has its own sequence number

### 4. **Encoding**
- Payload is raw bytes (not necessarily UTF-8)
- Client should handle binary data appropriately
- For text output, decode as UTF-8 with error handling

### 5. **End Detection**
- Process completion ≠ stream end
- Streams may close before process exits
- Always check `FLAG_EOS` to know when a stream is done
- Wait for `EXIT` frame to know process completion

## Example: Complete Client Loop

```java
boolean stdoutComplete = false;
boolean stderrComplete = false;
boolean processExited = false;

while (!processExited || !stdoutComplete || !stderrComplete) {
    Frame frame = readFrame(pipe);
    
    switch (frame.frameType) {
        case OUTPUT:
            if (frame.streamId == StreamId.STDOUT) {
                handleStdout(frame.payload);
                if (frame.hasFlag(FLAG_EOS)) {
                    stdoutComplete = true;
                }
            } else if (frame.streamId == StreamId.STDERR) {
                handleStderr(frame.payload);
                if (frame.hasFlag(FLAG_EOS)) {
                    stderrComplete = true;
                }
            }
            break;
            
        case EXIT:
            ExitPayload exit = parseExit(frame.payload);
            processExited = true;
            exitCode = exit.exitCode;
            break;
            
        case ERROR:
            // Handle error
            break;
    }
}
```

## Troubleshooting

### No Output Received
- Check if process actually produces output
- Verify frame decoding is correct
- Check `stream_id` filtering logic

### Output Out of Order
- Use `sequence` number to order frames within each stream
- Don't assume frames arrive in order between streams

### Missing End of Stream
- Always check `FLAG_EOS` flag
- Don't rely on process exit alone

### Large Output Handling
- Implement buffering for multi-frame output
- Reassemble chunks using sequence numbers
- Handle window updates for flow control

