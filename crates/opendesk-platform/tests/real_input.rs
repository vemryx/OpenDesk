//! Testes que mexem no computador de verdade. Ficam ignorados por padrão;
//! rode com `cargo test -p opendesk-platform -- --ignored` numa sessão com
//! área de trabalho.
#![cfg(windows)]

use opendesk_platform::capture::{Capturer, displays};
use opendesk_platform::input::Injector;
use opendesk_proto::control::InputEvent;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursorPos(point: *mut [i32; 2]) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
}

fn cursor() -> (i32, i32) {
    let mut point = [0i32; 2];
    assert_ne!(unsafe { GetCursorPos(&mut point) }, 0);
    (point[0], point[1])
}

/// A posição normalizada enviada pelo visualizador tem de cair no pixel certo
/// do monitor compartilhado, em qualquer arranjo de monitores.
#[test]
#[ignore = "move o cursor real por um instante"]
fn mouse_move_lands_on_the_expected_pixel_of_each_display() {
    opendesk_platform::init_process();
    let original = cursor();
    let mut failures = Vec::new();

    for display in displays().unwrap() {
        let mut injector = Injector::new(&display);
        for (nx, ny) in [(0u16, 0u16), (65535, 65535), (32768, 32768), (16384, 49152)] {
            injector.apply(InputEvent::MouseMove { x: nx, y: ny });
            std::thread::sleep(std::time::Duration::from_millis(30));
            let expected = (
                display.x + (nx as f64 / 65535.0 * (display.width - 1) as f64).round() as i32,
                display.y + (ny as f64 / 65535.0 * (display.height - 1) as f64).round() as i32,
            );
            let got = cursor();
            if (got.0 - expected.0).abs() > 1 || (got.1 - expected.1).abs() > 1 {
                failures.push(format!("{} ({nx}, {ny}): esperava {expected:?}, cursor em {got:?}", display.name));
            }
        }
    }

    unsafe { SetCursorPos(original.0, original.1) };
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
#[ignore = "captura a tela real"]
fn captures_a_full_frame_of_every_display() {
    opendesk_platform::init_process();
    for (index, display) in displays().unwrap().into_iter().enumerate() {
        let mut capturer = Capturer::new(index).unwrap();
        // O primeiro quadro da duplicação é sempre a tela inteira.
        let frame = (0..50).find_map(|_| capturer.frame(100).unwrap().map(|f| (f.width, f.height, f.data.len())));
        let (width, height, len) = frame.expect("nenhum quadro em 5 s");
        assert_eq!((width as u32, height as u32), (display.width, display.height), "{}", display.name);
        assert_eq!(len, width * height * 4);
    }
}
