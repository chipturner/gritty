//! The docs that restate a constant must agree with the code. CLAUDE.md asks
//! for doc updates in the same commit as the code; these tests are the part of
//! that rule a reviewer does not have to remember.

use gritty::protocol::PROTOCOL_VERSION;

const WIRE_PROTOCOL_MD: &str = include_str!("../docs/wire-protocol.md");
const CLAUDE_MD: &str = include_str!("../CLAUDE.md");

#[test]
fn wire_protocol_doc_states_the_current_protocol_version() {
    let expected = format!("`PROTOCOL_VERSION: u16` is currently **{PROTOCOL_VERSION}**");
    assert!(
        WIRE_PROTOCOL_MD.contains(&expected),
        "docs/wire-protocol.md does not say {expected:?} -- bump the doc with the constant"
    );
}

#[test]
fn claude_md_states_the_current_protocol_version() {
    let expected = format!("(currently v{PROTOCOL_VERSION})");
    assert!(
        CLAUDE_MD.contains(&expected),
        "CLAUDE.md does not say {expected:?} -- bump the doc with the constant"
    );
}
