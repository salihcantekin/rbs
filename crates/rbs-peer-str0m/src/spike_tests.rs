//! Proves the provider: two str0m instances connect over an in-memory network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

fn rtc(now: Instant) -> Rtc {
    crate::payload::rtc_config()
        .set_crypto_provider(crate::crypto::provider())
        .build(now)
}

#[test]
fn two_instances_connect_on_the_pure_rust_provider() {
    let start = Instant::now();
    let a_addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let b_addr: SocketAddr = "10.0.0.2:6000".parse().unwrap();
    let mut a = rtc(start);
    let mut b = rtc(start);
    a.add_local_candidate(Candidate::host(a_addr, "udp").unwrap());
    b.add_local_candidate(Candidate::host(b_addr, "udp").unwrap());
    let mut change = a.sdp_api();
    change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
    change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
    let (offer, pending) = change.apply().unwrap();
    let offer_text = offer.to_sdp_string();
    let offer = SdpOffer::from_sdp_string(&offer_text).unwrap();
    let answer = b.sdp_api().accept_offer(offer).unwrap();
    let answer_text = answer.to_sdp_string();
    a.sdp_api()
        .accept_answer(pending, SdpAnswer::from_sdp_string(&answer_text).unwrap())
        .unwrap();
    let code_len = |sdp: &str| {
        let env = rbs_peer::Envelope::new(
            rbs_peer::CodeKind::Invite,
            rbs_peer::TransportKind::WebRtc,
            rbs_peer::SessionId::new([1; 16]).unwrap(),
            0,
            crate::payload::encode(sdp),
        );
        env.encode().map(|c| c.expose().len())
    };
    // A trimmed SDP with one candidate makes a code well under the 4 KiB limit, with
    // room for the 7 more candidates a code may carry.
    assert!(
        code_len(&offer_text).unwrap() < 2000,
        "{:?}",
        code_len(&offer_text)
    );
    assert!(code_len(&answer_text).unwrap() < 2000);

    let mut now = start;
    let (mut a_up, mut b_up) = (false, false);
    for _ in 0..2000 {
        a_up |= pump(&mut a, &mut b, a_addr, b_addr, now);
        b_up |= pump(&mut b, &mut a, b_addr, a_addr, now);
        if a_up && b_up {
            break;
        }
        now += Duration::from_millis(10);
        a.handle_input(Input::Timeout(now)).unwrap();
        b.handle_input(Input::Timeout(now)).unwrap();
    }
    assert!(a_up && b_up, "DTLS did not connect");
}

fn pump(me: &mut Rtc, other: &mut Rtc, from: SocketAddr, to: SocketAddr, now: Instant) -> bool {
    let mut up = false;
    loop {
        match me.poll_output().unwrap() {
            Output::Timeout(_) => return up,
            Output::Transmit(t) => {
                assert_eq!(t.source, from);
                let rx = Receive::new(Protocol::Udp, from, to, &t.contents).unwrap();
                other.handle_input(Input::Receive(now, rx)).unwrap();
            }
            Output::Event(Event::Connected) => up = true,
            Output::Event(_) => {}
        }
    }
}
