//! Test-only connecting process for the accepted-socket caller fixture.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

fn main() -> std::io::Result<()> {
    let address = std::env::args().nth(1).expect("listener address");
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    // Keep the connecting socket owned by this process until the parent has
    // finished its lookup, rather than exiting as soon as connect returns.
    stream.write_all(&[1])?;
    let mut release = [0];
    stream.read_exact(&mut release)?;
    Ok(())
}
