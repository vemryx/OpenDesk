//! Injeção de entrada com `SendInput`.
//!
//! Limitação do Windows (UIPI): um processo sem elevação não consegue injetar
//! entrada em janelas elevadas nem na área de trabalho segura do UAC. Resolver
//! isso do jeito certo exige o serviço previsto no roteiro; não há atalho
//! seguro.

use crate::capture::DisplayInfo;
use opendesk_proto::control::{InputEvent, MouseButton};
use std::collections::HashSet;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

/// Aplica no sistema os eventos de entrada recebidos do visualizador.
pub struct Injector {
    display: DisplayInfo,
    keys_down: HashSet<(u16, bool)>,
    buttons_down: HashSet<MouseButton>,
}

impl Injector {
    pub fn new(display: &DisplayInfo) -> Self {
        Self { display: display.clone(), keys_down: HashSet::new(), buttons_down: HashSet::new() }
    }

    pub fn apply(&mut self, event: InputEvent) {
        match event {
            InputEvent::MouseMove { x, y } => self.mouse(x, y, MOUSE_EVENT_FLAGS(0), 0),
            InputEvent::MouseButton { button, down, x, y } => {
                if down {
                    self.buttons_down.insert(button);
                } else {
                    self.buttons_down.remove(&button);
                }
                let (flags, data) = button_flags(button, down);
                self.mouse(x, y, flags, data);
            }
            InputEvent::Wheel { dx, dy } => {
                if dy != 0 {
                    send(&[mouse_input(0, 0, MOUSEEVENTF_WHEEL, dy as i32 as u32)]);
                }
                if dx != 0 {
                    send(&[mouse_input(0, 0, MOUSEEVENTF_HWHEEL, dx as i32 as u32)]);
                }
            }
            InputEvent::Key { scancode, extended, down } => {
                if down {
                    self.keys_down.insert((scancode, extended));
                } else {
                    self.keys_down.remove(&(scancode, extended));
                }
                send(&[key_input(scancode, extended, down)]);
            }
        }
    }

    /// Solta tudo o que o visualizador deixou pressionado. Sem isto, uma
    /// sessão que cai no meio de um Ctrl deixaria a tecla presa no host.
    pub fn release_all(&mut self) {
        for (scancode, extended) in self.keys_down.drain() {
            send(&[key_input(scancode, extended, false)]);
        }
        for button in self.buttons_down.drain() {
            let (flags, data) = button_flags(button, false);
            send(&[mouse_input(0, 0, flags, data)]);
        }
    }

    fn mouse(&self, x: u16, y: u16, extra: MOUSE_EVENT_FLAGS, data: u32) {
        // Posição normalizada sobre o monitor compartilhado → pixel na área de
        // trabalho virtual → coordenada absoluta 0..65535 que o SendInput usa.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let d = &self.display;
        let px = d.x as i64 + (x as i64 * (d.width as i64 - 1) + 32767) / 65535;
        let py = d.y as i64 + (y as i64 * (d.height as i64 - 1) + 32767) / 65535;
        let ax = ((px - vx as i64) * 65535 + (vw as i64 - 1) / 2) / (vw as i64 - 1);
        let ay = ((py - vy as i64) * 65535 + (vh as i64 - 1) / 2) / (vh as i64 - 1);
        let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | extra;
        send(&[mouse_input(ax.clamp(0, 65535) as i32, ay.clamp(0, 65535) as i32, flags, data)]);
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn button_flags(button: MouseButton, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key_input(scancode: u16, extended: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let _: KEYBD_EVENT_FLAGS = flags;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scancode, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(inputs: &[INPUT]) {
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        tracing::debug!("SendInput aceitou {sent} de {} eventos (janela elevada em foco?)", inputs.len());
    }
}
