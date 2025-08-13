# StringBuf

A `no_std` compatible crate providing a 4K length-prefixed string buffer for storing logs.

## Features

- **No Standard Library**: Works in `no_std` environments
- **Fixed Size**: Uses exactly 4KB of storage
- **Length-Prefixed**: Each string is stored with a u16 length prefix in little-endian format
- **UTF-8 Support**: All strings are validated as UTF-8
- **Reading and Writing**: Can both append new strings and read from existing buffers
- **Capacity Tracking**: Tracks remaining capacity and detects when buffer is full

## Usage

```rust
use stringbuf::{StringBuffer, BUFFER_SIZE};

// Create a 4K storage buffer
let mut storage = [0u8; BUFFER_SIZE];

// Create a new string buffer
let mut buffer = StringBuffer::new(&mut storage);

// Append strings
buffer.append("Hello, World!")?;
buffer.append("Another log message")?;

// Iterate over stored strings
for result in buffer.iter() {
    match result {
        Ok(s) => println!("Log: {}", s),
        Err(e) => println!("Error: {}", e),
    }
}

// Check capacity
println!("Used: {} bytes", buffer.used_capacity());
println!("Remaining: {} bytes", buffer.remaining_capacity());

// Load from existing buffer
let buffer2 = StringBuffer::from_existing(&mut storage)?;
```

## Buffer Format

The buffer stores strings sequentially with the following format:

```
[u16 length][UTF-8 string data][u16 length][UTF-8 string data]...
```

- Length is stored in little-endian format
- Maximum string length is 65,535 bytes (u16::MAX)
- Buffer terminates when a zero length is encountered or end of buffer is reached

## Error Handling

The crate provides `StringBufferError` enum for error handling:

- `StringTooLong`: String exceeds u16::MAX length
- `BufferFull`: No space remaining in buffer
- `InvalidFormat`: Malformed data in buffer
- `InvalidUtf8`: Non-UTF-8 data encountered

## Thread Safety

The crate is not inherently thread-safe. If you need to share a `StringBuffer` between threads, you must provide your own synchronization.

## Examples

See `examples/basic_usage.rs` for a complete usage example.
