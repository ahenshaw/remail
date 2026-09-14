//! The library as something other than the window uses it.
//!
//! These reach `remail` from outside the crate, which is the whole point of
//! there being a library: before the split there was only a binary, and the
//! mail engine could not be driven — or tested — without one.

use remail::mail::{Envelope, Query, Store, parse};

#[test]
fn a_query_compiles_to_an_imap_search() {
    let query = Query::parse("subject:pickleball and from:dupr -is:read").expect("parses");
    let criteria = query.to_imap().expect("compiles");

    assert!(criteria.contains(r#"SUBJECT "pickleball""#), "{criteria}");
    assert!(criteria.contains(r#"FROM "dupr""#), "{criteria}");
    // `-is:read` negates the SEEN flag; `is:unread` is the other spelling
    // and compiles to UNSEEN.
    assert!(criteria.contains("NOT SEEN"), "{criteria}");
}

#[test]
fn a_query_filters_cached_envelopes_without_a_server() {
    let query = Query::parse("from:dupr").expect("parses");

    let from_dupr = Envelope {
        subject: "Welcome to DUPR".into(),
        from: vec![remail::mail::Addr { name: "DUPR".into(), email: "noreply@mydupr.com".into() }],
        ..Default::default()
    };
    let from_anyone_else = Envelope {
        subject: "Welcome to DUPR".into(),
        from: vec![remail::mail::Addr {
            name: "Walter".into(),
            email: "walter@example.com".into(),
        }],
        ..Default::default()
    };

    assert!(query.matches(&from_dupr));
    assert!(!query.matches(&from_anyone_else), "the subject answered for the sender");
}

#[test]
fn a_message_can_be_parsed_into_a_body() {
    let raw = b"From: Lance <lance@example.net>\r\n\
                Subject: Re: The Harbor Point Interactive Map\r\n\
                MIME-Version: 1.0\r\n\
                Content-Type: multipart/mixed; boundary=\"b\"\r\n\
                \r\n\
                --b\r\n\
                Content-Type: text/plain; charset=utf-8\r\n\
                \r\n\
                The map is up to date now.\r\n\
                --b\r\n\
                Content-Type: application/octet-stream\r\n\
                Content-ID: <tracking>\r\n\
                Content-Disposition: attachment; filename=\"tracking.xlsx\"\r\n\
                \r\n\
                payload\r\n\
                --b--\r\n";

    let envelope = parse::parse_envelope(41349, raw);
    assert_eq!(envelope.subject, "Re: The Harbor Point Interactive Map");
    assert_eq!(envelope.from[0].email, "lance@example.net");

    let body = parse::parse_body(raw);
    assert!(body.text.unwrap().contains("up to date"));
    assert_eq!(body.attachments.len(), 1, "the attachment was filed as a document resource");
    assert_eq!(body.attachments[0].filename, "tracking.xlsx");
}

#[test]
fn the_cache_round_trips_an_envelope() {
    let store = Store::open_memory().expect("in-memory store");
    let envelope = Envelope { uid: 41349, subject: "Re: The map".into(), ..Default::default() };

    store.save_envelopes(1, "INBOX", std::slice::from_ref(&envelope)).expect("saved");
    let loaded = store.load_envelopes(1, "INBOX", 10).expect("loaded");

    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].subject, "Re: The map");
    assert_eq!(loaded[0].mailbox, "INBOX", "the mailbox is filled in on the way out");
}
