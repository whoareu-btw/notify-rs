//! Every opcode and enum value in src/wl/proto.rs against the table
//! transcribed from the protocol XML (tests/fixtures/wayland_protocol.txt).

use std::collections::HashMap;

use notify_rs::wl::proto::*;

fn table() -> HashMap<(String, String, String), u16> {
    let mut m = HashMap::new();
    for l in include_str!("fixtures/wayland_protocol.txt").lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('|').collect();
        assert_eq!(f.len(), 5, "{l}");
        let dup = m.insert((f[0].to_string(), f[1].to_string(), f[3].to_string()), f[2].parse().unwrap());
        assert!(dup.is_none(), "duplicate entry: {l}");
    }
    m
}

macro_rules! check {
    ($t:expr, $( $iface:literal $kind:literal $name:literal => $val:expr ),* $(,)?) => {
        $(
            let want = $t.get(&($iface.to_string(), $kind.to_string(), $name.to_string()))
                .unwrap_or_else(|| panic!("{} {} {} is not in the protocol table", $iface, $kind, $name));
            assert_eq!(*want, $val, "{}.{} ({})", $iface, $name, $kind);
        )*
    };
}

#[test]
fn opcodes_match_the_protocol_xml() {
    let t = table();
    check!(t,
        "wl_display" "req" "sync" => display::SYNC,
        "wl_display" "req" "get_registry" => display::GET_REGISTRY,
        "wl_display" "evt" "error" => display::ERROR,
        "wl_display" "evt" "delete_id" => display::DELETE_ID,
        "wl_registry" "req" "bind" => registry::BIND,
        "wl_registry" "evt" "global" => registry::GLOBAL,
        "wl_registry" "evt" "global_remove" => registry::GLOBAL_REMOVE,
        "wl_callback" "evt" "done" => callback::DONE,
        "wl_compositor" "req" "create_surface" => compositor::CREATE_SURFACE,
        "wl_compositor" "req" "create_region" => compositor::CREATE_REGION,
        "wl_region" "req" "destroy" => region::DESTROY,
        "wl_region" "req" "add" => region::ADD,
        "wl_shm" "req" "create_pool" => shm::CREATE_POOL,
        "wl_shm" "evt" "format" => shm::FORMAT,
        "wl_shm_pool" "req" "create_buffer" => shm_pool::CREATE_BUFFER,
        "wl_shm_pool" "req" "destroy" => shm_pool::DESTROY,
        "wl_buffer" "req" "destroy" => buffer::DESTROY,
        "wl_buffer" "evt" "release" => buffer::RELEASE,
        "wl_seat" "req" "get_pointer" => seat::GET_POINTER,
        "wl_seat" "req" "release" => seat::RELEASE,
        "wl_seat" "evt" "capabilities" => seat::CAPABILITIES,
        "wl_pointer" "req" "release" => pointer::RELEASE,
        "wl_pointer" "evt" "enter" => pointer::ENTER,
        "wl_pointer" "evt" "leave" => pointer::LEAVE,
        "wl_pointer" "evt" "motion" => pointer::MOTION,
        "wl_pointer" "evt" "button" => pointer::BUTTON,
        "wl_pointer" "evt" "axis" => pointer::AXIS,
        "wl_output" "req" "release" => output::RELEASE,
        "wl_output" "evt" "scale" => output::SCALE,
        "wl_surface" "req" "destroy" => surface::DESTROY,
        "wl_surface" "req" "attach" => surface::ATTACH,
        "wl_surface" "req" "set_input_region" => surface::SET_INPUT_REGION,
        "wl_surface" "req" "commit" => surface::COMMIT,
        "wl_surface" "req" "set_buffer_scale" => surface::SET_BUFFER_SCALE,
        "wl_surface" "req" "damage_buffer" => surface::DAMAGE_BUFFER,
        "wl_surface" "evt" "enter" => surface::ENTER,
        "wl_surface" "evt" "leave" => surface::LEAVE,
        "wl_surface" "evt" "preferred_buffer_scale" => surface::PREFERRED_BUFFER_SCALE,
        "zwlr_layer_shell_v1" "req" "get_layer_surface" => layer_shell::GET_LAYER_SURFACE,
        "zwlr_layer_surface_v1" "req" "set_size" => layer_surface::SET_SIZE,
        "zwlr_layer_surface_v1" "req" "set_anchor" => layer_surface::SET_ANCHOR,
        "zwlr_layer_surface_v1" "req" "set_exclusive_zone" => layer_surface::SET_EXCLUSIVE_ZONE,
        "zwlr_layer_surface_v1" "req" "set_margin" => layer_surface::SET_MARGIN,
        "zwlr_layer_surface_v1" "req" "set_keyboard_interactivity" => layer_surface::SET_KEYBOARD_INTERACTIVITY,
        "zwlr_layer_surface_v1" "req" "ack_configure" => layer_surface::ACK_CONFIGURE,
        "zwlr_layer_surface_v1" "req" "destroy" => layer_surface::DESTROY,
        "zwlr_layer_surface_v1" "evt" "configure" => layer_surface::CONFIGURE,
        "zwlr_layer_surface_v1" "evt" "closed" => layer_surface::CLOSED,
        "wp_viewporter" "req" "destroy" => viewporter::DESTROY,
        "wp_viewporter" "req" "get_viewport" => viewporter::GET_VIEWPORT,
        "wp_viewport" "req" "destroy" => viewport::DESTROY,
        "wp_viewport" "req" "set_destination" => viewport::SET_DESTINATION,
        "wp_fractional_scale_manager_v1" "req" "get_fractional_scale" => fractional_scale_manager::GET_FRACTIONAL_SCALE,
        "wp_fractional_scale_v1" "req" "destroy" => fractional_scale::DESTROY,
        "wp_fractional_scale_v1" "evt" "preferred_scale" => fractional_scale::PREFERRED_SCALE,
        "wp_cursor_shape_manager_v1" "req" "get_pointer" => cursor_shape_manager::GET_POINTER,
        "wp_cursor_shape_device_v1" "req" "destroy" => cursor_shape_device::DESTROY,
        "wp_cursor_shape_device_v1" "req" "set_shape" => cursor_shape_device::SET_SHAPE,
    );
}

#[test]
fn enum_values() {
    // layer-shell: background 0, bottom 1, top 2, overlay 3; anchor bits 1, 2, 4, 8
    assert_eq!((layer_shell::LAYER_TOP, layer_shell::LAYER_OVERLAY), (2, 3));
    assert_eq!(
        (layer_surface::ANCHOR_TOP, layer_surface::ANCHOR_BOTTOM, layer_surface::ANCHOR_LEFT, layer_surface::ANCHOR_RIGHT),
        (1, 2, 4, 8)
    );
    // wl_shm.format: argb8888 = 0 (xrgb8888 = 1); wl_seat.capability: pointer = 1
    assert_eq!((shm::FORMAT_ARGB8888, seat::CAP_POINTER), (0, 1));
    // wl_pointer: axis vertical_scroll = 0, button_state pressed = 1
    assert_eq!((pointer::AXIS_VERTICAL, pointer::BUTTON_PRESSED), (0, 1));
    // cursor-shape: default = 1, pointer = 4
    assert_eq!((cursor_shape_device::SHAPE_DEFAULT, cursor_shape_device::SHAPE_POINTER), (1, 4));
    // linux input codes
    assert_eq!(notify_rs::wl::client::BTN_LEFT, 0x110);
}
