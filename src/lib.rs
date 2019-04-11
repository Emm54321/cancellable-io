#![deny(missing_docs)]

//! A crate implementing cancellable synchronous network I/O.
//!
//! This crate exposes structs [TcpStream](struct.TcpStream.html),
//! [TcpListener](struct.TcpListener.html) and [UdpSocket](struct.UdpSocket.html)
//! that are similar to their std::net variants, except that I/O operations
//! can be cancelled through [Canceller](struct.Canceller.html) objects
//! created with them.
//!
//! Most methods work as they do in the std::net implementations, and you
//! should refer to the [original documentation](https://doc.rust-lang.org/std/net/)
//! for details and examples.
//!
//! Main differences with the original std::net implementations :
//! * Methods that return a [TcpStream](struct.TcpStream.html), a
//! [TcpListener](struct.TcpListener.html), or an [UdpSocket](struct.UdpSocket.html)
//! also return a [Canceller](struct.Canceller.html) object.
//! * There are no peek() methods (yet?)
//! * [TcpListener](struct.TcpListener.html) and [UdpSocket](struct.UdpSocket.html)
//! are not `Sync` (yet?)
//!
//! # Example
//! ```
//! use cancellable_io::*;
//! let (listener, canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
//! let handle = std::thread::spawn(move || {
//!     println!("Waiting for connections.");
//!     let r = listener.accept();
//!     assert!(is_cancelled(&r.unwrap_err()));
//!     println!("Server cancelled.");
//! });
//!
//! std::thread::sleep(std::time::Duration::from_secs(2));
//! canceller.cancel().unwrap();
//! handle.join().unwrap();
//! ```

extern crate mio;

use std::cell::RefCell;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::io;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use mio::*;

const STOP_TOKEN: Token = Token(0);
const OBJECT_TOKEN: Token = Token(1);

/// An object that can be used to cancel an i/o operation.
///
/// It is created with the object it can cancel.
/// When an operation is cancelled, it returns an error that can be
/// identified with the [is_cancelled](fn.is_cancelled.html) function.
#[derive(Clone, Debug)]
pub struct Canceller {
    set_readiness: SetReadiness,
}

/// A TCP stream between a local and a remote socket.
///
/// The socket will be closed when the value is dropped.
///
/// The [read](struct.TcpStream.html#method.read) and
/// [write](struct.TcpStream.html#method.write) methods can be interrupted
/// using the [Canceller](struct.Canceller.html) object created with the
/// stream.
///
/// Otherwise similar to
/// [std::net::TcpStream](https://doc.rust-lang.org/std/net/struct.TcpStream.html).
pub struct TcpStream {
    stream: mio::net::TcpStream,
    poll: Poll,
    _stop_registration: Registration,
    events: Events,
    options: Arc<RwLock<TcpStreamOptions>>,
}

struct TcpStreamOptions {
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    nonblocking: bool,
}

/// A TCP socket server, listening for connections.
///
/// The socket will be closed when the value is dropped.
///
/// The [accept](struct.TcpListener.html#method.accept) and the
/// [incoming](struct.TcpListener.html#method.incoming) methods can be
/// interrupted using the [Canceller](struct.Canceller.html) object
/// created with the socket.
///
/// Otherwise similar to
/// [std::net::TcpListener](https://doc.rust-lang.org/std/net/struct.TcpListener.html).
pub struct TcpListener {
    listener: mio::net::TcpListener,
    poll: Poll,
    _stop_registration: Registration,

    // RefCell makes TcpListener !Sync while std::net::TcpListener is, but
    // using a Mutex would conflict with nonblocking mode, and allocating
    // events on each call to accept() is not that nice. So let's keep it
    // this way for now. One can always use try_clone() anyway if sharing
    // a socket between threads is needed.
    events: RefCell<Events>,
    options: Arc<RwLock<TcpListenerOptions>>,
}

struct TcpListenerOptions {
    timeout: Option<Duration>,
    nonblocking: bool,
}

/// An iterator that infinitely [accept](struct.TcpListener.html#method.accept)s
/// connections on a TcpListener.
///
/// It is created by the [incoming](struct.TcpListener.html#method.incoming) method.
pub struct Incoming<'a> {
    listener: &'a TcpListener,
}

/// An UDP socket bound to an address.
///
/// The [recv_from](struct.UdpSocket.html#method.recv_from),
/// [send_to](struct.UdpSocket.html#method.send_to),
/// [recv](struct.UdpSocket.html#method.recv), and
/// [send](struct.UdpSocket.html#method.send) can be interrupted using the
/// [Canceller](struct.Canceller.html) object created with the socket.
///
/// Otherwise similar to
/// [std::net::UdpSocket](https://doc.rust-lang.org/std/net/struct.UdpSocket.html).
pub struct UdpSocket {
    socket: mio::net::UdpSocket,
    poll: Poll,
    _stop_registration: Registration,
    events: RefCell<Events>, // !Sync, cf TcpListener.
    options: Arc<RwLock<UdpSocketOptions>>,
}

struct UdpSocketOptions {
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    nonblocking: bool,
}

impl Canceller {
    /// Cancels an operation on the associated object.
    ///
    /// The pending operation is aborted and returns an error that can be
    /// checked by the [is_cancelled](fn.is_cancelled.html) function. If
    /// there is no pending operation, the next one will be cancelled.
    pub fn cancel(&self) -> io::Result<()> {
        self.set_readiness.set_readiness(Ready::readable())
    }
}

fn cancelled_error() -> io::Error {
    // Can't use ErrorKind::Interrupted because it causes silent retries
    // from various library functions.
    io::Error::new(io::ErrorKind::Other, "cancelled")
}

/// Checks if the error returned by a method was caused by the operation
/// being cancelled.
pub fn is_cancelled(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string() == "cancelled"
}

impl TcpStream {
    fn simple_connect(address: &SocketAddr) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let stream = mio::net::TcpStream::connect(address)?;
        poll.register(
            &stream,
            OBJECT_TOKEN,
            Ready::readable() | Ready::writable(),
            PollOpt::level(),
        )?;

        let events = Events::with_capacity(4);

        Ok((
            TcpStream {
                stream,
                poll,
                _stop_registration: stop_registration,
                events,
                options: Arc::new(RwLock::new(TcpStreamOptions {
                    read_timeout: None,
                    write_timeout: None,
                    nonblocking: false,
                })),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Creates a new TCP stream with an object used to cancel read/write
    /// operations, and connects it to a remote address.
    pub fn connect<A: ToSocketAddrs>(address: A) -> io::Result<(Self, Canceller)> {
        let mut error = io::Error::from(io::ErrorKind::InvalidInput);
        for a in address.to_socket_addrs()? {
            match Self::simple_connect(&a) {
                Ok(r) => return Ok(r),
                Err(e) => error = e,
            }
        }
        Err(error)
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream.local_addr()
    }

    /// Shuts down the read, write, or both halves of this connection.
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.stream.shutdown(how)
    }

    /// Creates a new independently owned handle to the underlying socket.
    /// The [Canceller](struct.Canceller.html)s associated with the original
    /// object and its clone are also independent.
    pub fn try_clone(&self) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let stream = self.stream.try_clone()?;
        poll.register(
            &stream,
            OBJECT_TOKEN,
            Ready::readable() | Ready::writable(),
            PollOpt::level(),
        )?;

        let events = Events::with_capacity(4);

        Ok((
            TcpStream {
                stream,
                poll,
                _stop_registration: stop_registration,
                events,
                options: self.options.clone(),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Sets the read timeout.
    pub fn set_read_timeout(&self, duration: Option<Duration>) -> io::Result<()> {
        self.options.write().unwrap().read_timeout = duration;
        Ok(())
    }

    /// Sets the write timeout.
    pub fn set_write_timeout(&self, duration: Option<Duration>) -> io::Result<()> {
        self.options.write().unwrap().write_timeout = duration;
        Ok(())
    }

    /// Gets the read timeout.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.options.read().unwrap().read_timeout)
    }

    /// Gets the write timeout.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.options.read().unwrap().write_timeout)
    }

    //pub fn peek(&self)

    /// Sets the value of the `TCP_NODELAY` option on this socket.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.stream.set_nodelay(nodelay)
    }

    /// Gets the value of the `TCP_NODELAY` option for this socket.
    pub fn nodelay(&self) -> io::Result<bool> {
        self.stream.nodelay()
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.stream.set_ttl(ttl)
    }

    /// Gets the value for the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.stream.ttl()
    }

    /// Gets the value of the `SO_ERROR` option for this socket.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.stream.take_error()
    }

    /// Moves this TCP stream into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.options.write().unwrap().nonblocking = nonblocking;
        Ok(())
    }
}

impl Read for TcpStream {
    /// Reads data from the socket. This operation can be cancelled
    /// by the associated [Canceller](struct.Canceller.html) object.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut self.events, Some(Duration::from_millis(0)))?;
            for event in self.events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_readable() {
                        return self.stream.read(buf);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let read_timeout = self.options.read().unwrap().read_timeout;
            loop {
                self.poll.poll(&mut self.events, read_timeout)?;
                for event in self.events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_readable() {
                            return self.stream.read(buf);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }
}

impl Write for TcpStream {
    /// Writes data to the socket. This operation can be cancelled
    /// by the associated [Canceller](struct.Canceller.html) object.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut self.events, Some(Duration::from_millis(0)))?;
            for event in self.events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_writable() {
                        return self.stream.write(buf);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let write_timeout = self.options.read().unwrap().write_timeout;
            loop {
                self.poll.poll(&mut self.events, write_timeout)?;
                for event in self.events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_writable() {
                            return self.stream.write(buf);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            self.poll.poll(&mut self.events, None)?;
            for event in self.events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_writable() {
                        return self.stream.flush();
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
        }
    }
}

impl Debug for TcpStream {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.stream.fmt(f)
    }
}

impl TcpListener {
    fn simple_bind(address: &SocketAddr) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let listener = mio::net::TcpListener::bind(address)?;
        poll.register(&listener, OBJECT_TOKEN, Ready::readable(), PollOpt::level())?;

        let events = Events::with_capacity(4);

        Ok((
            TcpListener {
                listener,
                poll,
                _stop_registration: stop_registration,
                events: RefCell::new(events),
                options: Arc::new(RwLock::new(TcpListenerOptions {
                    timeout: None,
                    nonblocking: false,
                })),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Creates a new [TcpListener](struct.TcpListener.html) which will be
    /// bound to the specified address, together with an object that allows
    /// cancelling [accept](struct.TcpListener.html#method.accept) operations.
    pub fn bind<A: ToSocketAddrs>(address: A) -> io::Result<(Self, Canceller)> {
        let mut error = io::Error::from(io::ErrorKind::InvalidInput);
        for a in address.to_socket_addrs()? {
            match Self::simple_bind(&a) {
                Ok(r) => return Ok(r),
                Err(e) => error = e,
            }
        }
        Err(error)
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Creates a new independently owned handle to the underlying socket.
    pub fn try_clone(&self) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let listener = self.listener.try_clone()?;
        poll.register(&listener, OBJECT_TOKEN, Ready::readable(), PollOpt::level())?;

        let events = Events::with_capacity(4);

        Ok((
            TcpListener {
                listener,
                poll,
                _stop_registration: stop_registration,
                events: RefCell::new(events),
                options: self.options.clone(),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Accepts a new incoming connection from this listener.
    ///
    /// This method can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object.
    ///
    /// This method returns a [TcpStream](struct.TcpStream.html) with an
    /// associated [Canceller](struct.Canceller.html) and the address of the
    /// remote peer.
    pub fn accept(&self) -> io::Result<(TcpStream, Canceller, SocketAddr)> {
        let mut events = self.events.borrow_mut();
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut events, Some(Duration::from_millis(0)))?;
            for event in events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    let (stream, addr) = self.listener.accept()?;
                    let poll = Poll::new()?;

                    let stop_token = Token(0);
                    let (stop_registration, stop_set_readiness) = Registration::new2();
                    poll.register(
                        &stop_registration,
                        stop_token,
                        Ready::readable(),
                        PollOpt::edge(),
                    )?;

                    let stream_token = Token(1);
                    poll.register(
                        &stream,
                        stream_token,
                        Ready::readable() | Ready::writable(),
                        PollOpt::level(),
                    )?;

                    let events = Events::with_capacity(4);

                    return Ok((
                        TcpStream {
                            stream,
                            poll,
                            _stop_registration: stop_registration,
                            events,
                            options: Arc::new(RwLock::new(TcpStreamOptions {
                                read_timeout: None,
                                write_timeout: None,
                                nonblocking: false,
                            })),
                        },
                        Canceller {
                            set_readiness: stop_set_readiness,
                        },
                        addr,
                    ));
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let timeout = self.options.read().unwrap().timeout;
            loop {
                self.poll.poll(&mut events, timeout)?;
                for event in events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        let (stream, addr) = self.listener.accept()?;
                        let poll = Poll::new()?;

                        let stop_token = Token(0);
                        let (stop_registration, stop_set_readiness) = Registration::new2();
                        poll.register(
                            &stop_registration,
                            stop_token,
                            Ready::readable(),
                            PollOpt::edge(),
                        )?;

                        let stream_token = Token(1);
                        poll.register(
                            &stream,
                            stream_token,
                            Ready::readable() | Ready::writable(),
                            PollOpt::level(),
                        )?;

                        let events = Events::with_capacity(4);

                        return Ok((
                            TcpStream {
                                stream,
                                poll,
                                _stop_registration: stop_registration,
                                events,
                                options: Arc::new(RwLock::new(TcpStreamOptions {
                                    read_timeout: None,
                                    write_timeout: None,
                                    nonblocking: false,
                                })),
                            },
                            Canceller {
                                set_readiness: stop_set_readiness,
                            },
                            addr,
                        ));
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }

    /// Returns an iterator over the connections being received by this listener.
    ///
    /// The iteration can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object. If the iteration is cancelled, the next() method will return an
    /// error that can be identified with [is_cancelled](fn.is_cancelled.html).
    pub fn incoming(&self) -> Incoming {
        Incoming { listener: self }
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.listener.set_ttl(ttl)
    }

    /// Gets the value for the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.listener.ttl()
    }

    /// Gets the value for the `SO_ERROR` option for this socket.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.listener.take_error()
    }

    /// Moves this TCP connection into or out of non blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.options.write().unwrap().nonblocking = nonblocking;
        Ok(())
    }
}

impl Debug for TcpListener {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.listener.fmt(f)
    }
}

impl<'a> Iterator for Incoming<'a> {
    type Item = io::Result<(TcpStream, Canceller, SocketAddr)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.listener.accept())
    }
}

impl UdpSocket {
    fn simple_bind(address: &SocketAddr) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let socket = mio::net::UdpSocket::bind(address)?;
        poll.register(
            &socket,
            OBJECT_TOKEN,
            Ready::readable() | Ready::writable(),
            PollOpt::level(),
        )?;

        let events = Events::with_capacity(4);

        Ok((
            UdpSocket {
                socket,
                poll,
                _stop_registration: stop_registration,
                events: RefCell::new(events),
                options: Arc::new(RwLock::new(UdpSocketOptions {
                    read_timeout: None,
                    write_timeout: None,
                    nonblocking: false,
                })),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Creates an UDP socket from the given address, together with an
    /// object that can be used to cancel send/recv operations.
    pub fn bind<A: ToSocketAddrs>(address: A) -> io::Result<(Self, Canceller)> {
        let mut error = io::Error::from(io::ErrorKind::InvalidInput);
        for a in address.to_socket_addrs()? {
            match Self::simple_bind(&a) {
                Ok(r) => return Ok(r),
                Err(e) => error = e,
            }
        }
        Err(error)
    }

    /// Receives a single datagram message from the socket.
    ///
    /// This method can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut events = self.events.borrow_mut();
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut events, Some(Duration::from_millis(0)))?;
            for event in events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_readable() {
                        return self.socket.recv_from(buf);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let read_timeout = self.options.read().unwrap().read_timeout;
            loop {
                self.poll.poll(&mut events, read_timeout)?;
                for event in events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_readable() {
                            return self.socket.recv_from(buf);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }

    /// Sends a single datagram message to the given address.
    ///
    /// This method can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object.
    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        let mut events = self.events.borrow_mut();
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut events, Some(Duration::from_millis(0)))?;
            for event in events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_writable() {
                        return self.socket.send_to(buf, addr);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let write_timeout = self.options.read().unwrap().write_timeout;
            loop {
                self.poll.poll(&mut events, write_timeout)?;
                for event in events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_writable() {
                            return self.socket.send_to(buf, addr);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }

    /// Gets the socket address that this object was created from.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Creates a new independently owned handle to the underlying socket.
    pub fn try_clone(&self) -> io::Result<(Self, Canceller)> {
        let poll = Poll::new()?;

        let (stop_registration, stop_set_readiness) = Registration::new2();
        poll.register(
            &stop_registration,
            STOP_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;

        let socket = self.socket.try_clone()?;
        poll.register(
            &socket,
            OBJECT_TOKEN,
            Ready::readable() | Ready::writable(),
            PollOpt::level(),
        )?;

        let events = Events::with_capacity(4);

        Ok((
            UdpSocket {
                socket,
                poll,
                _stop_registration: stop_registration,
                events: RefCell::new(events),
                options: self.options.clone(),
            },
            Canceller {
                set_readiness: stop_set_readiness,
            },
        ))
    }

    /// Sets the read timeout to the timeout specified.
    pub fn set_read_timeout(&self, duration: Option<Duration>) -> io::Result<()> {
        self.options.write().unwrap().read_timeout = duration;
        Ok(())
    }

    /// Sets the write timeout to the timeout specified.
    pub fn set_write_timeout(&self, duration: Option<Duration>) -> io::Result<()> {
        self.options.write().unwrap().write_timeout = duration;
        Ok(())
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.options.read().unwrap().read_timeout)
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.options.read().unwrap().write_timeout)
    }

    /// Sets the value of the `SO_BROADCAST` option for this socket.
    pub fn set_broadcast(&self, broadcast: bool) -> io::Result<()> {
        self.socket.set_broadcast(broadcast)
    }

    /// Gets the value of the `SO_BROADCAST` option for this socket.
    pub fn broadcast(&self) -> io::Result<bool> {
        self.socket.broadcast()
    }

    /// Sets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v4(&self, multicast_loop: bool) -> io::Result<()> {
        self.socket.set_multicast_loop_v4(multicast_loop)
    }

    /// Gets the value of the `IP_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        self.socket.multicast_loop_v4()
    }

    /// Sets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn set_multicast_ttl_v4(&self, multicast_ttl: u32) -> io::Result<()> {
        self.socket.set_multicast_ttl_v4(multicast_ttl)
    }

    /// Gets the value of the `IP_MULTICAST_TTL` option for this socket.
    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        self.socket.multicast_ttl_v4()
    }

    /// Sets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn set_multicast_loop_v6(&self, multicast_loop: bool) -> io::Result<()> {
        self.socket.set_multicast_loop_v6(multicast_loop)
    }

    /// Gets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        self.socket.multicast_loop_v6()
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.socket.set_ttl(ttl)
    }

    /// Gets the value for the `IP_TTL` option for this socket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.socket.ttl()
    }

    /// Executes an operation of the `IP_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        self.socket.join_multicast_v4(multiaddr, interface)
    }

    /// Executes an operation of the `IPV6_ADD_MEMBERSHIP` type.
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        self.socket.join_multicast_v6(multiaddr, interface)
    }

    /// Executes an operation of the `IP_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> io::Result<()> {
        self.socket.leave_multicast_v4(multiaddr, interface)
    }

    /// Executes an operation of the `IPV6_DROP_MEMBERSHIP` type.
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> io::Result<()> {
        self.socket.leave_multicast_v6(multiaddr, interface)
    }

    /// Gets the value of the `SO_ERROR` option for this socket.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.socket.take_error()
    }

    /// Sets the value for the `IPV6_V6ONLY` option on this socket.
    pub fn set_only_v6(&self, only_v6: bool) -> io::Result<()> {
        self.socket.set_only_v6(only_v6)
    }

    /// Gets the value for the `IPV6_V6ONLY` option for this socket.
    pub fn only_v6(&self) -> io::Result<bool> {
        self.socket.only_v6()
    }

    fn simple_connect(&self, address: SocketAddr) -> io::Result<()> {
        self.socket.connect(address)
    }

    /// Connects this socket to a remote address.
    pub fn connect<A: ToSocketAddrs>(&self, address: A) -> io::Result<()> {
        let mut error = io::Error::from(io::ErrorKind::InvalidInput);
        for a in address.to_socket_addrs()? {
            match self.simple_connect(a) {
                Ok(r) => return Ok(r),
                Err(e) => error = e,
            }
        }
        Err(error)
    }

    /// Moves this socket into or out of non blocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.options.write().unwrap().nonblocking = nonblocking;
        Ok(())
    }

    /// Receives a single datagram from the connected remote address.
    ///
    /// This method can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut events = self.events.borrow_mut();
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut events, Some(Duration::from_millis(0)))?;
            for event in events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_readable() {
                        return self.socket.recv(buf);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let read_timeout = self.options.read().unwrap().read_timeout;
            loop {
                self.poll.poll(&mut events, read_timeout)?;
                for event in events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_readable() {
                            return self.socket.recv(buf);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }

    /// Sends a single datagram to the connected remote address.
    ///
    /// This method can be cancelled by the associated [Canceller](struct.Canceller.html)
    /// object.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let mut events = self.events.borrow_mut();
        if self.options.read().unwrap().nonblocking {
            self.poll
                .poll(&mut events, Some(Duration::from_millis(0)))?;
            for event in events.iter() {
                let t = event.token();
                if t == OBJECT_TOKEN {
                    if event.readiness().is_writable() {
                        return self.socket.send(buf);
                    }
                } else if t == STOP_TOKEN {
                    return Err(cancelled_error());
                }
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            let write_timeout = self.options.read().unwrap().write_timeout;
            loop {
                self.poll.poll(&mut events, write_timeout)?;
                for event in events.iter() {
                    let t = event.token();
                    if t == OBJECT_TOKEN {
                        if event.readiness().is_writable() {
                            return self.socket.send(buf);
                        }
                    } else if t == STOP_TOKEN {
                        return Err(cancelled_error());
                    }
                }
            }
        }
    }
}

impl Debug for UdpSocket {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.socket.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn test_is_cancelled() {
        assert_eq!(
            is_cancelled(&io::Error::new(io::ErrorKind::Interrupted, "")),
            false
        );
        assert_eq!(
            is_cancelled(&io::Error::new(io::ErrorKind::Other, "")),
            false
        );
        assert_eq!(is_cancelled(&cancelled_error()), true);
    }

    #[test]
    fn test_simple_connection() {
        let (listener, listener_canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for r in listener.incoming() {
                match r {
                    Ok((mut stream, _canceller, _addr)) => {
                        thread::spawn(move || {
                            let mut buf = [0; 3];
                            stream.read_exact(&mut buf).unwrap();
                            assert_eq!(&buf, b"foo");
                            stream.write_all(b"bar").unwrap();
                        });
                    }
                    Err(ref e) if is_cancelled(e) => break,
                    Err(ref e) => panic!("{:?}", e),
                }
            }
        });

        for _ in 0..3 {
            let (mut stream, _stream_canceller) = TcpStream::connect(&addr).unwrap();
            stream.write_all(b"foo").unwrap();
            stream.flush().unwrap();
            let mut buf = Vec::new();
            assert_eq!(stream.read_to_end(&mut buf).unwrap(), 3);
            assert_eq!(&buf[..], b"bar");
        }

        listener_canceller.cancel().unwrap();

        handle.join().unwrap();
    }

    #[test]
    fn test_cancel_stream() {
        let (listener, _listener_canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _canceller, _addr) = listener.accept().unwrap();
            let mut buf = [0; 16];
            assert_eq!(stream.read(&mut buf).unwrap(), 0);
        });

        let (mut stream, stream_canceller) = TcpStream::connect(&addr).unwrap();
        let client = thread::spawn(move || {
            let mut buf = [0; 16];
            assert!(is_cancelled(&stream.read(&mut buf).unwrap_err()));
        });

        thread::sleep(Duration::from_secs(1));
        stream_canceller.cancel().unwrap();

        client.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn test_non_blocking() {
        let barrier = Arc::new(Barrier::new(3));
        let barrier_server = barrier.clone();
        let (listener, _listener_canceller) = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _canceller, _addr) = listener.accept().unwrap();
            // Wait for the client to attempt reading when no data is there.
            barrier_server.wait();
            stream.write(b"foo").unwrap();
            // Wake up the client now that there is data.
            barrier_server.wait();
            barrier_server.wait();
            barrier_server.wait();
            let mut buf = [0; 16];
            assert_eq!(stream.read(&mut buf).unwrap(), 0);
        });

        let barrier_client = barrier.clone();
        let (mut stream, stream_canceller) = TcpStream::connect(&addr).unwrap();
        stream.set_nonblocking(true).unwrap();
        let client = thread::spawn(move || {
            let mut buf = [0; 3];
            // Attempt reading while there is no data available.
            assert_eq!(
                stream.read(&mut buf).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            // Tell the server it can put some data now.
            barrier_client.wait();
            // Wait until the server has put some data.
            barrier_client.wait();
            // Some data is available now.
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"foo");
            // Tell the main thread it can cancel the stream now.
            barrier_client.wait();
            // Wait for the stream to be cancelled.
            barrier_client.wait();
            // It is cancelled now.
            assert!(is_cancelled(&stream.read(&mut buf).unwrap_err()));
        });

        // Wait until the client has attempted is first read.
        barrier.wait();
        // Wait until the server has written some data.
        barrier.wait();
        // Wait until the client has done is second read.
        barrier.wait();
        stream_canceller.cancel().unwrap();
        // Tell the client it can attempt reading again.
        barrier.wait();

        client.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn test_udp() {
        let barrier = Arc::new(Barrier::new(3));
        let (socket1, canceller1) = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (socket2, canceller2) = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address1 = socket1.local_addr().unwrap();
        let address2 = socket2.local_addr().unwrap();
        let barrier1 = barrier.clone();
        let barrier2 = barrier.clone();
        let thread1 = thread::spawn(move || {
            let mut buf = [0; 16];
            assert_eq!(socket1.recv_from(&mut buf).unwrap(), (3, address2));
            assert_eq!(socket1.send_to(b"bar", &address2).unwrap(), 3);
            barrier1.wait();
            assert!(is_cancelled(&socket1.recv_from(&mut buf).unwrap_err()));
        });
        let thread2 = thread::spawn(move || {
            assert_eq!(socket2.send_to(b"foo", &address1).unwrap(), 3);
            let mut buf = [0; 16];
            assert_eq!(socket2.recv_from(&mut buf).unwrap(), (3, address1));
            barrier2.wait();
            assert!(is_cancelled(&socket2.recv_from(&mut buf).unwrap_err()));
        });
        barrier.wait();
        canceller1.cancel().unwrap();
        canceller2.cancel().unwrap();
        thread1.join().unwrap();
        thread2.join().unwrap();
    }

    /* TcpListener is !Sync.
    #[test]
    fn test_sync() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap().0);
        let listener2 = listener.clone();
        let t = thread::spawn(move|| {
            listener.accept().unwrap();
        });
        listener2.accept().unwrap();
        t.join().unwrap();
    }*/
}
