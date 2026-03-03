//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Event translation from baseview to masonry
//!
//! Converts baseview events into masonry-compatible pointer and keyboard events.
//! Adapted from masonry_baseview,
//! (see https://github.com/baltobor/masonry_baseview for reference)

use baseview::{Event, MouseButton, MouseEvent, ScrollDelta, WindowEvent};
use keyboard_types::Modifiers as KbModifiers;
use masonry::core::pointer::PointerButtons;
use masonry::core::{
    Modifiers, PointerButton, PointerButtonEvent, PointerEvent, PointerInfo, PointerId,
    PointerScrollEvent, PointerState, PointerType, PointerUpdate,
};
use masonry::dpi::PhysicalPosition;

/// Translate a baseview mouse button to masonry pointer button.
pub fn translate_mouse_button(button: MouseButton) -> PointerButton {
    match button {
        MouseButton::Left => PointerButton::Primary,
        MouseButton::Right => PointerButton::Secondary,
        MouseButton::Middle => PointerButton::Auxiliary,
        MouseButton::Back => PointerButton::X1,
        MouseButton::Forward => PointerButton::X2,
        MouseButton::Other(_) => PointerButton::Primary,
    }
}

/// Translate baseview modifiers to masonry modifiers.
pub fn translate_modifiers(mods: KbModifiers) -> Modifiers {
    let mut result = Modifiers::empty();
    if mods.contains(KbModifiers::SHIFT) {
        result |= Modifiers::SHIFT;
    }
    if mods.contains(KbModifiers::CONTROL) {
        result |= Modifiers::CONTROL;
    }
    if mods.contains(KbModifiers::ALT) {
        result |= Modifiers::ALT;
    }
    if mods.contains(KbModifiers::META) {
        result |= Modifiers::META;
    }
    result
}

/// Event translator that maintains pointer state between events.
pub struct EventTranslator {
    pointer_x: f64,
    pointer_y: f64,
    buttons: PointerButtons,
    modifiers: Modifiers,
    scale_factor: f64,
    start_time: std::time::Instant,
}

impl EventTranslator {
    pub fn new(scale_factor: f64) -> Self {
        Self {
            pointer_x: 0.0,
            pointer_y: 0.0,
            buttons: PointerButtons::default(),
            modifiers: Modifiers::empty(),
            scale_factor,
            start_time: std::time::Instant::now(),
        }
    }

    pub fn set_scale_factor(&mut self, scale: f64) {
        self.scale_factor = scale;
    }

    /// Translate a baseview event into a masonry event.
    /// Returns None if the event doesn't map to a masonry event.
    pub fn translate(&mut self, event: &Event) -> Option<MasonryEvent> {
        match event {
            Event::Mouse(mouse) => self.translate_mouse(mouse),
            Event::Keyboard(kb) => self.translate_keyboard(kb),
            Event::Window(win) => self.translate_window(win),
        }
    }

    fn get_time_nanos(&self) -> u64 {
        self.start_time.elapsed().as_nanos() as u64
    }

    fn make_pointer_info(&self) -> PointerInfo {
        PointerInfo {
            pointer_id: Some(PointerId::PRIMARY),
            persistent_device_id: None,
            pointer_type: PointerType::Mouse,
        }
    }

    fn make_pointer_state(&self) -> PointerState {
        PointerState {
            time: self.get_time_nanos(),
            position: PhysicalPosition::new(
                self.pointer_x * self.scale_factor,
                self.pointer_y * self.scale_factor,
            ),
            buttons: self.buttons.clone(),
            modifiers: self.modifiers,
            count: 1,
            contact_geometry: masonry::dpi::PhysicalSize::new(1.0, 1.0),
            orientation: Default::default(),
            pressure: 0.0,
            tangential_pressure: 0.0,
            scale_factor: self.scale_factor,
        }
    }

    fn translate_mouse(&mut self, event: &MouseEvent) -> Option<MasonryEvent> {
        match event {
            MouseEvent::CursorMoved { position, modifiers } => {
                self.pointer_x = position.x / self.scale_factor;
                self.pointer_y = position.y / self.scale_factor;
                self.modifiers = translate_modifiers(*modifiers);

                let update = PointerUpdate {
                    pointer: self.make_pointer_info(),
                    current: self.make_pointer_state(),
                    coalesced: vec![],
                    predicted: vec![],
                };

                Some(MasonryEvent::Pointer(PointerEvent::Move(update)))
            }

            MouseEvent::ButtonPressed { button, modifiers } => {
                self.modifiers = translate_modifiers(*modifiers);
                let btn = translate_mouse_button(*button);
                self.buttons |= btn;

                let event = PointerButtonEvent {
                    button: Some(btn),
                    pointer: self.make_pointer_info(),
                    state: self.make_pointer_state(),
                };

                Some(MasonryEvent::Pointer(PointerEvent::Down(event)))
            }

            MouseEvent::ButtonReleased { button, modifiers } => {
                self.modifiers = translate_modifiers(*modifiers);
                let btn = translate_mouse_button(*button);
                self.buttons.remove(btn);

                let event = PointerButtonEvent {
                    button: Some(btn),
                    pointer: self.make_pointer_info(),
                    state: self.make_pointer_state(),
                };

                Some(MasonryEvent::Pointer(PointerEvent::Up(event)))
            }

            MouseEvent::WheelScrolled { delta, modifiers } => {
                self.modifiers = translate_modifiers(*modifiers);

                let scroll_delta = match delta {
                    ScrollDelta::Lines { x, y } => {
                        masonry::core::ScrollDelta::LineDelta(*x, *y)
                    }
                    ScrollDelta::Pixels { x, y } => {
                        masonry::core::ScrollDelta::PixelDelta(PhysicalPosition::new(
                            *x as f64, *y as f64,
                        ))
                    }
                };

                let event = PointerScrollEvent {
                    pointer: self.make_pointer_info(),
                    state: self.make_pointer_state(),
                    delta: scroll_delta,
                };

                Some(MasonryEvent::Pointer(PointerEvent::Scroll(event)))
            }

            MouseEvent::CursorEntered => Some(MasonryEvent::Pointer(PointerEvent::Enter(
                self.make_pointer_info(),
            ))),

            MouseEvent::CursorLeft => Some(MasonryEvent::Pointer(PointerEvent::Leave(
                self.make_pointer_info(),
            ))),

            _ => None,
        }
    }

    fn translate_keyboard(
        &mut self,
        event: &keyboard_types::KeyboardEvent,
    ) -> Option<MasonryEvent> {
        self.modifiers = translate_modifiers(event.modifiers);
        Some(MasonryEvent::Keyboard(event.clone()))
    }

    fn translate_window(&mut self, event: &WindowEvent) -> Option<MasonryEvent> {
        match event {
            WindowEvent::Resized(info) => {
                self.scale_factor = info.scale();
                Some(MasonryEvent::Resize {
                    width: info.physical_size().width as f64,
                    height: info.physical_size().height as f64,
                    scale: info.scale(),
                })
            }
            WindowEvent::Focused => Some(MasonryEvent::Focus(true)),
            WindowEvent::Unfocused => Some(MasonryEvent::Focus(false)),
            WindowEvent::WillClose => Some(MasonryEvent::Close),
        }
    }
}

/// Events that can be sent to masonry.
pub enum MasonryEvent {
    Pointer(PointerEvent),
    Keyboard(keyboard_types::KeyboardEvent),
    Resize {
        width: f64,
        height: f64,
        scale: f64,
    },
    #[allow(dead_code)]
    Focus(bool),
    Close,
}
