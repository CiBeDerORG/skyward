//! The reboot page and the fastboot page: where the phone leaves for, and
//! how fastboot stands.

use super::*;

/// Where the reboot page sends the device, in the order it lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    System,
    Recovery,
    Fastboot,
    Bootloader,
    Edl,
    PowerOff,
}

pub const TARGETS: [Target; 6] = [Target::System, Target::Recovery, Target::Fastboot, Target::Bootloader, Target::Edl, Target::PowerOff];

/// Where the fastboot page goes, in the order it lists them: the volume
/// keys move between them and the power key takes one.
pub const FASTBOOT_CHOICES: [(Text, Action); 4] = [
    (texts::installed::REBOOT_TO_SYSTEM, Action::RebootTo(Target::System)),
    (texts::afterwards::ENTER_RECOVERY, Action::EnterRecovery),
    (texts::fastboot::REBOOT_TO_BOOTLOADER, Action::RebootTo(Target::Bootloader)),
    (texts::reboot::target::POWER_OFF, Action::RebootTo(Target::PowerOff)),
];

/// How far a computer has the USB port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbLink {
    /// No cable, or nothing answering at its other end.
    Unplugged,
    /// Plugged in, with the computer still setting the port up.
    Connecting,
    /// Set up by the computer, which can send commands.
    Connected,
}

/// Fastboot as its page shows it: whether the phone's fastbootd runs and
/// how far a computer has the port, as the runtime last read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fastboot {
    pub running: bool,
    pub link: UsbLink,
}

impl App {
    /// The runtime read whether fastbootd runs and how far a computer has
    /// the port: keep it and show it if it changed.
    pub fn set_fastboot(&mut self, running: bool, link: UsbLink) {
        let now = Fastboot { running, link };
        if self.state.fastboot != now {
            self.state.fastboot = now;
            self.refresh();
        }
    }

    /// Fastboot has stopped and the recovery is up: its pages start again
    /// from the home page, which nothing can go back from to fastboot.
    pub fn enter_recovery(&mut self) {
        self.stack = vec![(Screen::Home, 0.0, Focus::Unset)];
        self.settle_page();
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;

    #[test]
    fn a_tap_reboots_to_the_chosen_target_once_on_release() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Reboot, W, H, PPD);
        tap(&mut app, &f, &i, Action::RebootTarget(3));
        let (x, y) = spot(&mut app, &f, &i, Action::Confirm(Confirm::Reboot));
        assert!(app.touch(Touch::Down(x, y), &f, 1000).is_empty());
        assert_eq!(app.touch(Touch::Up, &f, 1060), vec![Effect::Reboot(Target::Bootloader)]);
        assert!(app.touch(Touch::Up, &f, 1061).is_empty());
    }

    #[test]
    fn dragging_across_or_scrolling_from_the_button_does_not_reboot() {
        let (f, i) = (fonts(), info());
        for (dx, dy) in [(120.0, 0.0), (0.0, -120.0)] {
            let mut app = App::new(Screen::Reboot, W, H, PPD);
            let (x, y) = spot(&mut app, &f, &i, Action::Confirm(Confirm::Reboot));
            assert!(app.touch(Touch::Down(x, y), &f, 1000).is_empty());
            assert!(app.touch(Touch::Move(x + dx, y + dy), &f, 1030).is_empty());
            assert!(app.touch(Touch::Move(x, y), &f, 1060).is_empty());
            assert!(app.touch(Touch::Up, &f, 1090).is_empty());
        }
    }

    #[test]
    fn fastboot_is_chosen_and_restarted_to_like_any_target() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Reboot, W, H, PPD);
        let fastboot = TARGETS.iter().position(|t| *t == Target::Fastboot).unwrap();
        tap(&mut app, &f, &i, Action::RebootTarget(fastboot));
        assert_eq!(app.state.reboot, fastboot);
        assert_eq!(tap_confirm(&mut app, &f, &i, Confirm::Reboot), vec![Effect::Reboot(Target::Fastboot)]);
    }

    #[test]
    fn the_fastboot_page_is_worked_with_the_keys() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Fastboot, W, H, PPD);
        let choice = |k: usize| Some(Stop::Hit(FASTBOOT_CHOICES[k].1));
        assert_eq!(lit(&mut app, &f, &i), choice(0), "the first choice is lit as the page opens");
        assert!(press(&mut app, Key::VolumeDown, 0).is_empty());
        assert_eq!(lit(&mut app, &f, &i), choice(1), "down moves to the next choice");
        assert!(app.key(Key::Power, 0).is_empty(), "power takes it as it comes up");
        assert_eq!(app.key_up(Key::Power, 0), vec![Effect::EnterRecovery]);
        assert!(app.display_on(), "power leaves the panel on here");
        press(&mut app, Key::VolumeUp, 0);
        press(&mut app, Key::VolumeUp, 0);
        assert_eq!(lit(&mut app, &f, &i), choice(FASTBOOT_CHOICES.len() - 1), "up from the first goes round to the last");
        assert_eq!(press(&mut app, Key::Power, 0), vec![Effect::Reboot(Target::PowerOff)]);
        assert_eq!(tap(&mut app, &f, &i, Action::RebootTo(Target::Bootloader)), vec![Effect::Reboot(Target::Bootloader)], "a touch takes a choice too");
        assert!(press(&mut app, Key::Back, 0).is_empty());
        assert_eq!(app.screen(), Screen::Fastboot, "there is nothing to go back to");
    }

    #[test]
    fn the_recovery_comes_up_from_fastboot_at_the_home_page() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Fastboot, W, H, PPD);
        app.frame(&f, &i);
        app.enter_recovery();
        assert_eq!(app.screen(), Screen::Home);
        app.key(Key::Back, 0);
        assert_eq!(app.screen(), Screen::Home, "fastboot is not behind it");
        assert_eq!(lit(&mut app, &f, &i), Some(Stop::Hit(Action::Open(Screen::Search))), "the keys go on working the pages");
        app.touch(Touch::Down(W as f32 / 2.0, 200.0), &f, 0);
        app.touch(Touch::Up, &f, 0);
        assert_eq!(press(&mut app, Key::Power, 0), vec![Effect::Ticks]);
        assert!(!app.display_on(), "with the light put out, power turns the panel off");
    }
}
