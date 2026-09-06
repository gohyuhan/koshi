//! Protocol-driven checks for Kitty upload and placement lifetimes.

use super::*;

use crate::engine::TerminalEngine;
use koshi_core::process::PtySize;

const UPLOAD: &[u8] = b"\x1b_Ga=T,f=32,s=1,v=1,c=2,r=2,C=1,i=7,q=2;/wAA/w==\x1b\\";

fn engine() -> TerminalEngine {
    TerminalEngine::new(PtySize { cols: 20, rows: 10 })
}

#[test]
fn a_failed_numbered_upload_does_not_reply_with_an_older_image_id() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,I=9,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,U=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=0,I=9;ENOTSUP:unsupported image placement\x1b\\"
    );
    assert_eq!(
        engine
            .state()
            .kitty_images
            .iter()
            .map(|image| image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(1)]
    );
}

#[test]
fn a_continuation_error_uses_the_first_chunks_reply_identity() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=9,f=32,s=1,v=1,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Gm=0;!\x1b\\"),
        b"\x1b_Gi=9;EINVAL:invalid image data\x1b\\"
    );
    assert_eq!(engine.state().kitty_images, []);
}

#[test]
fn a_delete_range_frees_uploads_without_placements() {
    let mut engine = engine();
    for id in 1..=3 {
        assert_eq!(
            engine.advance(format!("\x1b_Ga=t,i={id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\").as_bytes()),
            b""
        );
    }
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=R,x=1,y=2\x1b\\"), b"");
    assert_eq!(
        engine
            .state()
            .kitty_images
            .iter()
            .map(|image| image.display.image_id)
            .collect::<Vec<_>>(),
        [Some(3)]
    );
}

#[test]
fn restoring_old_placements_rebuilds_the_numbered_upload_store() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,I=9,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    let mut value = serde_json::to_value(engine.state()).expect("serialize");
    value
        .as_object_mut()
        .expect("object")
        .remove("kitty_images");
    let state: TerminalState = serde_json::from_value(value).expect("restore old state");
    let mut engine = TerminalEngine::from_state(state, &[]);
    assert_eq!(
        engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,I=9,c=2,r=3,C=1\x1b\\"),
        b"\x1b_Gi=1,I=9;OK\x1b\\"
    );
    let placement = &engine.state().image_placements()[0];
    assert_eq!(
        (placement.anchor(), placement.columns, placement.rows),
        ((0, 0), 2, 3)
    );
}

fn anchors(engine: &TerminalEngine) -> Vec<(u16, u16)> {
    engine
        .state()
        .image_placements()
        .iter()
        .map(|p| p.anchor())
        .collect()
}

#[test]
fn a_relative_placement_uses_its_parent_and_keeps_the_cursor_in_place() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,H=2,V=1,c=1,r=1,C=0,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (1, 2)]);
    assert_eq!(engine.state().active_cursor_position(), (0, 0));
}

#[test]
fn a_relative_placement_without_a_parent_is_rejected_without_state_change() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,q=0\x1b\\"),
        b"\x1b_Gi=8,p=2;EINVAL:invalid image placement\x1b\\"
    );
    assert_eq!(anchors(&engine), []);
}

#[test]
fn deleting_a_relative_parent_also_deletes_its_children() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=8,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=8;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=8,p=2,P=7,Q=1,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (0, 0)]);
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=i,i=7,p=1,q=2\x1b\\"), b"");
    assert!(anchors(&engine).is_empty());
}

#[test]
fn a_unicode_placeholder_is_rendered_from_the_matching_cell() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=t,i=7,f=32,s=1,v=1;/wAA/w==\x1b\\"),
        b"\x1b_Gi=7;OK\x1b\\"
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,p=3,c=2,r=2,U=1,C=1,q=2\x1b\\"),
        b""
    );
    assert!(engine.state().image_placements().is_empty());
    assert_eq!(
        engine.advance("\x1b[38;5;7m\u{10eeee}\u{0305}\u{0305}".as_bytes()),
        b""
    );
    let placements = engine.state().image_placements_for_view(0);
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].anchor(), (0, 0));
    assert_eq!(placements[0].dimensions(), (2, 2));
}

#[test]
fn kitty_animation_frame_control_changes_the_displayed_frame() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,c=2,s=3,q=2\x1b\\"), b"");
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [0, 0, 255, 255]
    );
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=f,i=7,r=2,q=2\x1b\\"), b"");
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn loading_animation_state_survives_a_state_restore() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,i=7,f=32,s=1,v=1,c=1,r=1,C=1,q=2;/wAA/w==\x1b\\"),
        b""
    );
    assert_eq!(
        engine.advance(b"\x1b_Ga=f,i=7,f=32,s=1,v=1,z=10,q=2;AAD//w==\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=a,i=7,s=2,q=2\x1b\\"), b"");

    let value = serde_json::to_value(engine.state()).expect("serialize loading animation");
    let restored: TerminalState = serde_json::from_value(value).expect("restore loading animation");

    assert_eq!(
        restored.image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn redraw_removes_the_old_position_and_displays_the_stored_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(b"\x1b[3;1H"), b"");
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(anchors(&engine), [(2, 0)]);
    assert_eq!(
        engine.advance(b"\x1b_Ga=d,d=a,q=2\x1b\\\x1b[6;1H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(5, 0)]);
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
    assert_eq!(engine.state().active_cursor_position(), (5, 0));
}

#[test]
fn placing_one_upload_twice_shares_pixels_and_keeps_both_positions() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(
        engine.advance(b"\x1b[5;4H\x1b_Ga=p,i=7,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(0, 0), (4, 3)]);
    let placements = engine.state().image_placements();
    assert!(Arc::ptr_eq(
        &placements[0].record().image,
        &placements[1].record().image
    ));
    assert_eq!(
        placements[0].content_id(),
        placements[1].content_id(),
        "one Kitty upload has one canonical content identity"
    );
}

#[test]
fn retransmitting_a_kitty_id_gets_a_new_content_identity() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    let first = engine.state().image_placements()[0].content_id();

    assert_eq!(engine.advance(UPLOAD), b"");
    let placements = engine.state().image_placements();
    assert_eq!(placements.len(), 1);
    assert_ne!(first, placements[0].content_id());
}

#[test]
fn a_named_placement_moves_without_adding_another_placement() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(
        engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    let id = engine.state().image_placements()[0].id();
    assert_eq!(
        engine.advance(b"\x1b[7;2H\x1b_Ga=p,i=7,p=3,c=1,r=1,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&engine), [(6, 1)]);
    assert_eq!(engine.state().image_placements()[0].id(), id);
}

#[test]
fn freeing_an_upload_makes_a_new_placement_report_the_missing_image() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7\x1b\\"), b"");
    assert_eq!(anchors(&engine), []);
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=7,c=1,r=1,C=1\x1b\\"),
        b"\x1b_Gi=7;ENOENT:image not found\x1b\\"
    );
    assert_eq!(anchors(&engine), []);
}

#[test]
fn stored_pixels_survive_a_state_restore_with_no_visible_placement() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\"), b"");
    let state: TerminalState =
        serde_json::from_slice(&serde_json::to_vec(engine.state()).expect("serialize"))
            .expect("restore");
    let mut restored = TerminalEngine::from_state(state, &[]);
    assert_eq!(
        restored.advance(b"\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\"),
        b""
    );
    assert_eq!(anchors(&restored), [(3, 2)]);
    assert_eq!(
        restored.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn query_replies_before_device_attributes_and_does_not_store_pixels() {
    let mut engine = engine();
    let reply = engine.advance(b"\x1b_Ga=q,i=31,f=32,s=1,v=1;/wAA/w==\x1b\\\x1b[c");
    assert_eq!(reply, b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c");
    assert_eq!(engine.state().kitty_images, []);
    assert_eq!(anchors(&engine), []);
    assert_eq!(engine.take_graphics(), []);
    assert_eq!(
        engine.advance(b"\x1b_Ga=p,i=31,c=1,r=1\x1b\\"),
        b"\x1b_Gi=31;ENOENT:image not found\x1b\\"
    );
}

#[test]
fn every_byte_split_keeps_the_same_redraw_result() {
    let redraw = b"\x1b_Ga=d\x1b\\\x1b[4;3H\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\";
    for split in 0..=redraw.len() {
        let mut engine = engine();
        assert_eq!(engine.advance(UPLOAD), b"");
        assert_eq!(engine.advance(&redraw[..split]), b"");
        assert_eq!(engine.advance(&redraw[split..]), b"");
        assert_eq!(anchors(&engine), [(3, 2)], "split {split}");
    }
}

#[test]
fn deleting_aborts_an_incomplete_upload_and_allows_a_new_one() {
    let mut engine = engine();
    assert_eq!(
        engine.advance(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1,i=7,q=2,m=1;/wAA\x1b\\"),
        b""
    );
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\"), b"");
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(anchors(&engine), [(0, 0)]);
    assert_eq!(
        engine.state().image_placements()[0].record().image.rgba,
        [255, 0, 0, 255]
    );
}

#[test]
fn deleting_one_named_placement_keeps_pixels_used_by_another() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d\x1b\\\x1b_Ga=p,i=7,p=1,c=1,r=1,C=1,q=2\x1b\\\x1b[5;1H\x1b_Ga=p,i=7,p=2,c=1,r=1,C=1,q=2\x1b\\"), b"");
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7,p=1\x1b\\"), b"");
    assert_eq!(anchors(&engine), [(4, 0)]);
    assert_eq!(engine.state().kitty_images[0].display.image_id, Some(7));
    assert_eq!(engine.advance(b"\x1b_Ga=d,d=I,i=7,p=2\x1b\\"), b"");
    assert_eq!(engine.state().kitty_images, []);
}

#[test]
fn a_hard_reset_removes_uploads_as_well_as_placements() {
    let mut engine = engine();
    assert_eq!(engine.advance(UPLOAD), b"");
    assert_eq!(engine.advance(b"\x1bc"), b"");
    assert_eq!(engine.state().kitty_images, []);
    assert_eq!(anchors(&engine), []);
}
