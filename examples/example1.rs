use cancellable_io::*;

use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let (listener, listener_canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        println!("Server: ready");
        let (mut socket, socket_canceller, addr) = listener.accept().unwrap();
        println!("Server: got connection from {}", addr);

        let connection = thread::spawn(move || {
            println!("Connection: ready");
            let mut buf = [0; 16];
            let len = socket.read(&mut buf).unwrap();
            println!("Connection: read {}", String::from_utf8_lossy(&buf[..len]));
            println!("Connection: try reading more.");
            match socket.read(&mut buf) {
                Ok(0) => println!("Connection: socket closed."),
                Err(ref e) if is_cancelled(e) => println!("Connection: cancelled."),
                ref e => panic!("Connection: unexpected {:?}", e),
            }
        });

        println!("Server: try accepting more.");
        if is_cancelled(&listener.accept().unwrap_err()) {
            println!("Server: accept cancelled.");
        }

        socket_canceller.cancel().unwrap();
        connection.join().unwrap();
    });

    thread::sleep(Duration::from_secs(2));
    let (mut socket, socket_canceller) = TcpStream::connect(&address).unwrap();
    let client = thread::spawn(move || {
        println!("Client: ready");
        thread::sleep(Duration::from_secs(2));
        println!("Client: write data.");
        socket.write(b"Hello!").unwrap();
        println!("Client: try reading.");
        let mut buf = [0; 16];
        match socket.read(&mut buf) {
            Ok(0) => println!("Client: socket closed."),
            Err(ref e) if is_cancelled(e) => println!("Client: cancelled."),
            ref e => panic!("Client: unexpected {:?}", e),
        }
    });

    thread::sleep(Duration::from_secs(4));
    socket_canceller.cancel().unwrap();
    thread::sleep(Duration::from_secs(2));
    listener_canceller.cancel().unwrap();

    server.join().unwrap();
    client.join().unwrap();
}
