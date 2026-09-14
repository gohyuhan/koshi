//! Tests for Kitty image-placement and deletion commands.

use super::*;

#[test]
fn parses_a_place_command_without_payload() {
    let command = parse_kitty_command(b"Ga=p,i=7,p=9", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.get_command_kind(), KittyCommandKind::Place);
    assert_eq!(command.get_image_display().image_id, Some(7));
    assert_eq!(command.get_image_display().placement_id, Some(9));
    assert!(!command.should_free_image_data());
}

#[test]
fn parses_relative_parent_and_signed_offsets() {
    let command = parse_kitty_command(b"Ga=p,i=7,p=9,P=3,Q=4,H=-2,V=5", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.get_image_display().relative_image_id, Some(3));
    assert_eq!(command.get_image_display().relative_placement_id, Some(4));
    assert_eq!(command.get_image_display().relative_column_offset, -2);
    assert_eq!(command.get_image_display().relative_row_offset, 5);
}

#[test]
fn parses_a_delete_selector_and_uppercase_free_data() {
    let command = parse_kitty_command(b"Ga=d,d=I,i=7", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(
        command.get_command_kind(),
        KittyCommandKind::Delete(KittyDelete::ImageId)
    );
    assert_eq!(command.get_image_display().image_id, Some(7));
    assert!(command.should_free_image_data());
}

#[test]
fn parses_delete_selector_after_other_fields() {
    let command = parse_kitty_command(b"Ga=d,i=7,d=I", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(
        command.get_command_kind(),
        KittyCommandKind::Delete(KittyDelete::ImageId)
    );
    assert_eq!(command.get_image_display().image_id, Some(7));
    assert!(command.should_free_image_data());
}

#[test]
fn rejects_payload_bytes_on_a_command() {
    assert_eq!(
        parse_kitty_command(b"Ga=p", b"AAAA"),
        Some(Err(GraphicsError::InvalidCommand {
            protocol: GraphicsProtocol::Kitty,
        }))
    );
}

#[test]
fn non_command_action_is_left_for_image_transfer_parsing() {
    assert_eq!(parse_kitty_command(b"Ga=T,f=32,s=1,v=1", b"AAAA"), None);
}

#[test]
fn parses_animation_frame_transfer_fields_and_payload() {
    let command = parse_kitty_command(b"Ga=f,i=7,f=32,s=1,v=1,r=2,c=1,x=0,y=0,z=12", b"/wAA/w==")
        .expect("the body is a Kitty command")
        .expect("the command is valid");

    assert_eq!(command.get_command_kind(), KittyCommandKind::AnimationFrame);
    let animation_command = command.get_animation_command().expect("animation fields");
    assert_eq!(animation_command.media_format, Some(32));
    assert_eq!(animation_command.frame_width_pixels, Some(1));
    assert_eq!(animation_command.frame_height_pixels, Some(1));
    assert_eq!(animation_command.frame_number, Some(2));
    assert_eq!(animation_command.base_frame_number, Some(1));
    assert_eq!(animation_command.gap_milliseconds, Some(12));
    assert_eq!(animation_command.encoded_payload_bytes, b"/wAA/w==");
}

#[test]
fn leaves_a_multipart_animation_frame_for_transfer_assembly() {
    assert_eq!(
        parse_kitty_command(b"Ga=f,i=7,f=32,s=1,v=1,r=1,m=1", b"/wAA"),
        None
    );
}

#[test]
fn parses_animation_control_compose_and_delete_actions() {
    let animation_control_command = parse_kitty_command(b"Ga=a,i=7,c=2,s=3,v=2,z=-1", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(
        animation_control_command.get_command_kind(),
        KittyCommandKind::AnimationControl
    );
    assert_eq!(
        animation_control_command
            .get_animation_command()
            .expect("control fields")
            .frame_number,
        Some(2)
    );
    assert_eq!(
        animation_control_command
            .get_animation_command()
            .expect("control fields")
            .playback_state,
        Some(3)
    );
    assert_eq!(
        animation_control_command
            .get_animation_command()
            .expect("control fields")
            .loop_count,
        Some(2)
    );
    assert_eq!(
        animation_control_command
            .get_animation_command()
            .expect("control fields")
            .gap_milliseconds,
        Some(-1)
    );

    let compose_command = parse_kitty_command(b"Ga=c,i=7,r=1,c=2,x=1,y=2,X=3,Y=4,w=5,h=6,C=1", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(
        compose_command.get_command_kind(),
        KittyCommandKind::AnimationCompose
    );
    let compose_animation_command = compose_command
        .get_animation_command()
        .expect("compose fields");
    assert_eq!(compose_animation_command.source_frame_number, Some(1));
    assert_eq!(compose_animation_command.destination_frame_number, Some(2));
    assert_eq!(compose_animation_command.frame_width_pixels, Some(5));
    assert_eq!(compose_animation_command.frame_height_pixels, Some(6));
    assert!(compose_animation_command.replaces_destination_pixels);

    let delete_command = parse_kitty_command(b"Ga=d,d=f,i=7,r=2", &[])
        .expect("the body is a Kitty command")
        .expect("the command is valid");
    assert_eq!(
        delete_command.get_command_kind(),
        KittyCommandKind::AnimationDelete
    );
    assert_eq!(
        delete_command
            .get_animation_command()
            .expect("delete fields")
            .frame_number,
        Some(2)
    );
}

#[test]
fn command_header_limit_is_checked_before_rewriting_fields() {
    let mut control_header_bytes = b"Ga=p".to_vec();
    control_header_bytes.extend(std::iter::repeat_n(
        b',',
        MAX_GRAPHICS_CONTROL_BYTE_COUNT - control_header_bytes.len(),
    ));
    assert!(parse_kitty_command(&control_header_bytes, &[])
        .is_some_and(|parsed_command_result| parsed_command_result.is_ok()));

    control_header_bytes.push(b'x');
    assert_eq!(
        parse_kitty_command(&control_header_bytes, &[]),
        Some(Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        }))
    );
}

#[test]
fn parse_reply_display_keeps_only_response_identifiers_and_suppression_level() {
    let image_display = parse_reply_display(b"Ga=T,f=32,i=7,I=8,p=9,q=3");

    assert_eq!(image_display.image_id, Some(7));
    assert_eq!(image_display.image_number, Some(8));
    assert_eq!(image_display.placement_id, Some(9));
    assert_eq!(image_display.response_suppression_level, 2);
    assert_eq!(image_display.requested_width, None);
}
