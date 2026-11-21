#![allow(unsafe_code)]

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::windows::io::AsRawSocket;
use std::time::Duration;
use windows_sys::Win32::Networking::WinSock::{
    WSAIoctl, SIO_TCP_INITIAL_RTO, TCP_INITIAL_RTO_DEFAULT_RTT,
    TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS, TCP_INITIAL_RTO_PARAMETERS,
};

/// Connect to a TCP loopback address, with the given deadline. A closed local
/// port need not spend the system TCP SYN retry budget before reporting refusal.
/// Windows versions that do not support the option use the normal TCP behavior.
pub fn connect_loopback(address: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "address is not loopback",
        ));
    }
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    let parameters = TCP_INITIAL_RTO_PARAMETERS {
        Rtt: TCP_INITIAL_RTO_DEFAULT_RTT as u16,
        MaxSynRetransmissions: TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS as u8,
    };
    let mut returned = 0;
    // SAFETY: the socket is owned and live. WSAIoctl reads a correctly sized
    // parameter block synchronously, and writes only the returned byte count.
    // If unsupported, this optional per-socket setting leaves TCP unchanged.
    unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_TCP_INITIAL_RTO,
            (&raw const parameters).cast(),
            std::mem::size_of_val(&parameters) as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        );
    }
    socket.connect_timeout(&address.into(), timeout)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_closed_loopback_port_refuses_without_waiting_for_syn_retries() {
        let closed = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        // Reserve the port without listen, so a concurrent fixture cannot
        // accidentally supply a listener at the address being checked.
        closed
            .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
            .unwrap();
        let address = closed.local_addr().unwrap().as_socket().unwrap();
        let started = Instant::now();
        let result = connect_loopback(address, Duration::from_millis(500));
        let elapsed = started.elapsed();
        assert!(
            result.as_ref().is_err_and(|err| err.kind() == io::ErrorKind::ConnectionRefused)
                && elapsed < Duration::from_millis(250),
            "closed loopback {address}: expected ConnectionRefused within 250 ms; got {result:?} in {elapsed:?}"
        );
    }

    #[test]
    fn the_loopback_connect_helper_refuses_other_addresses() {
        let address = "192.0.2.1:80".parse().unwrap();
        assert_eq!(
            connect_loopback(address, Duration::from_millis(500))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
