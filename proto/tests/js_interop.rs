//! A duress message built by the web app (webapp/src/core.js, nostr-tools) must be readable by
//! the Rust watcher. Regenerate the fixture with `node webapp/test/interop.mjs fixture`.
//! The keys in the fixture are throwaway test keys.

use nostr::prelude::*;
use warn_proto::{unwrap, Context, Message};

#[test]
fn web_app_duress_unwraps_in_rust() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/js_duress.json")).expect("fixture json");
    let watcher = Keys::parse(fixture["watcher_secret_hex"].as_str().unwrap()).unwrap();
    let device = PublicKey::parse(fixture["device_npub"].as_str().unwrap()).unwrap();
    let wrap = Event::from_json(fixture["wrap"].to_string()).expect("wrap event");
    wrap.verify().expect("valid wrap signature");

    let got = unwrap(&watcher, &wrap).expect("well-formed").expect("addressed to this watcher");
    assert_eq!(got.sender, device);
    let expected: Context = serde_json::from_value(fixture["context"].clone()).unwrap();
    assert_eq!(got.envelope.msg, Message::Duress { context: expected });
}
