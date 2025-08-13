// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Example usage of the stringbuf crate.

use stringbuf::{StringBuffer, BUFFER_SIZE};

fn main() {
    // Create a 4K storage buffer
    let mut storage = [0u8; BUFFER_SIZE];
    
    // Create a new string buffer
    let mut buffer = StringBuffer::new(&mut storage);
    
    // Append some log messages
    let _ = buffer.append("System starting up...");
    let _ = buffer.append("Loading configuration...");
    let _ = buffer.append("Ready to serve requests");
    
    println!("Buffer usage: {} / {} bytes", buffer.used_capacity(), BUFFER_SIZE);
    println!("Remaining capacity: {} bytes", buffer.remaining_capacity());
    
    // Iterate over all stored strings
    println!("\nStored log messages:");
    for (i, result) in buffer.iter().enumerate() {
        match result {
            Ok(s) => println!("  {}: {}", i + 1, s),
            Err(e) => println!("  Error reading string {}: {}", i + 1, e),
        }
    }
    
    // Demonstrate loading from existing buffer
    println!("\n--- Demonstrating loading from existing buffer ---");
    
    // Create a new buffer reference to the same storage
    let buffer2 = StringBuffer::from_existing(&mut storage).unwrap();
    
    println!("Loaded buffer usage: {} / {} bytes", buffer2.used_capacity(), BUFFER_SIZE);
    println!("Loaded messages:");
    for (i, result) in buffer2.iter().enumerate() {
        match result {
            Ok(s) => println!("  {}: {}", i + 1, s),
            Err(e) => println!("  Error reading string {}: {}", i + 1, e),
        }
    }
}
