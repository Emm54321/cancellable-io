# cancellable-io

A crate implementing cancellable synchronous network I/O.

This crate exposes structs [TcpStream](struct.TcpStream.html),
[TcpListener](struct.TcpListener.html) and [UdpSocket](struct.UdpSocket.html)
that are similar to their std::net variants, except that I/O operations
can be cancelled through [Canceller](struct.Canceller.html) objects
created with them.

Most methods work as they do in the std::net implementations, and you
should refer to the [original documentation](https://doc.rust-lang.org/std/net/)
for details and examples.

Main differences with the original std::net implementations :
* Methods that return a [TcpStream](struct.TcpStream.html), a
[TcpListener](struct.TcpListener.html), or an [UdpSocket](struct.UdpSocket.html)
also return a [Canceller](struct.Canceller.html) object.
* There are no peek() methods (yet?)
* [TcpListener](struct.TcpListener.html) and [UdpSocket](struct.UdpSocket.html)
are not `Sync` (yet?)

## Example
```rust
use cancellable_io::*;
let (listener, canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
let handle = std::thread::spawn(move || {
    println!("Waiting for connections.");
    let r = listener.accept();
    assert!(is_cancelled(&r.unwrap_err()));
    println!("Server cancelled.");
});

std::thread::sleep(std::time::Duration::from_secs(2));
canceller.cancel().unwrap();
handle.join().unwrap();
```

License: MIT/Apache-2.0
