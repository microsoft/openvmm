//! # Bare Metal TPM Driver
//! 
//! A kernel-mode TPM 2.0 driver written in Rust.
//! This module provides direct hardware access to TPM functionality without
//! requiring user-mode components or additional dependencies.
//!
//! ## Features
//! - Direct hardware access to TPM 2.0 devices
//! - Support for both MMIO and TIS (TPM Interface Specification) interfaces
//! - Safe Rust abstractions for TPM commands
//! - Minimizes unsafe code to hardware interaction boundaries
//! - No_std compatible for kernel environments
//! - Comprehensive logging via the `log` crate
//! - Uses alloc::vec::Vec for dynamic memory allocation

#![no_std]
#![allow(dead_code)] // Remove this in production code

extern crate alloc;

use core::fmt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use log::{error, warn, info, debug, trace};
use alloc::vec::Vec;

use crate::arch::rtc::delay_sec;

/// TPM command codes as defined in the TPM 2.0 specification
#[repr(u32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TpmCommandCode {
    Startup = 0x00000144,
    SelfTest = 0x00000143,
    GetRandom = 0x0000017B,
    GetCapability = 0x0000017A,
    PCRExtend = 0x00000182,
    PCRRead = 0x0000017E,
    CreatePrimary = 0x00000131,
    Create = 0x00000153,
    Load = 0x00000157,
    Sign = 0x0000015D,
    VerifySignature = 0x0000015E,
    // Add more command codes as needed
}

impl fmt::Display for TpmCommandCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TpmCommandCode::Startup => write!(f, "TPM_CC_Startup"),
            TpmCommandCode::SelfTest => write!(f, "TPM_CC_SelfTest"),
            TpmCommandCode::GetRandom => write!(f, "TPM_CC_GetRandom"),
            TpmCommandCode::GetCapability => write!(f, "TPM_CC_GetCapability"),
            TpmCommandCode::PCRExtend => write!(f, "TPM_CC_PCR_Extend"),
            TpmCommandCode::PCRRead => write!(f, "TPM_CC_PCR_Read"),
            TpmCommandCode::CreatePrimary => write!(f, "TPM_CC_CreatePrimary"),
            TpmCommandCode::Create => write!(f, "TPM_CC_Create"),
            TpmCommandCode::Load => write!(f, "TPM_CC_Load"),
            TpmCommandCode::Sign => write!(f, "TPM_CC_Sign"),
            TpmCommandCode::VerifySignature => write!(f, "TPM_CC_VerifySignature"),
        }
    }
}

/// TPM interface types supported by this driver
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TpmInterfaceType {
    /// TPM Interface Specification (TIS) - Port I/O based
    Tis,
    /// Memory-mapped I/O interface
    Mmio,
    /// Command Response Buffer Interface
    Crb,
    /// Firmware TPM Interface
    Fifo,
}

impl fmt::Display for TpmInterfaceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TpmInterfaceType::Tis => write!(f, "TIS"),
            TpmInterfaceType::Mmio => write!(f, "MMIO"),
            TpmInterfaceType::Crb => write!(f, "CRB"),
            TpmInterfaceType::Fifo => write!(f, "FIFO"),
        }
    }
}

/// TPM hardware address information
pub struct TpmAddress {
    interface_type: TpmInterfaceType,
    base_address: usize,
}

/// Error types for TPM operations
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TpmError {
    Timeout,
    BadParameter,
    CommunicationFailure,
    BufferTooSmall,
    UnsupportedCommand,
    HardwareFailure,
    AuthFailure,
    TpmResponseError(u32),
    NotInitialized,
    AllocationFailure,
}

impl fmt::Display for TpmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TpmError::Timeout => write!(f, "TPM operation timed out"),
            TpmError::BadParameter => write!(f, "Invalid parameter provided to TPM"),
            TpmError::CommunicationFailure => write!(f, "Failed to communicate with TPM"),
            TpmError::BufferTooSmall => write!(f, "Buffer too small for TPM operation"),
            TpmError::UnsupportedCommand => write!(f, "Command not supported by TPM"),
            TpmError::HardwareFailure => write!(f, "TPM hardware failure"),
            TpmError::AuthFailure => write!(f, "TPM authorization failure"),
            TpmError::TpmResponseError(code) => write!(f, "TPM response error: 0x{code:08X}"),
            TpmError::NotInitialized => write!(f, "TPM driver not initialized"),
            TpmError::AllocationFailure => write!(f, "Memory allocation failure"),
        }
    }
}

/// TPM response structure for commands
#[repr(C, packed)]
pub struct TpmResponse {
    pub tag: u16,
    pub response_size: u32,
    pub response_code: u32,
    // Followed by command-specific response data
}

/// Status of the TPM device
pub struct TpmStatus {
    pub initialized: bool,
    pub active: bool,
    pub version_major: u8,
    pub version_minor: u8,
}

/// Memory buffer for TPM operations
pub struct TpmBuffer {
    data: [u8; Self::MAX_SIZE],
    len: usize,
}

impl TpmBuffer {
    const MAX_SIZE: usize = 4096;
    
    pub fn new() -> Self {
        debug!("Creating new TpmBuffer with capacity {}", Self::MAX_SIZE);
        Self {
            data: [0; Self::MAX_SIZE],
            len: 0,
        }
    }
    
    pub fn clear(&mut self) {
        trace!("Clearing TpmBuffer (previous length: {})", self.len);
        self.len = 0;
    }
    
    pub fn as_slice(&self) -> &[u8] {
        trace!("Accessing TpmBuffer as slice, length: {}", self.len);
        &self.data[..self.len]
    }
    
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        trace!("Accessing TpmBuffer as mutable slice, length: {}", self.len);
        &mut self.data[..self.len]
    }
    
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, TpmError> {
        if self.len + bytes.len() > Self::MAX_SIZE {
            error!("TpmBuffer write failed: buffer too small (current: {}, append: {}, max: {})",
                self.len, bytes.len(), Self::MAX_SIZE);
            return Err(TpmError::BufferTooSmall);
        }
        
        trace!("Writing {} bytes to TpmBuffer at offset {}", bytes.len(), self.len);
        self.data[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        
        Ok(bytes.len())
    }
    
    pub fn read(&mut self, bytes: &mut [u8]) -> Result<usize, TpmError> {
        let to_read = core::cmp::min(bytes.len(), self.len);
        trace!("Reading {} bytes from TpmBuffer (requested: {}, available: {})",
            to_read, bytes.len(), self.len);
            
        bytes[..to_read].copy_from_slice(&self.data[..to_read]);
        
        // Shift remaining data to beginning of buffer
        for i in 0..(self.len - to_read) {
            self.data[i] = self.data[i + to_read];
        }
        self.len -= to_read;
        
        Ok(to_read)
    }
    
    pub fn len(&self) -> usize {
        self.len
    }
    
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// TIS (TPM Interface Specification) specific constants and register offsets
mod tis {
    // TIS register space offsets
    pub const ACCESS: usize = 0x0;
    pub const STS: usize = 0x18;
    pub const DATA_FIFO: usize = 0x24;
    pub const INTERFACE_ID: usize = 0x30;
    pub const INT_ENABLE: usize = 0x08;
    pub const INT_VECTOR: usize = 0x0C;
    pub const INT_STATUS: usize = 0x10;
    
    // TIS access register bits
    pub const ACCESS_VALID: u8 = 0x80;
    pub const ACCESS_ACTIVE_LOCALITY: u8 = 0x20;
    pub const ACCESS_REQUEST_USE: u8 = 0x02;
    pub const ACCESS_SEIZE: u8 = 0x04;
    
    // TIS status register bits
    pub const STS_VALID: u8 = 0x80;
    pub const STS_COMMAND_READY: u8 = 0x40;
    pub const STS_DATA_AVAILABLE: u8 = 0x10;
    pub const STS_EXPECT: u8 = 0x08;
    pub const STS_GO: u8 = 0x20;
    pub const STS_RESPONSE_RETRY: u8 = 0x02;
}

/// CRB (Command Response Buffer) specific constants and register offsets
mod crb {
    pub const CONTROL_AREA_REQUEST: usize = 0x40;
    pub const CONTROL_AREA_STATUS: usize = 0x44;
    pub const CONTROL_CANCEL: usize = 0x18;
    pub const CONTROL_START: usize = 0x1C;
    pub const COMMAND_BUFFER: usize = 0x80;
    pub const RESPONSE_BUFFER: usize = 0x80;
    
    // CRB status register bits
    pub const CRB_STATUS_IDLE: u32 = 0x00000001;
    pub const CRB_STATUS_READY: u32 = 0x00000002;
}

/// The main TPM driver structure
pub struct TpmDriver {
    address: TpmAddress,
    initialized: AtomicBool,
    current_locality: u8,
}

impl TpmDriver {
    /// Create a new TPM driver instance with the specified interface
    pub fn new(interface_type: TpmInterfaceType, base_address: usize) -> Self {
        info!("Creating new TPM driver with {} interface at base address 0x{:X}", 
            interface_type, base_address);
            
        Self {
            address: TpmAddress {
                interface_type,
                base_address,
            },
            initialized: AtomicBool::new(false),
            current_locality: 1,
        }
    }
    
    /// Initialize the TPM driver and hardware
    pub fn initialize(&self) -> Result<(), TpmError> {
        // Skip initialization if already done
        if self.initialized.load(Ordering::SeqCst) {
            debug!("TPM driver already initialized, skipping initialization");
            return Ok(());
        }
        
        info!("Initializing TPM driver with {} interface", self.address.interface_type);
        
        match self.address.interface_type {
            TpmInterfaceType::Tis => {
                debug!("Initializing TPM using TIS interface");
                self.initialize_tis()?
            },
            TpmInterfaceType::Mmio => {
                debug!("Initializing TPM using MMIO interface");
                self.initialize_mmio()?
            },
            TpmInterfaceType::Crb => {
                debug!("Initializing TPM using CRB interface");
                self.initialize_crb()?
            },
            TpmInterfaceType::Fifo => {
                debug!("Initializing TPM using FIFO interface");
                self.initialize_fifo()?
            },
        }
        
        debug!("TPM interface initialized, sending startup command");
        
        // Send TPM startup command
        let mut cmd_buffer = TpmBuffer::new();
        self.build_startup_command(&mut cmd_buffer)?;
        
        debug!("Sending TPM_CC_Startup command, buffer size: {}", cmd_buffer.len());
        self.send_command(cmd_buffer.as_slice())?;
        
        info!("TPM driver successfully initialized");
        self.initialized.store(true, Ordering::SeqCst);
        Ok(())
    }
    
    /// Initialize TPM using TIS interface
    fn initialize_tis(&self) -> Result<(), TpmError> {
        debug!("Requesting access to locality {}", self.current_locality);
        
        // Request access to locality 0
        self.write_tis_reg8(tis::ACCESS, tis::ACCESS_REQUEST_USE)?;
        
        // Wait for access grant
        let mut timeout = 5;
        debug!("Waiting for locality {} access grant", self.current_locality);
        
        while timeout > 0 {
            let access = self.read_tis_reg8(tis::ACCESS)?;
            trace!("TIS ACCESS register: 0x{:02X}", access);
            
            if (access & tis::ACCESS_ACTIVE_LOCALITY) != 0 {
                debug!("Locality {} access granted", self.current_locality);
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for locality {} access", self.current_locality);
                return Err(TpmError::Timeout);
            }
            delay_sec(2);
        }
        
        // Check if TPM is valid
        let access = self.read_tis_reg8(tis::ACCESS)?;
        trace!("TIS ACCESS register after grant: 0x{:02X}", access);
        
        if (access & tis::ACCESS_VALID) == 0 {
            error!("TPM TIS interface not valid, ACCESS register: 0x{:02X}", access);
            return Err(TpmError::HardwareFailure);
        }
        
        debug!("Setting TPM to command ready state");
        // Set command ready
        self.write_tis_reg8(tis::STS, tis::STS_COMMAND_READY)?;
        
        // Wait for command ready state
        debug!("Waiting for command ready state");
        let mut timeout = 5;
        while timeout > 0 {
            let status = self.read_tis_reg8(tis::STS)?;
            trace!("TIS STS register: 0x{:02X}", status);
            
            if (status & tis::STS_COMMAND_READY) != 0 {
                debug!("TPM command ready state achieved");
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for command ready state");
                return Err(TpmError::Timeout);
            }
            // Small delay
            delay_sec(2);
        }
        
        info!("TPM TIS interface successfully initialized");
        Ok(())
    }
    
    /// Initialize TPM using MMIO interface
    fn initialize_mmio(&self) -> Result<(), TpmError> {
        debug!("Initializing TPM using MMIO interface (using TIS flow)");
        // This would be similar to TIS but with memory-mapped registers
        // For simplicity, we'll use the TIS initialization flow
        self.initialize_tis()
    }
    
    /// Initialize TPM using CRB interface
    fn initialize_crb(&self) -> Result<(), TpmError> {
        debug!("Setting TPM CRB interface to idle state");
        // Set to idle state
        self.write_crb_reg32(crb::CONTROL_AREA_REQUEST, 0x00)?;
        
        // Wait for TPM to become ready
        debug!("Waiting for TPM CRB to enter idle state");
        let mut timeout = 1000;
        while timeout > 0 {
            let status = self.read_crb_reg32(crb::CONTROL_AREA_STATUS)?;
            trace!("CRB status register: 0x{:08X}", status);
            
            if (status & crb::CRB_STATUS_IDLE) != 0 {
                debug!("TPM CRB entered idle state");
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for CRB idle state");
                return Err(TpmError::Timeout);
            }
            
            delay_sec(2);
        }
        
        debug!("Requesting TPM CRB ready state");
        // Request command ready state
        self.write_crb_reg32(crb::CONTROL_AREA_REQUEST, crb::CRB_STATUS_READY)?;
        
        // Wait for ready state
        debug!("Waiting for TPM CRB to enter ready state");
        let mut timeout = 1000;
        while timeout > 0 {
            let status = self.read_crb_reg32(crb::CONTROL_AREA_STATUS)?;
            trace!("CRB status register: 0x{:08X}", status);
            
            if (status & crb::CRB_STATUS_READY) != 0 {
                debug!("TPM CRB entered ready state");
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for CRB ready state");
                return Err(TpmError::Timeout);
            }
            delay_sec(2);
        }
        
        info!("TPM CRB interface successfully initialized");
        Ok(())
    }
    
    /// Initialize TPM using FIFO interface
    fn initialize_fifo(&self) -> Result<(), TpmError> {
        debug!("Initializing TPM using FIFO interface (using TIS flow)");
        // Similar to TIS for most FIFO implementations
        self.initialize_tis()
    }
    
    /// Build TPM startup command
    fn build_startup_command(&self, buffer: &mut TpmBuffer) -> Result<(), TpmError> {
        debug!("Building TPM_CC_Startup command");
        
        // TPM 2.0 Startup command structure
        // Tag: TPM_ST_NO_SESSIONS (0x8001)
        // Command size: 12 bytes
        // Command code: TPM_CC_Startup (0x00000144)
        // Startup type: TPM_SU_CLEAR (0x0000)
        
        let startup_cmd = [
            0x80, 0x01,                 // TPM_ST_NO_SESSIONS
            0x00, 0x00, 0x00, 0x0C,     // Command size (12 bytes)
            0x00, 0x00, 0x01, 0x44,     // TPM_CC_Startup
            0x00, 0x00                  // TPM_SU_CLEAR
        ];
        
        trace!("Startup command bytes: {:02X?}", startup_cmd);
        
        buffer.write(&startup_cmd)
            .map(|written| {
                debug!("TPM_CC_Startup command built, size: {} bytes", written);
                
            })
            .map_err(|err| {
                error!("Failed to build TPM_CC_Startup command: {}", err);
                TpmError::BufferTooSmall
            })
    }
    
    /// Send a raw command to the TPM
    pub fn send_command(&self, command: &[u8]) -> Result<Vec<u8>, TpmError> {
        if !self.initialized.load(Ordering::SeqCst) && command[6..10] != [0x00, 0x00, 0x01, 0x44] {
            // Skip check for Startup command, which is sent during initialization
            error!("TPM driver not initialized");
            return Err(TpmError::NotInitialized);
        }
        
        debug!("Sending TPM command, size: {} bytes", command.len());
        trace!("Command header: {:02X?}", &command[0..10]);
        
        match self.address.interface_type {
            TpmInterfaceType::Tis | TpmInterfaceType::Fifo => {
                debug!("Using TIS/FIFO protocol for command");
                self.send_command_tis(command)
            },
            TpmInterfaceType::Mmio => {
                debug!("Using MMIO protocol for command");
                self.send_command_mmio(command)
            },
            TpmInterfaceType::Crb => {
                debug!("Using CRB protocol for command");
                self.send_command_crb(command)
            },
        }
    }
    
    /// Send a command using TIS interface
    fn send_command_tis(&self, command: &[u8]) -> Result<Vec<u8>, TpmError> {
        // Check if TPM is ready to receive command
        let status = self.read_tis_reg8(tis::STS)?;
        trace!("TIS STS register before send: 0x{:02X}", status);
        
        if (status & tis::STS_COMMAND_READY) == 0 {
            error!("TPM not ready to receive command, STS: 0x{:02X}", status);
            // TPM not ready, abort
            return Err(TpmError::CommunicationFailure);
        }
        
        debug!("Sending {} bytes to TPM FIFO", command.len());
        // Send the command data byte by byte to FIFO
        for (i, &byte) in command.iter().enumerate() {
            trace!("Writing byte {} to FIFO: 0x{:02X}", i, byte);
            self.write_tis_reg8(tis::DATA_FIFO, byte)?;
        }
        
        debug!("Command sent, issuing GO to execute");
        // Signal to TPM that command is complete and can be executed
        self.write_tis_reg8(tis::STS, tis::STS_GO)?;
        
        // Wait for data to become available
        debug!("Waiting for TPM response");
        let mut timeout = 2000; // Longer timeout for command execution
        while timeout > 0 {
            let status = self.read_tis_reg8(tis::STS)?;
            trace!("TIS STS register while waiting: 0x{:02X}", status);
            
            if (status & tis::STS_DATA_AVAILABLE) != 0 {
                debug!("TPM response data available");
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for TPM response");
                return Err(TpmError::Timeout);
            }
            delay_sec(2);
        }
        
        debug!("Reading TPM response header");
        // Read response header to determine size
        let mut response_header = [0u8; 10]; // TPM response header: tag(2) + size(4) + code(4)
        for i in 0..response_header.len() {
            response_header[i] = self.read_tis_reg8(tis::DATA_FIFO)?;
            trace!("Read header byte {}: 0x{:02X}", i, response_header[i]);
        }
        
        // Parse response size from header (bytes 2-5, big-endian)
        let response_size = u32::from_be_bytes([
            response_header[2], response_header[3], 
            response_header[4], response_header[5]
        ]) as usize;
        
        debug!("TPM response size: {} bytes", response_size);
        
        // Validate response size
        if !(10..=4096).contains(&response_size) {
            error!("Invalid TPM response size: {}", response_size);
            return Err(TpmError::CommunicationFailure);
        }
        
        // Create buffer for complete response
        let mut response = Vec::with_capacity(response_size);
        
        // Add header to response
        for &byte in &response_header {
            response.push(byte);
        }
        
        debug!("Reading remaining {} bytes of TPM response", response_size - 10);
        // Read the rest of the response
        for i in 10..response_size {
            let byte = self.read_tis_reg8(tis::DATA_FIFO)?;
            trace!("Read response byte {}: 0x{:02X}", i, byte);
            response.push(byte);
        }
        
        // Check response code
        let rc = u32::from_be_bytes([
            response_header[6], response_header[7], 
            response_header[8], response_header[9]
        ]);
        
        if rc != 0 {
            warn!("TPM returned error code: 0x{:08X}", rc);
            return Err(TpmError::TpmResponseError(rc));
        }
        
        debug!("TPM response successfully received, signaling command ready");
        // Signal to TPM that we're done reading
        self.write_tis_reg8(tis::STS, tis::STS_COMMAND_READY)?;
        
        debug!("Command completed successfully, response size: {}", response.len());
        Ok(response)
    }
    
    /// Send a command using MMIO interface
    fn send_command_mmio(&self, command: &[u8]) -> Result<Vec<u8>, TpmError> {
        debug!("MMIO command: using TIS implementation");
        // For simplicity, reuse TIS implementation as it's similar
        self.send_command_tis(command)
    }
    
    /// Send a command using CRB interface
    fn send_command_crb(&self, command: &[u8]) -> Result<Vec<u8>, TpmError> {
        // Ensure TPM is in ready state
        let status = self.read_crb_reg32(crb::CONTROL_AREA_STATUS)?;
        trace!("CRB status before command: 0x{:08X}", status);
        
        if (status & crb::CRB_STATUS_READY) == 0 {
            error!("TPM CRB not in ready state, status: 0x{:08X}", status);
            return Err(TpmError::CommunicationFailure);
        }
        
        debug!("Writing {} bytes to CRB command buffer", command.len());
        // Write command to command buffer
        for (i, &byte) in command.iter().enumerate() {
            trace!("Writing byte {} to command buffer: 0x{:02X}", i, byte);
            self.write_mmio_u8(crb::COMMAND_BUFFER + i, byte)?;
        }
        
        debug!("Starting CRB command execution");
        // Start command execution
        self.write_crb_reg32(crb::CONTROL_START, 1)?;
        
        // Wait for command completion
        debug!("Waiting for CRB command completion");
        let mut timeout = 2000; // Longer timeout for command execution
        while timeout > 0 {
            let status = self.read_crb_reg32(crb::CONTROL_AREA_STATUS)?;
            trace!("CRB status while waiting: 0x{:08X}", status);
            
            if (status & crb::CRB_STATUS_IDLE) != 0 {
                debug!("TPM CRB command completed");
                break;
            }
            timeout -= 1;
            if timeout == 0 {
                error!("Timeout waiting for CRB command completion");
                return Err(TpmError::Timeout);
            }
            delay_sec(2);
        }
        
        debug!("Reading TPM CRB response header");
        // Read response header to determine size
        let mut response_header = [0u8; 10]; // TPM response header: tag(2) + size(4) + code(4)
        for i in 0..response_header.len() {
            response_header[i] = self.read_mmio_u8(crb::RESPONSE_BUFFER + i)?;
            trace!("Read header byte {}: 0x{:02X}", i, response_header[i]);
        }
        
        // Parse response size from header (bytes 2-5, big-endian)
        let response_size = u32::from_be_bytes([
            response_header[2], response_header[3], 
            response_header[4], response_header[5]
        ]) as usize;
        
        debug!("TPM CRB response size: {} bytes", response_size);
        
        // Validate response size
        if !(10..=4096).contains(&response_size) {
            error!("Invalid TPM CRB response size: {}", response_size);
            return Err(TpmError::CommunicationFailure);
        }
        
        // Create buffer for complete response
        let mut response = Vec::with_capacity(response_size);
        
        // Add header to response
        for &byte in &response_header {
            response.push(byte);
        }
        
        debug!("Reading remaining {} bytes of TPM CRB response", response_size - 10);
        // Read the rest of the response
        for i in 10..response_size {
            let byte = self.read_mmio_u8(crb::RESPONSE_BUFFER + i)?;
            trace!("Read response byte {}: 0x{:02X}", i, byte);
            response.push(byte);
        }
        
        // Check response code
        let rc = u32::from_be_bytes([
            response_header[6], response_header[7], 
            response_header[8], response_header[9]
        ]);
        
        if rc != 0 {
            warn!("TPM returned error code: 0x{:08X}", rc);
            return Err(TpmError::TpmResponseError(rc));
        }
        
        debug!("Returning TPM CRB to ready state");
        // Return to ready state
        self.write_crb_reg32(crb::CONTROL_AREA_REQUEST, crb::CRB_STATUS_READY)?;
        
        debug!("CRB command completed successfully, response size: {}", response.len());
        Ok(response)
    }
    
    /// Execute a TPM command and parse the response
    pub fn execute_command(&self, command_code: TpmCommandCode, params: &[u8]) -> Result<Vec<u8>, TpmError> {
        info!("Executing TPM command: {}", command_code);
        debug!("Command parameters size: {} bytes", params.len());
        
        // Build the command buffer
        let mut cmd_buffer = TpmBuffer::new();
        
        // TPM command header: tag(2) + size(4) + command_code(4)
        let tag: u16 = 0x8001; // TPM_ST_NO_SESSIONS
        let cmd_size: u32 = 10 + params.len() as u32; // header + params
        
        debug!("Building command with tag 0x{:04X}, size {} bytes", tag, cmd_size);
        
        // Write tag (big-endian)
        cmd_buffer.write(&tag.to_be_bytes())?;
        
        // Write command size (big-endian)
        cmd_buffer.write(&cmd_size.to_be_bytes())?;
        
        // Write command code (big-endian)
        let code: u32 = command_code as u32;
        debug!("Command code: 0x{:08X}", code);
        cmd_buffer.write(&code.to_be_bytes())?;
        
        // Write command parameters
        cmd_buffer.write(params)?;
        
        debug!("Command buffer built, total size: {} bytes", cmd_buffer.len());
        trace!("Command buffer: {:02X?}", cmd_buffer.as_slice());
        
        // Send the command to TPM
        let result = self.send_command(cmd_buffer.as_slice());
        
        match &result {
            Ok(response) => {
                debug!("Command {} completed successfully, response size: {} bytes", 
                    command_code, response.len());
                trace!("Response header: {:02X?}", &response[0..10]);
            },
            Err(err) => {
                warn!("Command {} failed: {}", command_code, err);
            }
        }
        
        result
    }
    
    /// Read random bytes from the TPM
    pub fn get_random(&self, num_bytes: u16) -> Result<Vec<u8>, TpmError> {
        info!("Getting {} random bytes from TPM", num_bytes);
        
        // TPM2_GetRandom command structure
        // After the header:
        // bytes requested (2 bytes)
        
        // Prepare parameter buffer
        let params = num_bytes.to_be_bytes();
        debug!("GetRandom parameter: 0x{:04X}", num_bytes);
        
        // Execute the command
        let response = self.execute_command(TpmCommandCode::GetRandom, &params)?;
        
        // Parse response - skip header (10 bytes) and size field (2 bytes)
        if response.len() < 13 {
            error!("Invalid GetRandom response length: {}", response.len());
            return Err(TpmError::CommunicationFailure);
        }
        
        // Extract random bytes from response (skipping header and size)
        let random_size = u16::from_be_bytes([response[10], response[11]]) as usize;
        
        debug!("Received {} random bytes from TPM", random_size);
        
        let mut random_bytes = Vec::with_capacity(random_size);
        
        for i in 0..random_size {
            if 12 + i >= response.len() {
                warn!("Random data truncated, expected {} bytes, got {}", 
                    random_size, response.len() - 12);
                break;
            }
            random_bytes.push(response[12 + i]);
        }
        
        info!("Successfully retrieved {} random bytes", random_bytes.len());
        trace!("Random bytes: {:02X?}", random_bytes.as_slice());
        
        Ok(random_bytes)
    }
    
    /// Extend a PCR with provided data
    pub fn pcr_extend(&self, pcr_index: u32, digest: &[u8]) -> Result<(), TpmError> {
        info!("Extending PCR {} with {} bytes of digest data", pcr_index, digest.len());
        
        // TPM2_PCR_Extend command requires authorization, which complicates the example
        // This is a simplified version without proper authorization
        
        if digest.len() != 32 {
            error!("Invalid digest length for PCR extend: {} (expected 32 for SHA-256)", digest.len());
            return Err(TpmError::BadParameter); // Assuming SHA-256 (32 bytes)
        }
        
        // Prepare parameter buffer for PCR extend
        let mut params = Vec::new();
        
        debug!("Building PCR_Extend parameters");
        
        // PCR index (4 bytes)
        params.extend_from_slice(&pcr_index.to_be_bytes());
        debug!("Added PCR index: {}", pcr_index);
        
        // Authorization section placeholder (simplified)
        // In a real implementation, this would include proper authorization
        let auth_size: u32 = 9; // Minimal auth size
        params.extend_from_slice(&auth_size.to_be_bytes());
        debug!("Added auth section size: {}", auth_size);
        
        params.push(0); // TPM_RS_PW
        params.extend_from_slice(&[0, 0]); // nonce size = 0
        params.push(0); // session attributes
        params.extend_from_slice(&[0, 0]); // auth size = 0
        debug!("Added simplified auth section");
        
        // Count of hashes (1 byte)
        params.push(1);
        debug!("Added hash count: 1");
        
        // Hash algorithm (2 bytes) - SHA-256 = 0x000B
        params.extend_from_slice(&[0x00, 0x0B]);
        debug!("Added hash algorithm: SHA-256 (0x000B)");
        
        // Hash data
        params.extend_from_slice(digest);
        
        debug!("Added digest data, total parameter size: {} bytes", params.len());
        trace!("PCR_Extend parameters: {:02X?}", params.as_slice());
        
        // Execute command
        let _response = self.execute_command(TpmCommandCode::PCRExtend, &params)?;
        
        info!("PCR {} successfully extended", pcr_index);
        Ok(())
    }
    
    /// Read PCR value
    pub fn pcr_read(&self, pcr_index: u32) -> Result<Vec<u8>, TpmError> {
        info!("Reading PCR {} value", pcr_index);
        
        // Prepare parameter buffer for PCR read
        let mut params = Vec::new();
        
        debug!("Building PCR_Read parameters");
        
        // Count of PCRs to read (4 bytes) - just 1
        params.extend_from_slice(&1u32.to_be_bytes());
        debug!("Added PCR count: 1");
        
        // PCR index (4 bytes)
        params.extend_from_slice(&pcr_index.to_be_bytes());
        debug!("Added PCR index: {}", pcr_index);
        
        // Hash algorithm (2 bytes) - SHA-256 = 0x000B
        params.extend_from_slice(&[0x00, 0x0B]);
        debug!("Added hash algorithm: SHA-256 (0x000B)");
        debug!("PCR_Read parameters built, total size: {} bytes", params.len());
        trace!("PCR_Read parameters: {:02X?}", params.as_slice());
        
        // Execute command
        let response = self.execute_command(TpmCommandCode::PCRRead, &params)?;
        
        debug!("PCR_Read response received, size: {} bytes", response.len());
        trace!("PCR_Read response header: {:02X?}", &response[0..14]);
        
        // Parse response to extract PCR value
        // For simplicity, assuming SHA-256 and skipping detailed parsing
        if response.len() < 14 {
            error!("Invalid PCR_Read response length: {}", response.len());
            return Err(TpmError::CommunicationFailure);
        }
        
        // Extract PCR value (assuming SHA-256 of 32 bytes)
        let mut pcr_value = Vec::with_capacity(32);
        for i in 0..32 {
            if 14 + i >= response.len() {
                warn!("PCR value truncated, expected 32 bytes, got {}", response.len() - 14);
                break;
            }
            pcr_value.push(response[14 + i]);
        }
        
        info!("Successfully read PCR {} value, {} bytes", pcr_index, pcr_value.len());
        trace!("PCR value: {:02X?}", pcr_value.as_slice());
        
        Ok(pcr_value)
    }
    
    /// Read TIS register (8-bit)
    fn read_tis_reg8(&self, reg_offset: usize) -> Result<u8, TpmError> {
        let addr = self.address.base_address + reg_offset;
        trace!("Reading TIS register at offset 0x{:X} (address 0x{:X})", reg_offset, addr);
        
        // Use unsafe for direct hardware access
        let value = unsafe {
            read_volatile(addr as *const u8)
        };
        
        trace!("TIS register 0x{:X} value: 0x{:02X}", reg_offset, value);
        Ok(value)
    }
    
    /// Write TIS register (8-bit)
    fn write_tis_reg8(&self, reg_offset: usize, value: u8) -> Result<(), TpmError> {
        let addr = self.address.base_address + reg_offset;
        trace!("Writing TIS register at offset 0x{:X} (address 0x{:X}): 0x{:02X}", 
            reg_offset, addr, value);
            
        // Use unsafe for direct hardware access
        unsafe {
            write_volatile(addr as *mut u8, value);
        }
        Ok(())
    }
    
    /// Read CRB register (32-bit)
    fn read_crb_reg32(&self, reg_offset: usize) -> Result<u32, TpmError> {
        let addr = self.address.base_address + reg_offset;
        trace!("Reading CRB register at offset 0x{:X} (address 0x{:X})", reg_offset, addr);
        
        // Use unsafe for direct hardware access
        let value = unsafe {
            read_volatile(addr as *const u32)
        };
        
        trace!("CRB register 0x{:X} value: 0x{:08X}", reg_offset, value);
        Ok(value)
    }
    
    /// Write CRB register (32-bit)
    fn write_crb_reg32(&self, reg_offset: usize, value: u32) -> Result<(), TpmError> {
        let addr = self.address.base_address + reg_offset;
        trace!("Writing CRB register at offset 0x{:X} (address 0x{:X}): 0x{:08X}", 
            reg_offset, addr, value);
            
        // Use unsafe for direct hardware access
        unsafe {
            write_volatile(addr as *mut u32, value);
        }
        Ok(())
    }
    
    /// Read byte from MMIO
    fn read_mmio_u8(&self, offset: usize) -> Result<u8, TpmError> {
        let addr = self.address.base_address + offset;
        trace!("Reading MMIO byte at offset 0x{:X} (address 0x{:X})", offset, addr);
        
        // Use unsafe for direct hardware access
        let value = unsafe {
            read_volatile(addr as *const u8)
        };
        
        trace!("MMIO byte 0x{:X} value: 0x{:02X}", offset, value);
        Ok(value)
    }
    
    /// Write byte to MMIO
    fn write_mmio_u8(&self, offset: usize, value: u8) -> Result<(), TpmError> {
        let addr = self.address.base_address + offset;
        trace!("Writing MMIO byte at offset 0x{:X} (address 0x{:X}): 0x{:02X}", 
            offset, addr, value);
            
        // Use unsafe for direct hardware access
        unsafe {
            write_volatile(addr as *mut u8, value);
        }
        Ok(())
    }
}

/// Example usage of the TPM driver
pub fn tpm_driver_example() -> Result<(), TpmError> {
    info!("Starting TPM driver example");
    
    // Create a new TPM driver with TIS interface
    // Base address would typically be determined from hardware detection
    info!("Creating TPM driver with TIS interface at base address 0xFED40000");
    let tpm = TpmDriver::new(TpmInterfaceType::Tis, 0xFED40000);
    
    // Initialize the TPM
    info!("Initializing TPM driver");
    tpm.initialize()?;
    
    // Get 16 random bytes from TPM
    info!("Requesting 16 random bytes from TPM");
    let random_bytes = tpm.get_random(16)?;
    info!("Received {} random bytes from TPM", random_bytes.len());
    debug!("Random bytes: {:02X?}", random_bytes.as_slice());
    
    // Read PCR 0
    info!("Reading PCR 0");
    let pcr_value = tpm.pcr_read(0)?;
    info!("PCR 0 value retrieved, {} bytes", pcr_value.len());
    debug!("PCR 0 value: {:02X?}", pcr_value.as_slice());
    
    info!("TPM driver example completed successfully");
    Ok(())
}