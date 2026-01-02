# Remote Agent Service Architecture
## Production-Grade Design for PAExec/PsExec-Style Tool

---

## 1. Recommended IPC Design: Option B (Multiplexed Framed Protocol)

### Decision: Single Multiplexed Named Pipe with Framed Protocol

**Rationale:**

**Advantages of Option B (Multiplexed):**
1. **Connection Lifecycle Simplicity**: One pipe per client session eliminates race conditions between control and data pipes. Client disconnect is atomic—all streams terminate together.
2. **Flow Control**: Can implement proper backpressure via protocol-level flow control messages (e.g., `WINDOW_UPDATE`). With separate pipes, stdout/stderr blocking independently creates complex deadlock scenarios.
3. **Atomic Operations**: Framed protocol allows atomic multi-stream operations (e.g., "start job with all streams" in one transaction).
4. **Firewall/NAT Friendly**: Single port/pipe endpoint simplifies network configuration.
5. **Handle Management**: Fewer handles per client reduces resource exhaustion risk.
6. **Debugging**: Single pipe with structured frames is easier to log and trace than coordinating multiple pipes.
7. **Protocol Evolution**: Versioning and feature negotiation are simpler with a single control plane.

**Disadvantages Mitigated:**
- **Complexity**: Framing overhead is minimal compared to the benefits. Use efficient binary framing (length-prefixed).
- **Stream Separation**: Frames carry stream IDs (stdout=1, stderr=2, stdin=3, control=0), maintaining logical separation.

**Why Not Option A:**
- Separate pipes create race conditions: client may open control pipe but fail to open stdout/stderr before timeout.
- No unified flow control—stdout blocking doesn't prevent stderr writes, leading to handle exhaustion.
- Complex cleanup: must coordinate closure of 4+ handles atomically.
- Harder to implement cancellation: control pipe close doesn't guarantee child process sees it immediately.

### Protocol Design

**Named Pipe Name**: `\\.\pipe\VirimaRemoteAgent` (or configurable)

**Framing Format**:
```
[4 bytes: frame_length (little-endian, excludes length field)]
[1 byte: frame_type]
[1 byte: stream_id]
[2 bytes: flags]
[4 bytes: job_id (if applicable)]
[4 bytes: sequence_number (for ordering)]
[variable: payload]
```

**Frame Types**:
- `HELLO` (0x01): Client announces protocol version, capabilities
- `HELLO_ACK` (0x02): Server acknowledges, sends server version
- `RUN` (0x10): Execute command (payload: JSON/msgpack with cmdline, cwd, env, timeout, etc.)
- `RUN_ACK` (0x11): Job accepted, returns job_id
- `OUTPUT` (0x20): stdout/stderr data chunk (stream_id distinguishes)
- `STATUS` (0x30): Job state change (Starting/Running/Exiting)
- `EXIT` (0x40): Process exited, includes exit_code
- `CANCEL` (0x50): Client requests cancellation
- `CANCEL_ACK` (0x51): Cancellation acknowledged
- `WINDOW_UPDATE` (0x60): Flow control (client advertises available buffer)
- `ERROR` (0xF0): Error frame (includes error code and message)
- `PING` (0xFE): Keepalive
- `PONG` (0xFF): Keepalive response

**Stream IDs**:
- `0`: Control channel
- `1`: stdout
- `2`: stderr
- `3`: stdin (client → server)

**Job ID**: 32-bit incrementing counter (wraps, but collision unlikely in practice).

---

## 2. Component Breakdown

### 2.1 Service Host (`service_host.rs`)

**Responsibilities**:
- Windows Service entry point (`ServiceMain`, `ServiceCtrlHandler`)
- Service lifecycle management (start/stop/pause/continue)
- Initializes and coordinates all subsystems
- Handles service stop requests gracefully (drains active jobs with timeout)

**Key Functions**:
- `service_main()`: Entry point, registers service control handler
- `on_service_stop()`: Initiates graceful shutdown
- `wait_for_shutdown()`: Waits for all jobs to complete or timeout

**Dependencies**: Pipe server, Job manager, Logger

---

### 2.2 Pipe Server (`pipe_server.rs`)

**Responsibilities**:
- Listens on named pipe endpoint(s)
- Accepts client connections (non-blocking or thread pool)
- Spawns per-client handler threads/tasks
- Manages pipe security (ACLs, impersonation)
- Handles connection lifecycle (connect/disconnect)

**Key Functions**:
- `start_listening()`: Creates pipe instance, sets security descriptor
- `accept_connection()`: Accepts client, spawns handler
- `create_pipe_instance()`: Creates named pipe with appropriate security

**Security**:
- Default security descriptor: `SYSTEM` + `BUILTIN\Administrators` full access
- Impersonates client on connection to verify identity
- Logs client identity (SID, username) for auditing

**Concurrency**:
- One handler task per connected client
- Maximum concurrent clients configurable (default: 64)
- Rejects new connections when limit reached

---

### 2.3 Protocol Handler (`protocol_handler.rs`)

**Responsibilities**:
- Parses and validates incoming frames
- Routes frames to appropriate handlers based on type
- Constructs outgoing frames
- Manages per-client protocol state (version, capabilities, flow control windows)
- Handles protocol errors and malformed frames

**Key Functions**:
- `read_frame()`: Reads complete frame from pipe (handles partial reads)
- `write_frame()`: Writes frame atomically (with retry on partial writes)
- `handle_frame()`: Routes frame to appropriate handler
- `send_error()`: Sends ERROR frame with details

**State**:
- Protocol version negotiated
- Client capabilities (max frame size, supported features)
- Flow control windows per stream (stdout/stderr)
- Sequence numbers for ordering

---

### 2.4 Job Manager (`job_manager.rs`)

**Responsibilities**:
- Maintains registry of active jobs (job_id → JobHandle)
- Allocates unique job IDs
- Tracks job state transitions
- Coordinates job lifecycle (creation → execution → cleanup)
- Handles job cancellation and timeout
- Ensures cleanup on client disconnect

**Key Functions**:
- `create_job()`: Allocates job_id, creates JobHandle
- `get_job()`: Retrieves job by ID (with locking)
- `cancel_job()`: Initiates cancellation
- `cleanup_job()`: Releases resources, removes from registry
- `list_active_jobs()`: For diagnostics

**Data Structures**:
- `JobHandle`: Arc<Mutex<JobState>> containing:
  - job_id: u32
  - state: JobState enum
  - process_handle: Option<HANDLE>
  - job_object: Option<HANDLE> (Windows Job Object for tree termination)
  - stdout_pump: Option<JoinHandle>
  - stderr_pump: Option<JoinHandle>
  - stdin_writer: Option<Sender>
  - cancellation_token: CancellationToken
  - start_time: SystemTime
  - timeout: Option<Duration>

**Concurrency**:
- Uses `DashMap` or `RwLock<HashMap>` for O(1) job lookup
- Per-job mutex for state transitions (fine-grained locking)

---

### 2.5 Process Launcher (`process_launcher.rs`)

**Responsibilities**:
- Constructs `STARTUPINFO` and `PROCESS_INFORMATION` structures
- Handles command-line parsing and quoting (Windows rules)
- Sets up process environment (inheritance vs explicit env vars)
- Configures process security (token duplication, elevation)
- Creates Windows Job Object for process tree management
- Launches process via `CreateProcessAsUser` or `CreateProcess`

**Key Functions**:
- `launch_process()`: Main entry point
- `build_startup_info()`: Configures stdio handles
- `create_job_object()`: Creates job object for tree termination
- `duplicate_token_for_process()`: Handles "run as" semantics

**Security Contexts**:
- **LocalSystem**: Service runs as SYSTEM, child inherits (default)
- **Elevated**: Duplicates service token (already elevated if service runs as SYSTEM)
- **Impersonated**: Uses client's impersonated token (if authorized)

**Process Configuration**:
- Working directory: Validated, must exist
- Environment: Merges system env + provided vars (sanitized)
- Command line: Validated length (max 32KB), proper quoting
- Stdio handles: Anonymous pipes (non-inheritable handles, duplicated for child)

---

### 2.6 IO Pump (`io_pump.rs`)

**Responsibilities**:
- Reads from process stdout/stderr pipes concurrently
- Writes OUTPUT frames to client pipe with backpressure handling
- Manages per-stream buffers and flow control
- Handles end-of-stream detection (pipe close)
- Ensures ordering within each stream (sequence numbers)

**Key Functions**:
- `start_stdout_pump()`: Spawns thread reading stdout → OUTPUT frames
- `start_stderr_pump()`: Spawns thread reading stderr → OUTPUT frames
- `pump_stream()`: Main loop: read chunk → check flow control → send frame
- `handle_backpressure()`: Blocks or drops based on policy

**Concurrency Model**:
- One thread per stream (stdout, stderr) per job
- Uses `OVERLAPPED` I/O or async I/O for efficiency
- Non-blocking pipe writes with retry

**Backpressure Strategy** (detailed in section 4):
- Maintains per-stream send window (bytes in-flight)
- Blocks pump thread when window exhausted (default)
- Configurable: drop mode for high-throughput scenarios
- Sends `WINDOW_UPDATE` when client acknowledges (or uses implicit window)

---

### 2.7 Cancellation/Timeout Manager (`cancellation.rs`)

**Responsibilities**:
- Tracks per-job timeouts (wall-clock and CPU time)
- Monitors cancellation requests
- Terminates process trees cleanly (via Job Object)
- Handles graceful vs forceful termination
- Ensures no zombie processes

**Key Functions**:
- `start_timeout_monitor()`: Spawns monitor for job timeout
- `cancel_job()`: Initiates cancellation sequence
- `terminate_process_tree()`: Uses Job Object to kill entire tree
- `wait_for_exit()`: Waits for process exit with timeout

**Termination Strategy**:
1. Send `SIGTERM` equivalent (none on Windows) → skip
2. Post `WM_CLOSE` to main window (if GUI process)
3. Call `TerminateJobObject()` with exit code 1
4. Wait up to 5 seconds for clean exit
5. Force kill via `TerminateProcess()` if still running
6. Close all handles, remove from Job Object

---

### 2.8 Logger/Auditor (`logger.rs`)

**Responsibilities**:
- Structured logging to Windows Event Log
- File-based logging (optional, for diagnostics)
- Audit trail: who ran what command, when, exit code
- Correlation IDs (job_id) for traceability
- Log levels: ERROR, WARN, INFO, DEBUG, TRACE

**Key Functions**:
- `log_event()`: Writes structured log entry
- `audit_command()`: Logs command execution (who/what/when/result)
- `log_error()`: Logs errors with context (job_id, error code, message)

**Log Format**:
```
[timestamp] [level] [job_id] [component] message {key=value, ...}
```

**Audit Events**:
- `COMMAND_STARTED`: job_id, client_user, command_line, working_dir
- `COMMAND_COMPLETED`: job_id, exit_code, duration_ms
- `COMMAND_FAILED`: job_id, error_code, error_message
- `COMMAND_CANCELED`: job_id, reason (timeout/client_disconnect)
- `ACCESS_DENIED`: client_user, attempted_command

---

## 3. Job Lifecycle + State Machine

### 3.1 State Definitions

```rust
enum JobState {
    Created,      // Job allocated, not yet started
    Starting,     // Process launch in progress
    Running,      // Process running, IO pumps active
    Exiting,      // Process exited, waiting for final status
    Completed,    // Successfully completed (exit_code available)
    Failed,       // Failed to start or crashed (error_code available)
    Canceled,     // Canceled by client or timeout
    TimedOut,     // Exceeded timeout limit
}
```

### 3.2 State Transitions

```
Created → Starting:
  - Client sends RUN frame
  - Job manager validates request
  - Process launcher begins CreateProcess

Starting → Running:
  - CreateProcess succeeds
  - IO pumps started
  - STATUS frame sent (Running)

Starting → Failed:
  - CreateProcess fails
  - ERROR frame sent
  - Cleanup initiated

Running → Exiting:
  - Process exits (detected via WaitForSingleObject)
  - IO pumps finish draining
  - EXIT frame sent with exit_code

Running → Canceled:
  - Client sends CANCEL or disconnects
  - Process tree terminated
  - CANCEL_ACK sent (if client connected)

Running → TimedOut:
  - Timeout monitor detects expiration
  - Process tree terminated
  - EXIT frame sent with exit_code=1

Exiting → Completed:
  - All IO drained
  - Final status sent
  - Cleanup complete

Exiting → Failed:
  - IO pump error during drain
  - ERROR frame sent
  - Cleanup complete

Canceled → (terminal)
TimedOut → (terminal)
Completed → (terminal)
Failed → (terminal)
```

### 3.3 State Machine Guards

- **Created → Starting**: Valid RUN frame, job_id available, resources available
- **Starting → Running**: CreateProcess success, handles valid
- **Running → Exiting**: Process exit detected OR cancellation requested
- **Exiting → Completed**: All streams drained, no errors

### 3.4 Concurrent State Access

- State transitions protected by `Mutex<JobState>` within `JobHandle`
- Atomic compare-and-swap for state updates
- State queries use `RwLock` for read-heavy workloads

---

## 4. Output Capture/Streaming Strategy + Backpressure

### 4.1 Capture Mechanism

**Setup**:
1. Create anonymous pipes for stdout/stderr (non-inheritable)
2. Duplicate handles with `bInheritHandle=TRUE` for child process
3. Close original handles in parent (child owns them)
4. Use duplicated handles for reading in IO pumps

**Why Anonymous Pipes**:
- Lower overhead than named pipes for parent-child communication
- Automatic cleanup on process exit
- Built-in backpressure (blocking writes when buffer full)

### 4.2 Streaming Protocol

**Frame Format for OUTPUT**:
```
frame_length: 4 bytes
frame_type: OUTPUT (0x20)
stream_id: 1 (stdout) or 2 (stderr)
flags: EOS flag (0x01 = end of stream)
job_id: 4 bytes
sequence_number: 4 bytes (monotonically increasing per stream)
payload: variable (chunk of stdout/stderr data)
```

**Sequence Numbers**:
- Per-stream counter (stdout and stderr independent)
- Starts at 0, increments per OUTPUT frame
- Client can detect gaps/duplicates (for reliability, if needed)

### 4.3 Backpressure Handling

**Problem**: Client reads slowly → pipe buffer fills → `WriteFile` blocks → IO pump thread blocked → child process blocks on write → deadlock risk.

**Solution: Sliding Window Flow Control**

**Mechanism**:
1. **Send Window**: Tracks bytes sent but not acknowledged (per stream)
   - Initial window: 64KB per stream (configurable)
   - Decremented on send, incremented on `WINDOW_UPDATE`

2. **Client Acknowledgment**:
   - Option A (Implicit): Client reads → pipe buffer drains → `WriteFile` unblocks → window implicitly updated
   - Option B (Explicit): Client sends `WINDOW_UPDATE` frame with bytes_consumed

3. **Pump Behavior**:
   - Before sending OUTPUT frame, check: `bytes_in_flight < send_window`
   - If window exhausted: **block pump thread** (default) or **drop frame** (configurable)
   - On unblock/ack: resume sending

**Implementation**:
```rust
struct StreamPump {
    send_window: AtomicU64,      // Available window (bytes)
    bytes_in_flight: AtomicU64,  // Bytes sent, not acked
    window_lock: Mutex<()>,      // For blocking when window=0
}

fn send_output_frame(pump: &StreamPump, data: &[u8]) -> Result<()> {
    let data_len = data.len() as u64;
    
    // Check window (with retry loop for concurrent updates)
    loop {
        let in_flight = pump.bytes_in_flight.load(Ordering::Acquire);
        let window = pump.send_window.load(Ordering::Acquire);
        
        if in_flight + data_len <= window {
            // Window available, send
            pump.bytes_in_flight.fetch_add(data_len, Ordering::Release);
            write_frame(data)?;
            break;
        } else {
            // Window exhausted
            if DROP_POLICY {
                // Drop frame, log warning
                log::warn!("Dropping output frame, window exhausted");
                return Ok(()); // Swallow error
            } else {
                // Block until window available
                let _guard = pump.window_lock.lock().unwrap();
                // Re-check window (may have been updated)
                // If still exhausted, wait on condition variable
                pump.window_condvar.wait(_guard)?;
            }
        }
    }
    Ok(())
}
```

**Window Update Handling**:
- On `WINDOW_UPDATE` frame: `send_window += bytes_consumed`
- Wake blocked pump threads via condition variable

**Default Policy**: **Block** (preserves all output, simpler semantics)

**Drop Policy** (optional): For high-throughput scenarios where some loss is acceptable (e.g., log tailing).

### 4.4 End-of-Stream Semantics

**Detection**:
- `ReadFile` returns `ERROR_BROKEN_PIPE` → stream closed
- Set `EOS` flag in final OUTPUT frame
- Send `STATUS` frame: `Exiting` → `Completed`

**Ordering Guarantees**:
- Within-stream ordering: Sequence numbers ensure client can reconstruct order
- Cross-stream ordering: No guarantee (stdout frame #100 may arrive before/after stderr frame #50)
- Control frames (STATUS/EXIT): Ordered relative to last OUTPUT frame per stream (sent after drain)

### 4.5 Buffer Sizes

**Recommended Defaults**:
- Pipe buffer (OS-level): 64KB (Windows default, can be increased via `CreatePipe` with larger size)
- Per-stream send window: 64KB (matches pipe buffer)
- Frame payload max: 32KB (prevents fragmentation, allows efficient batching)
- Per-job stdout/stderr buffer: None (stream directly, no buffering in service)

**Rationale**:
- Small buffers → low latency, but more syscalls
- Large buffers → higher throughput, but higher memory usage
- 64KB balances both (typical for streaming scenarios)

---

## 5. Security Model for Named Pipes

### 5.1 Access Control List (ACL)

**Default Security Descriptor**:
```
Owner: SYSTEM
Group: SYSTEM
DACL:
  - SYSTEM: Full Control (FILE_ALL_ACCESS)
  - BUILTIN\Administrators: Full Control (FILE_ALL_ACCESS)
  - Everyone: Deny All (explicit deny for defense-in-depth)
```

**Implementation**:
```rust
fn create_pipe_security_descriptor() -> Result<SECURITY_DESCRIPTOR> {
    let mut sd = SECURITY_DESCRIPTOR::default();
    InitializeSecurityDescriptor(&mut sd, SECURITY_DESCRIPTOR_REVISION)?;
    
    // Build DACL
    let mut acl = ACL::new()?;
    
    // SYSTEM: Full access
    let system_sid = get_system_sid()?;
    acl.add_access_allowed_ace(
        &system_sid,
        FILE_ALL_ACCESS,
        NO_INHERITANCE,
    )?;
    
    // Administrators: Full access
    let admin_sid = get_administrators_sid()?;
    acl.add_access_allowed_ace(
        &admin_sid,
        FILE_ALL_ACCESS,
        NO_INHERITANCE,
    )?;
    
    // Everyone: Deny (explicit)
    let everyone_sid = get_everyone_sid()?;
    acl.add_access_denied_ace(
        &everyone_sid,
        FILE_ALL_ACCESS,
        NO_INHERITANCE,
    )?;
    
    SetSecurityDescriptorDacl(&mut sd, true, Some(&acl), false)?;
    Ok(sd)
}
```

### 5.2 Client Identity Verification

**On Connection**:
1. Accept pipe connection
2. Call `ImpersonateNamedPipeClient()` to assume client's security context
3. Call `GetTokenInformation(TokenUser)` to get client SID
4. Verify SID is in Administrators group (or custom allowlist)
5. Revert impersonation (or keep for audit logging)
6. Log connection: `[INFO] Client connected: {username}, SID: {sid}`

**Authorization Check**:
```rust
fn authorize_client(token: HANDLE) -> Result<bool> {
    let user_sid = get_token_user_sid(token)?;
    let admin_sid = get_administrators_sid()?;
    
    // Check if user is admin (or in custom allowlist)
    if is_sid_in_group(&user_sid, &admin_sid)? {
        return Ok(true);
    }
    
    // Check custom allowlist (if configured)
    if is_sid_in_allowlist(&user_sid)? {
        return Ok(true);
    }
    
    Ok(false)
}
```

### 5.3 Input Validation

**Command Line**:
- Max length: 32KB (configurable, default 32KB)
- Validate against allowlist/denylist patterns (optional)
- Sanitize: Remove control characters (except tabs/newlines if needed)
- Log full command for audit

**Working Directory**:
- Must be absolute path
- Must exist and be accessible
- Validate against restricted paths (e.g., deny `C:\Windows\System32`)

**Environment Variables**:
- Max total env size: 64KB
- Sanitize variable names (alphanumeric + underscore)
- Block dangerous vars: `PATH`, `PATHEXT` (or allowlist-only mode)

**Frame Validation**:
- Max frame size: 1MB (prevents DoS)
- Validate frame_type enum (reject unknown types)
- Validate job_id (must exist in registry)

### 5.4 Audit Logging

**Events Logged**:
1. **Connection**: `[AUDIT] Client connected: {user}, SID: {sid}, IP: {ip}`
2. **Command Execution**: `[AUDIT] Job {job_id}: User {user} executed: {command_line}, CWD: {cwd}`
3. **Completion**: `[AUDIT] Job {job_id}: Exit code {code}, Duration {ms}ms`
4. **Failure**: `[AUDIT] Job {job_id}: Failed - {error}`
5. **Access Denied**: `[AUDIT] Access denied: User {user} attempted {command}`

**Log Destination**:
- Windows Event Log: `Application` log, source `VirimaRemoteAgent`
- Optional file: `C:\ProgramData\Virima\RemoteAgent\audit.log` (append-only)

**Retention**:
- Event Log: 30 days (configurable)
- File Log: Rotate daily, keep 90 days

---

## 6. Failure Modes and Recovery

### 6.1 Client Disconnect Mid-Job

**Policy**: **Cancel job** (default) or **Detach and continue** (configurable)

**Default Behavior (Cancel)**:
1. Detect pipe disconnect: `ReadFile`/`WriteFile` returns `ERROR_PIPE_BROKEN`
2. Mark job as `Canceled`
3. Terminate process tree via Job Object
4. Cleanup resources (handles, threads)
5. Log: `[INFO] Job {job_id} canceled due to client disconnect`

**Alternative (Detach)**:
- Continue execution, buffer output (up to limit)
- On reconnect (same job_id?): Stream buffered output
- **Not recommended**: Complex, resource leak risk, unclear semantics

**Recommendation**: Always cancel on disconnect (simpler, safer).

### 6.2 Process Launch Failures

**Common Failures**:
- `ERROR_FILE_NOT_FOUND`: Executable not found
- `ERROR_ACCESS_DENIED`: Insufficient permissions
- `ERROR_INVALID_PARAMETER`: Malformed command line
- `ERROR_NO_SYSTEM_RESOURCES`: Handle exhaustion

**Recovery**:
1. Log error with context (job_id, command, error code)
2. Send `ERROR` frame to client with error code and message
3. Transition job to `Failed`
4. Cleanup any partial resources (handles, pipes)
5. Return job_id to pool (if applicable)

**Prevention**:
- Validate command line before launch
- Check executable existence (optional, may be expensive)
- Pre-allocate handle pool to detect exhaustion early

### 6.3 Pipe I/O Failures

**Failures**:
- `ERROR_PIPE_BROKEN`: Client disconnected
- `ERROR_NO_DATA`: Pipe closed gracefully
- `ERROR_IO_INCOMPLETE`: Partial read/write (retry)

**Recovery**:
- Broken pipe → cancel job (as above)
- Partial I/O → retry with remaining data
- Log I/O errors for diagnostics

### 6.4 Handle Exhaustion

**Symptoms**: `CreateProcess` fails with `ERROR_NO_SYSTEM_RESOURCES`

**Prevention**:
- Limit concurrent jobs (default: 32)
- Close handles promptly (RAII in Rust)
- Monitor handle count via `GetProcessHandleCount()`

**Recovery**:
- Reject new jobs when limit reached
- Send `ERROR` frame: "Service overloaded, try again later"
- Log warning: `[WARN] Handle exhaustion, rejecting new jobs`

### 6.5 Service Stop During Active Jobs

**Graceful Shutdown Sequence**:
1. Service receives `SERVICE_CONTROL_STOP`
2. Stop accepting new connections (close pipe listener)
3. Send cancellation to all active jobs
4. Wait up to 30 seconds for jobs to complete
5. Force terminate remaining jobs
6. Cleanup all resources
7. Exit service

**Implementation**:
```rust
fn on_service_stop() {
    log::info!("Service stop requested");
    
    // Stop accepting new connections
    pipe_server.stop_listening()?;
    
    // Cancel all jobs
    let active_jobs = job_manager.list_active_jobs();
    for job_id in active_jobs {
        job_manager.cancel_job(job_id, "Service stopping")?;
    }
    
    // Wait for completion (with timeout)
    let deadline = Instant::now() + Duration::from_secs(30);
    while job_manager.has_active_jobs() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    
    // Force cleanup
    job_manager.force_cleanup_all()?;
    
    log::info!("Service stopped");
}
```

### 6.6 Remote Reboot

**Scenario**: Target machine reboots while service has active jobs.

**Behavior**:
- Service stops (OS terminates all processes)
- On reboot, service auto-starts (if configured)
- Previous jobs are lost (expected)
- Client must handle reconnection and retry

**No special handling needed** (OS handles cleanup).

---

## 7. Implementation Checklist for Rust Agent

### Phase 1: Core Infrastructure

- [ ] **Service Host**
  - [ ] Implement Windows Service entry point (`ServiceMain`, `ServiceCtrlHandler`)
  - [ ] Register service with SCM (separate installer/configuration)
  - [ ] Implement graceful shutdown handler
  - [ ] Add service status reporting (`SetServiceStatus`)

- [ ] **Pipe Server**
  - [ ] Create named pipe with security descriptor (ACL)
  - [ ] Implement connection acceptance loop
  - [ ] Add client impersonation and authorization
  - [ ] Implement connection limit (max concurrent clients)
  - [ ] Add connection logging (client identity)

- [ ] **Protocol Handler**
  - [ ] Implement frame parsing (binary format)
  - [ ] Implement frame writing (with retry on partial writes)
  - [ ] Add protocol version negotiation (HELLO/HELLO_ACK)
  - [ ] Implement frame routing (dispatch by type)
  - [ ] Add malformed frame handling (send ERROR, disconnect)

### Phase 2: Job Management

- [ ] **Job Manager**
  - [ ] Implement job registry (concurrent hash map)
  - [ ] Add job ID allocation (atomic counter)
  - [ ] Implement job state machine (with mutex protection)
  - [ ] Add job lookup and state queries
  - [ ] Implement job cancellation API

- [ ] **Process Launcher**
  - [ ] Implement command-line parsing (Windows quoting rules)
  - [ ] Create anonymous pipes for stdio (non-inheritable)
  - [ ] Implement `CreateProcess` wrapper with proper `STARTUPINFO`
  - [ ] Add Windows Job Object creation (for tree termination)
  - [ ] Implement token duplication (for "run as" semantics)
  - [ ] Add input validation (command length, path sanitization)
  - [ ] Handle CreateProcess errors gracefully

### Phase 3: I/O Streaming

- [ ] **IO Pump**
  - [ ] Implement stdout pump thread (read pipe → OUTPUT frames)
  - [ ] Implement stderr pump thread (read pipe → OUTPUT frames)
  - [ ] Add stdin writer (OUTPUT frames → write to pipe)
  - [ ] Implement end-of-stream detection (broken pipe)
  - [ ] Add sequence numbers per stream
  - [ ] Implement flow control (sliding window)
  - [ ] Add backpressure handling (block or drop policy)

- [ ] **Cancellation/Timeout**
  - [ ] Implement timeout monitor (per-job wall-clock timer)
  - [ ] Add cancellation token propagation
  - [ ] Implement process tree termination (Job Object)
  - [ ] Add graceful vs forceful termination logic
  - [ ] Ensure no zombie processes (wait for exit, close handles)

### Phase 4: Security & Auditing

- [ ] **Security**
  - [ ] Implement ACL construction (SYSTEM + Administrators)
  - [ ] Add client authorization (verify admin group membership)
  - [ ] Implement input sanitization (command, paths, env vars)
  - [ ] Add frame size limits (prevent DoS)
  - [ ] Test with non-admin users (should be denied)

- [ ] **Audit Logging**
  - [ ] Implement Windows Event Log integration
  - [ ] Add structured logging (with job_id correlation)
  - [ ] Log all command executions (who/what/when/result)
  - [ ] Log access denials
  - [ ] Add optional file-based audit log

### Phase 5: Reliability & Diagnostics

- [ ] **Error Handling**
  - [ ] Implement comprehensive error types (custom `Error` enum)
  - [ ] Add error context (job_id, component, Windows error codes)
  - [ ] Map Windows errors to protocol ERROR frames
  - [ ] Add retry logic for transient failures (pipe I/O)

- [ ] **Cleanup & Resource Management**
  - [ ] Implement RAII for handles (custom `Handle` wrapper with `Drop`)
  - [ ] Ensure all handles closed on job completion/cancel
  - [ ] Verify no handle leaks (test with handle monitoring)
  - [ ] Add thread cleanup (join all pump threads)

- [ ] **Diagnostics**
  - [ ] Add health check endpoint (PING/PONG)
  - [ ] Implement job listing (for debugging)
  - [ ] Add verbose logging mode (trace all frame I/O)
  - [ ] Create diagnostic command (list active jobs, handle counts)

### Phase 6: Testing & Validation

- [ ] **Unit Tests**
  - [ ] Test frame parsing/writing (all frame types)
  - [ ] Test job state machine transitions
  - [ ] Test command-line parsing (quoting, special chars)
  - [ ] Test authorization logic

- [ ] **Integration Tests**
  - [ ] Test full job lifecycle (RUN → OUTPUT → EXIT)
  - [ ] Test cancellation (client disconnect, explicit CANCEL)
  - [ ] Test timeout handling
  - [ ] Test concurrent jobs (stress test)
  - [ ] Test backpressure (slow client, verify blocking/dropping)

- [ ] **Security Tests**
  - [ ] Test non-admin access (should be denied)
  - [ ] Test command injection attempts (should be sanitized)
  - [ ] Test large command lines (should be rejected or truncated)
  - [ ] Test handle exhaustion (should reject new jobs)

- [ ] **Failure Scenario Tests**
  - [ ] Test client disconnect mid-job (should cancel)
  - [ ] Test service stop during active jobs (should graceful shutdown)
  - [ ] Test invalid executable (should fail gracefully)
  - [ ] Test pipe I/O failures (should handle gracefully)

### Phase 7: Documentation & Deployment

- [ ] **Documentation**
  - [ ] Write protocol specification (frame formats, state machine)
  - [ ] Document configuration options (timeouts, limits, policies)
  - [ ] Create troubleshooting guide (common errors, diagnostics)
  - [ ] Document security model (ACLs, authorization)

- [ ] **Deployment**
  - [ ] Create service installer (register with SCM)
  - [ ] Create configuration file (pipe name, limits, security settings)
  - [ ] Add service recovery options (restart on failure)
  - [ ] Create uninstaller

---

## 8. Recommended Defaults and Configurable Knobs

### 8.1 Default Values

```toml
[service]
pipe_name = "\\.\pipe\VirimaRemoteAgent"
max_concurrent_clients = 64
max_concurrent_jobs_per_client = 8
graceful_shutdown_timeout_sec = 30

[job]
default_timeout_sec = 3600  # 1 hour
max_command_length = 32768  # 32KB
max_env_size = 65536        # 64KB
max_working_dir_length = 260

[io]
stdout_buffer_size = 65536      # 64KB
stderr_buffer_size = 65536      # 64KB
send_window_size = 65536        # 64KB per stream
max_frame_size = 1048576        # 1MB
frame_payload_max = 32768       # 32KB
backpressure_policy = "block"   # or "drop"

[security]
require_admin = true
allowed_groups = ["BUILTIN\\Administrators"]
audit_log_file = "C:\\ProgramData\\Virima\\RemoteAgent\\audit.log"
audit_retention_days = 90

[logging]
level = "INFO"  # ERROR, WARN, INFO, DEBUG, TRACE
event_log_source = "VirimaRemoteAgent"
file_log_path = "C:\\ProgramData\\Virima\\RemoteAgent\\service.log"
```

### 8.2 Configurable Knobs

**Performance Tuning**:
- `max_concurrent_jobs`: Increase for high-throughput scenarios
- `send_window_size`: Increase for high-bandwidth scenarios (risk: more memory)
- `frame_payload_max`: Increase for efficiency (risk: latency)

**Reliability**:
- `default_timeout_sec`: Adjust based on expected job duration
- `graceful_shutdown_timeout_sec`: Increase if jobs are long-running
- `backpressure_policy`: Use "drop" only if loss is acceptable

**Security**:
- `require_admin`: Set to `false` only if custom authorization implemented
- `allowed_groups`: Add custom security groups
- `max_command_length`: Reduce to prevent DoS (if needed)

---

## 9. Dataflow: Single RUN Request

### 9.1 Sequence Diagram

```
Client                    Pipe Server        Protocol Handler    Job Manager    Process Launcher    IO Pump
  |                            |                     |                |                 |            |
  |--HELLO-------------------->|                     |                |                 |            |
  |                            |--parse frame------->|                |                 |            |
  |                            |                     |--validate----->|                |            |
  |<--HELLO_ACK----------------|                     |                |                |            |
  |                            |                     |                |                 |            |
  |--RUN (cmdline)------------>|                     |                |                 |            |
  |                            |--parse frame------->|                |                 |            |
  |                            |                     |--validate----->|                |            |
  |                            |                     |--create_job--->|                |            |
  |                            |                     |                |--alloc job_id---|            |
  |                            |                     |<--job_id-------|                |            |
  |<--RUN_ACK (job_id)---------|                     |                |                 |            |
  |                            |                     |                |                 |            |
  |                            |                     |                |--launch_process->|            |
  |                            |                     |                |                 |--CreateProcess
  |                            |                     |                |                 |--create pipes
  |                            |                     |                |<--process_handle-|            |
  |                            |                     |                |                 |            |
  |                            |                     |                |--start_pumps---->|            |
  |                            |                     |                |                 |--spawn threads
  |<--STATUS (Running)---------|                     |                |                 |            |
  |                            |                     |                |                 |            |
  |                            |                     |                |                 |--read stdout
  |<--OUTPUT (stdout, seq=0)---|                     |                |                 |            |
  |                            |                     |                |                 |--read stderr
  |<--OUTPUT (stderr, seq=0)---|                     |                |                 |            |
  |                            |                     |                |                 |            |
  |                            |                     |                |                 |--process exits
  |                            |                     |                |--wait_exit------->|            |
  |                            |                     |                |<--exit_code------|            |
  |                            |                     |                |                 |--drain IO
  |<--OUTPUT (stdout, EOS)-----|                     |                |                 |            |
  |<--OUTPUT (stderr, EOS)-----|                     |                |                 |            |
  |<--EXIT (exit_code)---------|                     |                |                 |            |
  |                            |                     |                |--cleanup-------->|            |
```

### 9.2 Detailed Steps

1. **Connection**: Client opens named pipe → Pipe server accepts → Impersonates client → Verifies admin → Spawns handler task

2. **Protocol Negotiation**: Client sends `HELLO` → Server responds `HELLO_ACK` with version

3. **Job Creation**: Client sends `RUN` frame → Protocol handler validates → Job manager allocates `job_id` → Returns `RUN_ACK`

4. **Process Launch**: Job manager calls Process launcher → Creates pipes → Calls `CreateProcess` → Attaches to Job Object → Returns process handle

5. **IO Pump Start**: Job manager spawns stdout/stderr pump threads → Pumps begin reading pipes

6. **Status Update**: Job manager sends `STATUS` frame: `Running`

7. **Output Streaming**: Pumps read chunks → Check flow control window → Send `OUTPUT` frames → Update sequence numbers

8. **Process Exit**: Process exits → Job manager detects via `WaitForSingleObject` → Pumps drain remaining output → Send `EXIT` frame

9. **Cleanup**: Close handles → Join pump threads → Remove job from registry → Log completion

---

## 10. Additional Considerations

### 10.1 Windows-Specific Details

**Job Objects**:
- Use `CreateJobObject` to create container for process tree
- Assign process to job via `AssignProcessToJobObject`
- Terminate entire tree via `TerminateJobObject` (prevents orphaned children)

**Handle Inheritance**:
- Set `bInheritHandle=FALSE` on parent's pipe handles
- Duplicate handles with `bInheritHandle=TRUE` for child
- Close duplicated handles in parent after `CreateProcess`

**Impersonation**:
- `ImpersonateNamedPipeClient` assumes client's token
- Use token for authorization checks
- Revert via `RevertToSelf` (or keep for audit)

**Service Context**:
- Service runs as `LocalSystem` (highest privileges)
- Child processes inherit SYSTEM context (unless token duplicated)
- For "run as user": Duplicate client's impersonated token

### 10.2 Rust-Specific Implementation Notes

**Error Handling**:
- Use `windows-rs` or `winapi` crates for Windows APIs
- Map Windows errors (`GetLastError`) to Rust `Result` types
- Use `?` operator for error propagation

**Concurrency**:
- Use `std::thread` for IO pumps (blocking I/O)
- Use `std::sync::{Arc, Mutex, RwLock}` for shared state
- Consider `tokio` for async I/O (optional, adds complexity)

**Resource Management**:
- Implement `Drop` for handle wrappers (ensure cleanup)
- Use `RAII` patterns throughout
- Avoid `unsafe` where possible (use safe wrappers)

**Testing**:
- Mock Windows APIs for unit tests (use trait objects)
- Integration tests require Windows environment
- Use `#[cfg(test)]` for test-only code

---

## Conclusion

This architecture provides a robust, production-ready design for a Windows remote agent service. The multiplexed pipe design simplifies connection management and enables proper flow control, while the component breakdown ensures maintainability and testability. The security model restricts access to administrators and provides comprehensive auditing, and the failure handling ensures reliable operation even under adverse conditions.

The implementation checklist provides a clear roadmap for building the service incrementally, with each phase building on the previous one. The recommended defaults balance performance, reliability, and security, while the configurable knobs allow tuning for specific deployment scenarios.

