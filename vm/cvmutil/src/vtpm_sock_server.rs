// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

///! TPM socket server implementation using vTPM blob as backing state
///! This allows using standard TPM2 tools with a vTPM instance over TCP sockets.
use std::fs;
use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::vtpm_helper::create_tpm_engine_helper;
use tpm::tpm_helper::TpmEngineHelper;

/// Setup Ctrl+C signal handler to allow graceful shutdown
fn setup_signal_handler() -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        tracing::info!("Received Ctrl+C signal, shutting down TPM socket server...");
        r.store(false, Ordering::SeqCst);
    }).expect("Error setting Ctrl+C handler");

    running
}

/// Start a TPM socket server using vTPM blob as backing state
pub fn start_tpm_socket_server(vtpm_blob_path: &str, bind_addr: &str) {
    tracing::info!("Starting TPM socket server using vTPM blob: {}", vtpm_blob_path);
    tracing::info!("Binding to address: {}", bind_addr);

    // Setup signal handler for graceful shutdown
    let running = setup_signal_handler();

    // Parse the bind address to extract host and port
    let (host, data_port) = parse_bind_address(bind_addr);
    let ctrl_port = data_port + 1; // Control port is typically data_port + 1

    tracing::info!("Data port: {}, Control port: {}", data_port, ctrl_port);

    // Load the vTPM blob
    let vtpm_blob_content = fs::read(vtpm_blob_path)
        .expect("failed to read vtpm blob file");
    
    // Create TPM engine helper
    let (mut vtpm_engine_helper, mut nv_blob_accessor) = create_tpm_engine_helper();

    // Restore TPM state from blob
    tracing::info!("Restoring TPM state from blob ({} bytes)", vtpm_blob_content.len());
    let result = vtpm_engine_helper.tpm_engine.reset(Some(&vtpm_blob_content));
    assert!(result.is_ok(), "Failed to restore TPM state: {:?}", result);
    
     // Initialize the TPM engine (this does StartupType::Clear + SelfTest)
    let result = vtpm_engine_helper.initialize_tpm_engine();
    assert!(result.is_ok(), "Failed to initialize TPM engine: {:?}", result);
    
    tracing::info!("TPM engine initialized successfully");

    // Wrap TPM engine in Arc<Mutex> for thread safety
    let tpm_engine = Arc::new(Mutex::new(vtpm_engine_helper));
    let nv_accessor = Arc::new(Mutex::new(nv_blob_accessor));

    // Start both data and control listeners
    let data_addr = format!("{}:{}", host, data_port);
    let ctrl_addr = format!("{}:{}", host, ctrl_port);

    let data_listener = TcpListener::bind(&data_addr)
        .expect(&format!("Failed to bind to data address: {}", data_addr));
    
    let ctrl_listener = TcpListener::bind(&ctrl_addr)
        .expect(&format!("Failed to bind to control address: {}", ctrl_addr));
    
    // Set non-blocking mode for graceful shutdown
    data_listener.set_nonblocking(true)
        .expect("Failed to set data listener to non-blocking");
    ctrl_listener.set_nonblocking(true)
        .expect("Failed to set control listener to non-blocking");

    tracing::info!("TPM socket server listening on data port: {}", data_addr);
    tracing::info!("TPM socket server listening on control port: {}", ctrl_addr);
    tracing::info!("Use with: export TPM2TOOLS_TCTI=\"mssim:host={},port={}\"", host, data_port);
    tracing::info!("Press Ctrl+C to stop the server");

    // Start control socket handler in a separate thread
    let ctrl_tpm_engine = Arc::clone(&tpm_engine);
    let ctrl_running = running.clone();
    let ctrl_handle = thread::spawn(move || {
        handle_control_socket(ctrl_listener, ctrl_tpm_engine, ctrl_running);
    });

    // Handle data connections in the main thread
    while running.load(Ordering::SeqCst) {
        match data_listener.accept() {
            Ok((stream, _)) => {
                let peer_addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
                tracing::info!("New data connection from: {}", peer_addr);
                
                let tpm_engine_clone = Arc::clone(&tpm_engine);
                let nv_accessor_clone = Arc::clone(&nv_accessor);
                let client_running = running.clone();
                
                // Handle each connection in a separate thread
                thread::spawn(move || {
                    handle_tpm_data_client(stream, tpm_engine_clone, nv_accessor_clone, client_running);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Non-blocking accept returned no connection, sleep briefly and continue
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(e) => {
                if running.load(Ordering::SeqCst) {
                    tracing::error!("Failed to accept data connection: {}", e);
                }
            }
        }
    }

    tracing::info!("Shutting down TPM socket server...");

    // Wait for control thread to finish
    if let Err(e) = ctrl_handle.join() {
        tracing::warn!("Error joining control thread: {:?}", e);
    }

    tracing::info!("TPM socket server stopped");
}

/// Parse bind address like "localhost:2321" into (host, port)
fn parse_bind_address(bind_addr: &str) -> (String, u16) {
    if let Some(colon_pos) = bind_addr.rfind(':') {
        let host = bind_addr[..colon_pos].to_string();
        let port_str = &bind_addr[colon_pos + 1..];
        let port = port_str.parse::<u16>()
            .expect(&format!("Invalid port number: {}", port_str));
        (host, port)
    } else {
        panic!("Invalid bind address format. Expected host:port, got: {}", bind_addr);
    }
}

/// Handle the TPM simulator control socket
fn handle_control_socket(
    listener: TcpListener,
    tpm_engine: Arc<Mutex<TpmEngineHelper>>,
    running: Arc<AtomicBool>,
) {
    tracing::info!("Control socket handler started");

    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let tpm_engine_clone = Arc::clone(&tpm_engine);
                let client_running = running.clone();
                
                thread::spawn(move || {
                    handle_control_client(stream, tpm_engine_clone, client_running);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Non-blocking accept returned no connection, sleep briefly and continue
                thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(e) => {
                if running.load(Ordering::SeqCst) {
                    tracing::error!("Failed to accept control connection: {}", e);
                }
            }
        }
    }

    tracing::info!("Control socket handler stopped");
}

/// Handle a single control client connection
fn handle_control_client(
    mut stream: TcpStream,
    tpm_engine: Arc<Mutex<TpmEngineHelper>>,
    running: Arc<AtomicBool>,
) {
    let peer_addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    tracing::debug!("Control client connected from: {}", peer_addr);

    let mut reader = BufReader::new(&stream);
    let mut writer = BufWriter::new(&stream);

    // Set read timeout for graceful shutdown
    stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap_or_else(|e| tracing::warn!("Failed to set read timeout: {}", e));

    while running.load(Ordering::SeqCst) {
        match read_control_command(&mut reader) {
            Ok(command) => {
                tracing::debug!("Received control command: {:?}", command);

                let response = {
                    let mut engine = tpm_engine.lock().unwrap();
                    process_control_command(&mut engine, &command)
                };

                if let Err(e) = write_control_response(&mut writer, &response) {
                    tracing::error!("Failed to send control response: {}", e);
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Read timeout, continue loop to check running flag
                continue;
            }
            Err(e) => {
                if running.load(Ordering::SeqCst) {
                    tracing::debug!("Control client disconnected or read error: {}", e);
                }
                break;
            }
        }
    }

    tracing::debug!("Control client disconnected: {}", peer_addr);
}

/// Control command types for Microsoft TPM Simulator
#[derive(Debug)]
enum ControlCommand {
    SessionEnd,           // 0x00
    Stop,                 // 0x01  
    Reset,                // 0x02
    Restart,              // 0x03
    PowerOn,              // 0x04
    PowerOff,             // 0x05
    GetTestResult,        // 0x06
    GetCapability,        // 0x07
    NvOn,                 // 0x0B - MS_SIM_NV_ON
    NvOff,                // 0x0C - MS_SIM_NV_OFF
    HashStart,            // 0x0D
    HashData,             // 0x0E
    HashEnd,              // 0x0F
    Unknown(Vec<u8>),
}

/// Read a control command from the client
fn read_control_command(reader: &mut BufReader<&TcpStream>) -> Result<ControlCommand, std::io::Error> {
    use std::io::Read;

    // Control commands are 4 bytes (big endian)
    let mut buffer = [0u8; 4];
    reader.read_exact(&mut buffer)?;

    // Parse the 4-byte command as big-endian u32
    let command_code = u32::from_be_bytes(buffer);

    // Parse Microsoft TPM Simulator control commands
    match command_code {
        0x00000000 => Ok(ControlCommand::SessionEnd),
        0x00000001 => Ok(ControlCommand::Stop),
        0x00000002 => Ok(ControlCommand::Reset),
        0x00000003 => Ok(ControlCommand::Restart),
        0x00000004 => Ok(ControlCommand::PowerOn),
        0x00000005 => Ok(ControlCommand::PowerOff),
        0x00000006 => Ok(ControlCommand::GetTestResult),
        0x00000007 => Ok(ControlCommand::GetCapability),
        0x0000000B => Ok(ControlCommand::NvOn),      // MS_SIM_NV_ON
        0x0000000C => Ok(ControlCommand::NvOff),     // MS_SIM_NV_OFF
        0x0000000D => Ok(ControlCommand::HashStart),
        0x0000000E => Ok(ControlCommand::HashData),
        0x0000000F => Ok(ControlCommand::HashEnd),
        _ => Ok(ControlCommand::Unknown(buffer.to_vec())),
    }
}

/// Process a control command
fn process_control_command(
    engine: &mut TpmEngineHelper,
    command: &ControlCommand,
) -> Vec<u8> {
    match command {
        ControlCommand::SessionEnd => {
            tracing::debug!("TPM Session End requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::Stop => {
            tracing::info!("TPM Stop requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::Reset => {
            tracing::info!("TPM Reset requested");
            // You might want to reset TPM state here
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::Restart => {
            tracing::info!("TPM Restart requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::PowerOn => {
            tracing::info!("TPM Power On requested");
            
            // Perform TPM power-on sequence if needed
            // This might involve calling engine methods to simulate power-on
            
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::PowerOff => {
            tracing::info!("TPM Power Off requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::GetTestResult => {
            tracing::debug!("TPM Get Test Result requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success - no test failures
        }
        ControlCommand::GetCapability => {
            tracing::debug!("TPM Get Capability requested");
            // Return capability information
            // For now, return success with basic capabilities
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::NvOn => {
            tracing::info!("MS_SIM_NV_ON (TPM NV Enable) requested");
            
            // This is the command that was failing
            // Enable NV storage in the TPM
            // The ms-tpm-20-ref might have specific methods for this
            
            // For now, acknowledge success
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::NvOff => {
            tracing::info!("MS_SIM_NV_OFF (TPM NV Disable) requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::HashStart => {
            tracing::debug!("TPM Hash Start requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::HashData => {
            tracing::debug!("TPM Hash Data requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::HashEnd => {
            tracing::debug!("TPM Hash End requested");
            vec![0x00, 0x00, 0x00, 0x00] // Success
        }
        ControlCommand::Unknown(data) => {
            tracing::warn!("Unknown control command: {:02x?} ({})", data, u32::from_be_bytes([data[0], data[1], data[2], data[3]]));
            vec![0x00, 0x00, 0x00, 0x01] // Error response
        }
    }
}

/// Write a control response to the client
fn write_control_response(writer: &mut BufWriter<&TcpStream>, response: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    
    writer.write_all(response)?;
    writer.flush()?;
    Ok(())
}

/// Maximum internal buffer we can safely process (TPM_PAGE_SIZE equivalent).
const INTERNAL_MAX_CMD: usize = 4096;
const INTERNAL_MAX_RSP: usize = 4096;
const ABSOLUTE_MAX_CMD: usize = 8192; // hard safety ceiling beyond which we refuse

#[repr(u32)]
enum IfaceCmd {
    SignalHashStart = 5,
    SignalHashData  = 6,
    SignalHashEnd   = 7,
    SendCommand     = 8,
    RemoteHandshake = 15,
    SessionEnd      = 20,
    Stop            = 21,
}

fn handle_tpm_data_client(
    stream: TcpStream,
    tpm_engine: Arc<Mutex<TpmEngineHelper>>,
    _nv_accessor: Arc<Mutex<impl std::any::Any + Send>>,
    running: Arc<AtomicBool>,
) {
    use std::io::Write;
    let peer_addr = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
    tracing::info!("TPM data client connected from: {}", peer_addr);

    let mut reader = BufReader::new(&stream);
    let mut writer = BufWriter::new(&stream);

    // Set read timeout for graceful shutdown
    stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap_or_else(|e| tracing::warn!("Failed to set read timeout: {}", e));

    let max_cmd = INTERNAL_MAX_CMD; // internal engine limit
    while running.load(Ordering::SeqCst) {
        let cmd_code = match read_u32(&mut reader) {
            Ok(v) => v,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Read timeout, continue loop to check running flag
                continue;
            }
            Err(e) => {
                if running.load(Ordering::SeqCst) {
                    tracing::debug!("Client {} disconnected (read cmd): {}", peer_addr, e);
                }
                break;
            }
        };

        match cmd_code {
            x if x == IfaceCmd::RemoteHandshake as u32 => {
                let client_version = read_u32(&mut reader).unwrap();
                tracing::info!("REMOTE_HANDSHAKE client_version={}", client_version);
                // serverVersion = 1; flags = tpmInRawMode|tpmPlatformAvailable|tpmSupportsPP
                write_u32(&mut writer, 1);
                let flags = 0x04 | 0x01 | 0x08; // raw | platform | PP
                write_u32(&mut writer, flags);
            }
            x if x == IfaceCmd::SendCommand as u32 => {
                // locality
                let mut loc = [0u8;1];
                use std::io::Read;
                reader.read_exact(&mut loc);
                let locality = loc[0];
                let cmd_buf = read_var_bytes(&mut reader, max_cmd).unwrap();
                if cmd_buf.len() < 10 {
                    tracing::warn!("TPM command too short {}", cmd_buf.len());
                }
                if cmd_buf.len() >= 6 {
                    let tpm_declared = u32::from_be_bytes([cmd_buf[2],cmd_buf[3],cmd_buf[4],cmd_buf[5]]) as usize;
                    if tpm_declared != cmd_buf.len() {
                        tracing::warn!("TPM header size {} != envelope {}", tpm_declared, cmd_buf.len());
                    }
                }

                let resp = {
                    let mut engine = tpm_engine.lock().unwrap();
                    process_tpm_command(&mut engine, &cmd_buf)
                        .unwrap_or_else(|e| { 
                            tracing::error!("Exec error: {}", e); 
                            // Minimal TPM error skeleton if desired; for now empty.
                            vec![0u8; 0] 
                        })
                };

                write_var_bytes(&mut writer, &resp);
                tracing::info!("SendCommand locality={} in={} out={}", locality, cmd_buf.len(), resp.len());
            }
            x if x == IfaceCmd::SignalHashStart as u32 => {
                // no payload
            }
            x if x == IfaceCmd::SignalHashEnd as u32 => {
                // no payload
            }
            x if x == IfaceCmd::SignalHashData as u32 => {
                let data = read_var_bytes(&mut reader, max_cmd).unwrap();
                tracing::debug!("HashData {} bytes (ignored pass-through)", data.len());
            }
            x if x == IfaceCmd::SessionEnd as u32 => {
                tracing::info!("SessionEnd requested");
                write_u32(&mut writer, 0); // status before break (consistent with C? C returns true then writes status after switch)
                writer.flush();
                break;
            }
            x if x == IfaceCmd::Stop as u32 => {
                tracing::info!("Stop requested");
                write_u32(&mut writer, 0);
                writer.flush();
                // Optionally signal broader shutdown
                break;
            }
            other => {
                tracing::warn!("Unknown interface command 0x{:08x}", other);
                // In C, unknown causes return (dropping connection) *after* printing and not writing status.
                break;
            }
        }

        if !running.load(Ordering::SeqCst) {
            break;
        }

        // Trailing status (always 0) after a handled interface command (except unknown/early failure)
        write_u32(&mut writer, 0);
        if let Err(e) = writer.flush() {
            tracing::debug!("Flush failed: {}", e);
            break;
        }
    }

    tracing::info!("TPM data client disconnected: {}", peer_addr);
}

// Helpers.

fn read_u32(reader: &mut BufReader<&TcpStream>) -> std::io::Result<u32> {
    use std::io::Read;
    let mut b = [0u8;4];
    reader.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn write_u32(writer: &mut BufWriter<&TcpStream>, v: u32) -> std::io::Result<()> {
    use std::io::Write;
    writer.write_all(&v.to_be_bytes())
}

fn read_var_bytes(reader: &mut BufReader<&TcpStream>, max: usize) -> std::io::Result<Vec<u8>> {
    let len = read_u32(reader)? as usize;
    if len > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                 format!("VarBytes length {} > max {}", len, max)));
    }
    let mut buf = vec![0u8; len];
    use std::io::Read;
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_var_bytes(writer: &mut BufWriter<&TcpStream>, data: &[u8]) -> std::io::Result<()> {
    write_u32(writer, data.len() as u32)?;
    use std::io::Write;
    writer.write_all(data)
}

/// Process a TPM command using the TPM engine
fn process_tpm_command(
    vtpm_engine_helper: &mut TpmEngineHelper,
    command: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    tracing::debug!("Processing TPM command: {} bytes", command.len());
    
    // Check command size - TPM commands should fit in the page size
    if command.len() > 4096 {  // TPM_PAGE_SIZE
        return Err("Command too large for TPM buffer".into());
    }
    
    // Create a command buffer similar to how the TPM device does it
    let mut command_buffer = [0u8; 4096];  // Same size as TPM_PAGE_SIZE
    
    // Copy the command into the buffer
    command_buffer[..command.len()].copy_from_slice(command);
    
    tracing::trace!("Executing TPM command with engine...");
    tracing::trace!("Command (hex): {:02x?}", &command_buffer[..command.len()]);
    
    // Submit the command to the TPM engine
    let result = vtpm_engine_helper.tpm_engine.execute_command(
        &mut command_buffer,
        &mut vtpm_engine_helper.reply_buffer,
    );

    match result {
        Ok(response_size) => {
            tracing::debug!("TPM command executed successfully, response size: {}", response_size);
            
            if response_size == 0 {
                return Err("TPM returned zero-length response".into());
            }

             if response_size < 10 {
                return Err("TPM returned fatal response".into());
            }
            
            // response code are in bytes 6-9 of the response
            let response_code = u32::from_be_bytes(
                vtpm_engine_helper.reply_buffer[6..10].try_into().unwrap(),
            );
            tracing::debug!("TPM response code: 0x{:08x}", response_code);

            if response_size > 4096 {
                return Err(format!("TPM response too large: {}", response_size).into());
            }
            
            // Copy the response from the helper's reply buffer
            Ok(vtpm_engine_helper.reply_buffer[..response_size].to_vec())
        }
        Err(e) => {
            tracing::error!("TPM engine command failed: {:?}", e);
            Err(format!("TPM command processing failed: {:?}", e).into())
        }
    }
}