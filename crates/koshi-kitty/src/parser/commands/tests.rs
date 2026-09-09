//! Tests for Kitty image-placement and deletion commands.

use super::*;

#[test]
fn parses_a_place_command_without_payload() {
    let command = parse_command(b"Ga=p,i=7,p=9", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.kind(), KittyCommandKind::Place);
    assert_eq!(command.display().image_id, Some(7));
    assert_eq!(command.display().placement_id, Some(9));
    assert!(!command.free_data());
}

#[test]
fn parses_relative_parent_and_signed_offsets() {
    let command = parse_command(b"Ga=p,i=7,p=9,P=3,Q=4,H=-2,V=5", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.display().relative_image_id, Some(3));
    assert_eq!(command.display().relative_placement_id, Some(4));
    assert_eq!(command.display().relative_offset_x, -2);
    assert_eq!(command.display().relative_offset_y, 5);
}

#[test]
fn parses_a_delete_selector_and_uppercase_free_data() {
    let command = parse_command(b"Ga=d,d=I,i=7", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.kind(), KittyCommandKind::Delete(KittyDelete::Id));
    assert_eq!(command.display().image_id, Some(7));
    assert!(command.free_data());
}

#[test]
fn parses_delete_selector_after_other_fields() {
    let command = parse_command(b"Ga=d,i=7,d=I", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.kind(), KittyCommandKind::Delete(KittyDelete::Id));
    assert_eq!(command.display().image_id, Some(7));
    assert!(command.free_data());
}

#[test]
fn rejects_payload_bytes_on_a_command() {
    assert_eq!(
        parse_command(b"Ga=p", b"AAAA"),
        Some(Err(GraphicsError::InvalidCommand {
            protocol: GraphicsProtocol::Kitty,
        }))
    );
}

#[test]
fn non_command_action_is_left_for_image_transfer_parsing() {
    assert_eq!(parse_command(b"Ga=T,f=32,s=1,v=1", b"AAAA"), None);
}

#[test]
fn parses_animation_frame_transfer_fields_and_payload() {
    let command = parse_command(b"Ga=f,i=7,f=32,s=1,v=1,r=2,c=1,x=0,y=0,z=12", b"/wAA/w==")
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.kind(), KittyCommandKind::AnimationFrame);
    let animation = command.animation().expect("animation fields");
    assert_eq!(animation.format, Some(32));
    assert_eq!(animation.width, Some(1));
    assert_eq!(animation.height, Some(1));
    assert_eq!(animation.frame, Some(2));
    assert_eq!(animation.base_frame, Some(1));
    assert_eq!(animation.gap_ms, Some(12));
    assert_eq!(animation.payload, b"/wAA/w==");
}

#[test]
fn leaves_a_multipart_animation_frame_for_transfer_assembly() {
    assert_eq!(
        parse_command(b"Ga=f,i=7,f=32,s=1,v=1,r=1,m=1", b"/wAA"),
        None
    );
}

#[test]
fn parses_animation_control_compose_and_delete_actions() {
    let control = parse_command(b"Ga=a,i=7,c=2,s=3,v=2,z=-1", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(control.kind(), KittyCommandKind::AnimationControl);
    assert_eq!(control.animation().expect("control fields").frame, Some(2));
    assert_eq!(control.animation().expect("control fields").state, Some(3));
    assert_eq!(control.animation().expect("control fields").loops, Some(2));
    assert_eq!(
        control.animation().expect("control fields").gap_ms,
        Some(-1)
    );

    let compose = parse_command(b"Ga=c,i=7,r=1,c=2,x=1,y=2,X=3,Y=4,w=5,h=6,C=1", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(compose.kind(), KittyCommandKind::AnimationCompose);
    let compose = compose.animation().expect("compose fields");
    assert_eq!(compose.source_frame, Some(1));
    assert_eq!(compose.destination_frame, Some(2));
    assert_eq!(compose.width, Some(5));
    assert_eq!(compose.height, Some(6));
    assert!(compose.replace);

    let delete = parse_command(b"Ga=d,d=f,i=7,r=2", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(delete.kind(), KittyCommandKind::AnimationDelete);
    assert_eq!(delete.animation().expect("delete fields").frame, Some(2));
}

#[test]
fn command_header_limit_is_checked_before_rewriting_fields() {
    let mut header = b"Ga=p".to_vec();
    header.extend(std::iter::repeat_n(
        b',',
        MAX_GRAPHICS_CONTROL_BYTES - header.len(),
    ));
    assert!(parse_command(&header, &[]).is_some_and(|result| result.is_ok()));

    header.push(b'x');
    assert_eq!(
        parse_command(&header, &[]),
        Some(Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        }))
    );
}

#[test]
fn reply_display_keeps_only_response_identifiers_and_quiet_level() {
    let display = reply_display(b"Ga=T,f=32,i=7,I=8,p=9,q=3");

    assert_eq!(display.image_id, Some(7));
    assert_eq!(display.image_number, Some(8));
    assert_eq!(display.placement_id, Some(9));
    assert_eq!(display.quiet, 2);
    assert_eq!(display.width, None);
}
