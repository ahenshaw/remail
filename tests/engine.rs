//! Driving the mail engine without a window.
//!
//! What a command-line tool would do: send a command, wait for the answer,
//! act on it. None of this reaches the network — every message here is
//! already in the cache, which is where most questions can be answered from.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use remail::config::Config;
use remail::mail::{Command, Engine, Event, Store};

const RAW: &[u8] = b"From: Lance <lance@example.net>\r\n\
                     Subject: Re: The Harbor Point Interactive Map\r\n\
                     \r\n\
                     The map is up to date now.\r\n";

fn engine(store: Store) -> Engine {
    Engine::start(Arc::new(RwLock::new(Config::default())), Arc::new(store), || {}).expect("engine")
}

#[test]
fn a_cached_body_is_waited_for_and_returned() {
    let store = Store::open_memory().unwrap();
    store.save_raw(1, "INBOX", 41349, RAW).unwrap();
    let mut engine = engine(store);

    let body = engine
        .send_and_wait(
            Command::FetchBody { account: 1, mailbox: "INBOX".into(), uid: 41349, served: false },
            Duration::from_secs(5),
            |event| match event {
                Event::Body { uid: 41349, body, .. } => Some(body.clone()),
                _ => None,
            },
        )
        .expect("the body was never answered");

    assert!(body.text.as_deref().unwrap_or_default().contains("up to date"));
}

/// Events that are not the answer are passed over, not mistaken for it. A
/// body fetch also announces a refreshed preview, which arrives first.
#[test]
fn events_before_the_answer_do_not_end_the_wait() {
    let store = Store::open_memory().unwrap();
    store.save_raw(1, "INBOX", 41349, RAW).unwrap();
    let mut engine = engine(store);

    let subject = engine
        .send_and_wait(
            Command::FetchBody { account: 1, mailbox: "INBOX".into(), uid: 41349, served: false },
            Duration::from_secs(5),
            |event| match event {
                Event::Body { body, .. } => Some(
                    body.headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("subject"))
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default(),
                ),
                _ => None,
            },
        )
        .expect("the body was never answered");

    assert!(subject.contains("Harbor Point"), "{subject}");
}

/// Nothing to answer with, and no account to go and ask: the wait ends
/// rather than hanging on a message that is never coming.
#[test]
fn a_wait_with_no_answer_gives_up() {
    let mut engine = engine(Store::open_memory().unwrap());

    let started = Instant::now();
    let outcome = engine.wait_for(Duration::from_millis(200), |event| match event {
        Event::Sent => Some(()),
        _ => None,
    });

    assert!(outcome.is_err(), "a wait for something nobody sends came back with an answer");
    assert!(started.elapsed() < Duration::from_secs(2), "it waited well past its timeout");
}

/// An uncached body needs the server, and there is no account configured to
/// reach one. The failure is reported rather than waited out.
#[test]
fn an_error_ends_the_wait_instead_of_running_out_the_clock() {
    let mut engine = engine(Store::open_memory().unwrap());

    let started = Instant::now();
    let outcome = engine.send_and_wait(
        Command::FetchBody { account: 1, mailbox: "INBOX".into(), uid: 999, served: false },
        Duration::from_secs(30),
        |event| match event {
            Event::Body { uid: 999, .. } => Some(()),
            _ => None,
        },
    );

    let reported = outcome.expect_err("a body arrived for a message that is nowhere").to_string();
    // The engine's own complaint, not this helper's. Passing the error over
    // would have ended the same way thirty seconds later, and the difference
    // between the two is the whole point of the error being terminal.
    assert!(!reported.contains("within"), "the wait timed out rather than reporting: {reported}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the error was passed over and the wait ran to its timeout"
    );
}
