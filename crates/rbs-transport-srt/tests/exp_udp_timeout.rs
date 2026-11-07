//! TEMPORARY experiment (not for merge): is the last datagram lost on Windows when
//! the sending UDP socket is closed right after the send?

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

fn run(unbuffered: bool, linger: bool, iterations: u32) -> u32 {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    let addr = server.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let got = Arc::new(AtomicU32::new(0));
    let (s2, g2) = (stop.clone(), got.clone());
    let t = std::thread::spawn(move || {
        let mut buf = [0_u8; 64];
        let mut idle = 0;
        loop {
            match server.recv_from(&mut buf) {
                Ok((n, _)) => {
                    if n == 16 && buf[0] == 0xEE {
                        g2.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Err(_) => {
                    if s2.load(Ordering::SeqCst) {
                        idle += 1;
                        if idle > 500 {
                            break;
                        }
                    }
                }
            }
        }
    });
    for _ in 0..iterations {
        let local: SocketAddr = (Ipv4Addr::UNSPECIFIED, 0).into();
        let c = UdpSocket::bind(local).unwrap();
        c.connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        for _ in 0..4 {
            c.send(&[0x11; 1316]).unwrap();
        }
        if unbuffered {
            #[cfg(windows)]
            {
                let _ = socket2::SockRef::from(&c).set_send_buffer_size(0);
            }
        }
        c.send(&[0xEE; 16]).unwrap();
        if linger {
            let mut b = [0_u8; 8];
            let _ = c.recv(&mut b);
        }
        drop(c);
        // Pace the iterations (after the close) so the listener's buffer never fills.
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);
    t.join().unwrap();
    iterations - got.load(Ordering::SeqCst)
}

#[test]
#[ignore = "experiment"]
fn experiment() {
    let burn = Arc::new(AtomicBool::new(true));
    let burners: Vec<_> = (0..6)
        .map(|_| {
            let b = burn.clone();
            std::thread::spawn(move || {
                let mut x = 1_u64;
                while b.load(Ordering::Relaxed) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    let n = 1500;
    let plain = run(false, false, n);
    let unbuffered = run(true, false, n);
    let linger = run(false, true, n);
    burn.store(false, Ordering::Relaxed);
    for b in burners {
        b.join().unwrap();
    }
    println!("RESULT last datagram lost: plain {plain}, sndbuf0 {unbuffered}, 1ms linger {linger} (of {n} each)");
}
