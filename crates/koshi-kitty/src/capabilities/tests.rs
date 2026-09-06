//! Tests for Kitty capability queries and reply metadata.

use super::*;

#[test]
fn support_query_writes_the_exact_non_storing_request() {
    let mut output = Vec::new();

    write_kitty_support_query(&mut output).expect("the query writes");

    assert_eq!(
        output,
        b"\x1b_Gi=4294967295,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
    );
    assert_eq!(KITTY_QUERY_IMAGE_ID, u32::MAX);
}
