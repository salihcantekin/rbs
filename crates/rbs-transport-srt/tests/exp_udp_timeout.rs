//! TEMPORARY experiment (not for merge): does a UDP receive with a read timeout lose
//! datagrams on Windows when one arrives as the timeout expires?

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn run(nonblocking: bool, count: u32) -> u32 {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    if nonblocking {
        server.set_nonblocking(true).unwrap();
    } else {
        server.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    }
    let addr = server.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let t = std::thread::spawn(move || {
        let mut buf = [0_u8; 64];
        let mut got = 0_u32;
        let mut idle_after_stop = 0;
        loop {
            match server.recv_from(&mut buf) {
                Ok(_) => got += 1,
                Err(_) => {
                    if s2.load(Ordering::SeqCst) {
                        idle_after_stop += 1;
                        if idle_after_stop > 200 {
                            break;
                        }
                    }
                    if nonblocking {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
            }
        }
        got
    });
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect(addr).unwrap();
    let mut x = 0x1234_5678_u32;
    for i in 0..count {
        client.send(&i.to_be_bytes()).unwrap();
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let wait = Duration::from_micros(u64::from(x % 2500));
        let until = Instant::now() + wait;
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    }
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::SeqCst);
    let got = t.join().unwrap();
    count - got
}

#[test]
#[ignore = "experiment"]
fn experiment() {
    let n = 4000;
    let lost_timeout = run(false, n);
    let lost_nonblocking = run(true, n);
    println!("RESULT timeout-recv lost {lost_timeout} of {n}; nonblocking lost {lost_nonblocking} of {n}");
}
