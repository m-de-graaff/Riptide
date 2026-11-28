//! The console's input library, `GameInput.dll` (design: `docs/design/2026-11-28-input.md`).
//!
//! A title asks the library for the input object and reads controllers through it. The
//! library is on Windows too, under the same name, but Riptide answers for it itself: the
//! title must see a controller when the player has only a keyboard, bindings are Riptide's
//! to decide, and a test has to be able to press a button with nobody at the machine.
//!
//! The title sees one gamepad, always connected. What it reads from it is everything that
//! is held on any of three sources, together:
//!
//! - the first controller Windows reports through XInput;
//! - the keyboard, while a window of this process is in front ([`from_keys`]);
//! - `RIPTIDE_INPUT_SCRIPT`, a list of buttons to hold at times counted in seconds from
//!   the start of the run: `95:A,100-101.5:Down` presses A for 0.2 s at 95 s and holds
//!   Down from 100 s to 101.5 s.
//!
//! A reading is taken when the title asks for one, and a new one is kept whenever what is
//! held has changed, so a title that walks from one reading to the next sees every change
//! in order.
//!
//! Not built: more than one player, the keyboard and mouse as devices of their own,
//! reading callbacks, trigger rumble, bindings from `riptide.toml`, and every method of
//! the three interfaces not named here, which name themselves when called.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use riptide_pe::Symbol;

use crate::com::{self, Instance};
use crate::imports::Imports;
use crate::trace::trace;

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
/// The library's own errors: no such device, and no reading where one was asked for.
const DEVICE_NOT_FOUND: i32 = 0x838a_0002_u32 as i32;
const READING_NOT_FOUND: i32 = 0x838a_0003_u32 as i32;

/// The kind of input a gamepad gives, in the library's set of kinds.
const KIND_GAMEPAD: u32 = 0x4_0000;
/// A device that is connected and whose input is on.
const STATUS_CONNECTED: u32 = 0x3;

/// A gamepad's buttons, as the library numbers them.
pub(crate) mod button {
    pub(crate) const MENU: u32 = 0x1;
    pub(crate) const VIEW: u32 = 0x2;
    pub(crate) const A: u32 = 0x4;
    pub(crate) const B: u32 = 0x8;
    pub(crate) const X: u32 = 0x10;
    pub(crate) const Y: u32 = 0x20;
    pub(crate) const UP: u32 = 0x40;
    pub(crate) const DOWN: u32 = 0x80;
    pub(crate) const LEFT: u32 = 0x100;
    pub(crate) const RIGHT: u32 = 0x200;
    pub(crate) const LEFT_SHOULDER: u32 = 0x400;
    pub(crate) const RIGHT_SHOULDER: u32 = 0x800;
    pub(crate) const LEFT_STICK: u32 = 0x1000;
    pub(crate) const RIGHT_STICK: u32 = 0x2000;
}

/// What a gamepad holds at one moment, in the library's own layout: the buttons, the two
/// triggers from 0 to 1, and the two sticks from -1 to 1 with up and right positive.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Gamepad {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

impl Gamepad {
    /// Both held together: every button of either, and of each trigger and stick axis the
    /// one further from rest.
    fn with(self, other: Gamepad) -> Gamepad {
        let further = |a: f32, b: f32| if b.abs() > a.abs() { b } else { a };
        Gamepad {
            buttons: self.buttons | other.buttons,
            left_trigger: further(self.left_trigger, other.left_trigger),
            right_trigger: further(self.right_trigger, other.right_trigger),
            left_x: further(self.left_x, other.left_x),
            left_y: further(self.left_y, other.left_y),
            right_x: further(self.right_x, other.right_x),
            right_y: further(self.right_y, other.right_y),
        }
    }
}

/// What XInput reports of a controller, as a gamepad. XInput's sticks run from -32768 to
/// 32767 and its triggers from 0 to 255; nothing is taken off around the rest position,
/// which is the title's to do as it does on the console.
fn from_xinput(buttons: u16, triggers: [u8; 2], sticks: [i16; 4]) -> Gamepad {
    // XInput's buttons, each with the library's.
    const BUTTONS: [(u16, u32); 14] = [
        (0x0001, button::UP),
        (0x0002, button::DOWN),
        (0x0004, button::LEFT),
        (0x0008, button::RIGHT),
        (0x0010, button::MENU),
        (0x0020, button::VIEW),
        (0x0040, button::LEFT_STICK),
        (0x0080, button::RIGHT_STICK),
        (0x0100, button::LEFT_SHOULDER),
        (0x0200, button::RIGHT_SHOULDER),
        (0x1000, button::A),
        (0x2000, button::B),
        (0x4000, button::X),
        (0x8000, button::Y),
    ];
    let axis = |value: i16| (f32::from(value) / 32767.0).max(-1.0);
    Gamepad {
        buttons: BUTTONS
            .iter()
            .filter(|(host, _)| buttons & host != 0)
            .fold(0, |held, (_, ours)| held | ours),
        left_trigger: f32::from(triggers[0]) / 255.0,
        right_trigger: f32::from(triggers[1]) / 255.0,
        left_x: axis(sticks[0]),
        left_y: axis(sticks[1]),
        right_x: axis(sticks[2]),
        right_y: axis(sticks[3]),
    }
}

/// Windows' numbers for the keys the keyboard's gamepad uses.
mod key {
    pub(crate) const BACKSPACE: u8 = 0x08;
    pub(crate) const TAB: u8 = 0x09;
    pub(crate) const ENTER: u8 = 0x0d;
    pub(crate) const ESCAPE: u8 = 0x1b;
    pub(crate) const SPACE: u8 = 0x20;
    pub(crate) const LEFT: u8 = 0x25;
    pub(crate) const UP: u8 = 0x26;
    pub(crate) const RIGHT: u8 = 0x27;
    pub(crate) const DOWN: u8 = 0x28;
    pub(crate) const LEFT_SHIFT: u8 = 0xa0;
    pub(crate) const LEFT_CONTROL: u8 = 0xa2;
}

/// The gamepad the keyboard is, given which keys are held (design, "The keyboard").
///
/// | Gamepad | Keys |
/// | --- | --- |
/// | A, B, X, Y | Enter or Space, Backspace, F, R |
/// | Menu, View | Escape, Tab |
/// | Direction pad | the arrow keys |
/// | Left stick, and its button | W A S D, and C |
/// | Right stick, and its button | I J K L, and V |
/// | Shoulders | Q and E |
/// | Triggers | left Control and left Shift |
fn from_keys(held: impl Fn(u8) -> bool) -> Gamepad {
    const BUTTONS: [(u8, u32); 15] = [
        (key::ENTER, button::A),
        (key::SPACE, button::A),
        (key::BACKSPACE, button::B),
        (b'F', button::X),
        (b'R', button::Y),
        (key::ESCAPE, button::MENU),
        (key::TAB, button::VIEW),
        (key::UP, button::UP),
        (key::DOWN, button::DOWN),
        (key::LEFT, button::LEFT),
        (key::RIGHT, button::RIGHT),
        (b'Q', button::LEFT_SHOULDER),
        (b'E', button::RIGHT_SHOULDER),
        (b'C', button::LEFT_STICK),
        (b'V', button::RIGHT_STICK),
    ];
    // A stick axis from the two keys that push it either way.
    let axis =
        |back: u8, forth: u8| f32::from(u8::from(held(forth))) - f32::from(u8::from(held(back)));
    let trigger = |key: u8| f32::from(u8::from(held(key)));
    Gamepad {
        buttons: BUTTONS
            .iter()
            .filter(|(key, _)| held(*key))
            .fold(0, |all, (_, ours)| all | ours),
        left_trigger: trigger(key::LEFT_CONTROL),
        right_trigger: trigger(key::LEFT_SHIFT),
        left_x: axis(b'A', b'D'),
        left_y: axis(b'S', b'W'),
        right_x: axis(b'J', b'L'),
        right_y: axis(b'K', b'I'),
    }
}

/// One entry of a script: buttons held from one time to another, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Held {
    from: f32,
    to: f32,
    buttons: u32,
}

/// The button a script names, without regard to case.
fn named(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" => button::A,
        "b" => button::B,
        "x" => button::X,
        "y" => button::Y,
        "menu" => button::MENU,
        "view" => button::VIEW,
        "up" => button::UP,
        "down" => button::DOWN,
        "left" => button::LEFT,
        "right" => button::RIGHT,
        "lb" => button::LEFT_SHOULDER,
        "rb" => button::RIGHT_SHOULDER,
        "ls" => button::LEFT_STICK,
        "rs" => button::RIGHT_STICK,
        _ => return None,
    })
}

/// How long a script holds a button it gives one time for.
const TAP: f32 = 0.2;

/// Reads a script: entries separated by commas, each `<time>:<button>` or
/// `<from>-<to>:<button>`. An entry that cannot be read is an error naming it.
fn script(text: &str) -> Result<Vec<Held>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let wrong = || format!("`{entry}` is not `<time>:<button>` or `<from>-<to>:<button>`");
            let (when, name) = entry.split_once(':').ok_or_else(wrong)?;
            let buttons = named(name.trim()).ok_or_else(wrong)?;
            let time = |text: &str| text.trim().parse::<f32>().map_err(|_| wrong());
            let (from, to) = match when.split_once('-') {
                Some((from, to)) => (time(from)?, time(to)?),
                None => (time(when)?, time(when)? + TAP),
            };
            Ok(Held { from, to, buttons })
        })
        .collect()
}

/// What a script holds at `seconds` from the start.
fn scripted(script: &[Held], seconds: f32) -> Gamepad {
    Gamepad {
        buttons: script
            .iter()
            .filter(|held| held.from <= seconds && seconds < held.to)
            .fold(0, |all, held| all | held.buttons),
        ..Gamepad::default()
    }
}

/// One reading: what the gamepad held, when, and which reading of the run it is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Reading {
    sequence: u64,
    /// Microseconds since the run started.
    timestamp: u64,
    held: Gamepad,
}

/// How many readings are kept for a title to walk back and forth over.
const KEPT: usize = 256;

/// The readings kept, oldest first.
#[derive(Default)]
struct Readings {
    kept: VecDeque<Reading>,
}

impl Readings {
    /// Takes note of what is held now. A new reading is kept if it differs from the last,
    /// or if there is none yet.
    fn note(&mut self, timestamp: u64, held: Gamepad) {
        let last = self.kept.back();
        if last.is_some_and(|last| last.held == held) {
            return;
        }
        let sequence = last.map_or(1, |last| last.sequence + 1);
        if self.kept.len() == KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(Reading {
            sequence,
            timestamp,
            held,
        });
    }

    fn newest(&self) -> Option<Reading> {
        self.kept.back().copied()
    }

    /// The reading after the one numbered `sequence`, or before it.
    fn beside(&self, sequence: u64, after: bool) -> Option<Reading> {
        if after {
            self.kept
                .iter()
                .find(|one| one.sequence > sequence)
                .copied()
        } else {
            self.kept
                .iter()
                .rev()
                .find(|one| one.sequence < sequence)
                .copied()
        }
    }
}

/// When the run started, for timestamps and the script's times.
static STARTED: OnceLock<Instant> = OnceLock::new();

fn started() -> Instant {
    *STARTED.get_or_init(Instant::now)
}

/// Microseconds since the run started.
fn now() -> u64 {
    started().elapsed().as_micros() as u64
}

static READINGS: Mutex<Readings> = Mutex::new(Readings {
    kept: VecDeque::new(),
});

/// The script the run was given, read once.
fn the_script() -> &'static [Held] {
    static SCRIPT: OnceLock<Vec<Held>> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let Ok(text) = std::env::var("RIPTIDE_INPUT_SCRIPT") else {
            return Vec::new();
        };
        match script(&text) {
            Ok(script) => {
                trace!(
                    "input: a script of {} entries is played (RIPTIDE_INPUT_SCRIPT)",
                    script.len()
                );
                script
            }
            Err(why) => {
                trace!("input: RIPTIDE_INPUT_SCRIPT is not played: {why}");
                Vec::new()
            }
        }
    })
}

/// Looks at every source and keeps a reading if what is held has changed. Returns the
/// newest reading.
fn look() -> Reading {
    let timestamp = now();
    let held = scripted(the_script(), timestamp as f32 / 1e6)
        .with(host::controller().unwrap_or_default())
        .with(host::keyboard().unwrap_or_default());
    let mut readings = READINGS.lock().unwrap_or_else(|poison| poison.into_inner());
    readings.note(timestamp, held);
    readings.newest().expect("a reading was just noted")
}

/// What Windows reports: its controllers and its keyboard.
mod host {
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use super::{Gamepad, from_keys, from_xinput};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(key: i32) -> i16;
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowThreadProcessId(window: *mut c_void, process: *mut u32) -> u32;
    }

    /// XInput's state of one controller.
    #[repr(C)]
    #[derive(Default)]
    struct State {
        packet: u32,
        buttons: u16,
        triggers: [u8; 2],
        sticks: [i16; 4],
    }

    /// XInput's two functions, if this Windows has the library.
    struct XInput {
        get_state: unsafe extern "system" fn(u32, *mut State) -> u32,
        set_state: unsafe extern "system" fn(u32, *const [u16; 2]) -> u32,
    }

    fn xinput() -> Option<&'static XInput> {
        static XINPUT: OnceLock<Option<XInput>> = OnceLock::new();
        XINPUT
            .get_or_init(|| {
                let name: Vec<u16> = "xinput1_4.dll\0".encode_utf16().collect();
                // SAFETY: plain calls with terminated names; the library is Windows' own
                // and stays loaded.
                unsafe {
                    let module = LoadLibraryW(name.as_ptr());
                    if module.is_null() {
                        return None;
                    }
                    let get = GetProcAddress(module, c"XInputGetState".as_ptr().cast());
                    let set = GetProcAddress(module, c"XInputSetState".as_ptr().cast());
                    if get.is_null() || set.is_null() {
                        return None;
                    }
                    Some(XInput {
                        // SAFETY: the two functions have these signatures.
                        get_state: std::mem::transmute::<
                            *mut c_void,
                            unsafe extern "system" fn(u32, *mut State) -> u32,
                        >(get),
                        set_state: std::mem::transmute::<
                            *mut c_void,
                            unsafe extern "system" fn(u32, *const [u16; 2]) -> u32,
                        >(set),
                    })
                }
            })
            .as_ref()
    }

    /// Which of XInput's four places the controller was last found in, and when the places
    /// were last all found empty: asking about an empty place takes Windows a while, so
    /// they are not asked about again at once.
    struct Found {
        place: Option<u32>,
        empty_since: Option<Instant>,
    }

    static FOUND: Mutex<Found> = Mutex::new(Found {
        place: None,
        empty_since: None,
    });

    /// How long the places are left alone after all were found empty.
    const LOOK_AGAIN: Duration = Duration::from_secs(2);

    /// The first controller XInput reports, if there is one.
    pub(super) fn controller() -> Option<Gamepad> {
        let xinput = xinput()?;
        let mut found = FOUND.lock().unwrap_or_else(|poison| poison.into_inner());
        if found
            .empty_since
            .is_some_and(|since| since.elapsed() < LOOK_AGAIN)
        {
            return None;
        }
        let places = found.place.into_iter().chain(0..4);
        for place in places {
            let mut state = State::default();
            // SAFETY: a plain call that fills the state it is given.
            if unsafe { (xinput.get_state)(place, &mut state) } == 0 {
                found.place = Some(place);
                found.empty_since = None;
                return Some(from_xinput(state.buttons, state.triggers, state.sticks));
            }
        }
        found.place = None;
        found.empty_since = Some(Instant::now());
        None
    }

    /// Turns the controller's two motors, each from 0 to 1. Without a controller nothing
    /// happens.
    pub(super) fn rumble(low: f32, high: f32) {
        let Some(xinput) = xinput() else {
            return;
        };
        let place = FOUND
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .place;
        if let Some(place) = place {
            let speed = |level: f32| (level.clamp(0.0, 1.0) * 65535.0) as u16;
            // SAFETY: a plain call with two speeds that outlive it.
            unsafe { (xinput.set_state)(place, &[speed(low), speed(high)]) };
        }
    }

    /// The keyboard as a gamepad, while a window of this process is in front. Keys typed
    /// into another program are not the title's.
    pub(super) fn keyboard() -> Option<Gamepad> {
        // SAFETY: plain calls; the process id is written to a local.
        let ours = unsafe {
            let window = GetForegroundWindow();
            let mut process = 0;
            !window.is_null()
                && GetWindowThreadProcessId(window, &mut process) != 0
                && process == GetCurrentProcessId()
        };
        // SAFETY: a plain call per key; the top bit says the key is down now.
        ours.then(|| from_keys(|key| unsafe { GetAsyncKeyState(i32::from(key)) } < 0))
    }
}

/// What a device says of itself, in the library's layout. A title tells devices apart by
/// the id; the counts and the pointers to detail after the four words that follow it are
/// zero, which is "none" for each.
#[repr(C)]
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

/// The one gamepad's id: a name, so that a session log that shows it says whose it is.
const ID: [u8; 32] = *b"riptide gamepad one\0\0\0\0\0\0\0\0\0\0\0\0\0";

static INFO: DeviceInfo = DeviceInfo {
    size: size_of::<DeviceInfo>() as u32,
    // An Xbox controller's vendor and product, which a title may look for.
    vendor: 0x045e,
    product: 0x0b12,
    revision: 0,
    interface: 0,
    collection: 0,
    // A game pad among generic desktop controls.
    usage: [1, 5],
    versions: [0; 8],
    id: ID,
    root_id: ID,
    // An Xbox One family controller.
    family: 2,
    capabilities: 0,
    supported_input: KIND_GAMEPAD,
    // The low and the high frequency motor.
    rumble_motors: 0x3,
    rest: [0; 26],
};

/// The input object, made once.
fn input() -> usize {
    static INPUT: OnceLock<usize> = OnceLock::new();
    *INPUT.get_or_init(|| {
        com::singleton(
            "IGameInput",
            22,
            &[
                (0, no_interface as *const () as usize),
                (1, com::keep as *const () as usize),
                (2, com::keep as *const () as usize),
                (3, current_timestamp as *const () as usize),
                (4, current_reading as *const () as usize),
                (5, next_reading as *const () as usize),
                (6, previous_reading as *const () as usize),
                (7, temporal_reading as *const () as usize),
                (8, register_reading_callback as *const () as usize),
                (9, register_device_callback as *const () as usize),
                (10, com::register_for_nothing as *const () as usize),
                (11, com::register_for_nothing as *const () as usize),
                (12, stop_callback as *const () as usize),
                (13, unregister_callback as *const () as usize),
                (16, find_device_from_id as *const () as usize),
                (21, set_focus_policy as *const () as usize),
            ],
        )
    })
}

/// The one gamepad, made once.
fn device() -> usize {
    static DEVICE: OnceLock<usize> = OnceLock::new();
    *DEVICE.get_or_init(|| {
        com::singleton(
            "IGameInputDevice",
            22,
            &[
                (0, no_interface as *const () as usize),
                (1, com::keep as *const () as usize),
                (2, com::keep as *const () as usize),
                (3, device_info as *const () as usize),
                (4, device_status as *const () as usize),
                (5, battery_state as *const () as usize),
                (10, set_rumble_state as *const () as usize),
            ],
        )
    })
}

/// The table of a reading's methods.
fn reading_table() -> &'static [usize] {
    static TABLE: OnceLock<&'static [usize]> = OnceLock::new();
    TABLE.get_or_init(|| {
        com::table(
            "IGameInputReading",
            25,
            &[
                (0, no_interface as *const () as usize),
                (1, Instance::<Reading>::add_ref as *const () as usize),
                (2, Instance::<Reading>::release as *const () as usize),
                (3, reading_kind as *const () as usize),
                (4, reading_sequence as *const () as usize),
                (5, reading_timestamp as *const () as usize),
                (6, reading_device as *const () as usize),
                (22, reading_gamepad as *const () as usize),
            ],
        )
    })
}

/// `QueryInterface` of any of the three: none has another interface.
///
/// # Safety
/// `out` is null or where an interface pointer goes.
unsafe extern "system" fn no_interface(
    _this: *mut c_void,
    _interface: *const [u8; 16],
    out: *mut usize,
) -> i32 {
    trace!("input: another interface of an input object is asked for, and there is none");
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = 0;
    }
    E_NOINTERFACE
}

/// Hands the title `reading` as an object of its own, or says there is none.
///
/// # Safety
/// `out` is null or where a reading pointer goes.
unsafe fn hand(reading: Option<Reading>, kind: u32, out: *mut usize) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    match reading.filter(|_| kind & KIND_GAMEPAD != 0) {
        Some(reading) => {
            *out = Instance::create(reading_table(), reading) as usize;
            S_OK
        }
        None => {
            *out = 0;
            READING_NOT_FOUND
        }
    }
}

/// Whether `asked` is a device this library has: none named, or the one gamepad.
fn ours(asked: usize) -> bool {
    asked == 0 || asked == device()
}

/// `GetCurrentTimestamp`: microseconds on the clock readings are stamped by.
extern "system" fn current_timestamp(_this: usize) -> u64 {
    now()
}

/// `GetCurrentReading`: `(kinds, device or null, reading out)`.
///
/// # Safety
/// `out` is null or where a reading pointer goes.
unsafe extern "system" fn current_reading(
    _this: usize,
    kind: u32,
    asked: usize,
    out: *mut usize,
) -> i32 {
    show_first_read(kind);
    let reading = ours(asked).then(look);
    // SAFETY: as this function's contract says.
    unsafe { hand(reading, kind, out) }
}

/// Says once that the title reads input, and of which kinds.
fn show_first_read(kind: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if !SHOWN.swap(true, Ordering::Relaxed) {
        trace!("input: the title reads input of kinds {kind:#x}; it has one gamepad");
    }
}

/// The reading beside `reference`: `(reference, kinds, device or null, reading out)`.
///
/// # Safety
/// `reference` is null or a live reading of this library, and `out` is null or where a
/// reading pointer goes.
unsafe fn beside(
    reference: *const Instance<Reading>,
    kind: u32,
    asked: usize,
    out: *mut usize,
    after: bool,
) -> i32 {
    // SAFETY: the title's reading, null or live.
    let Some(reference) = (unsafe { reference.as_ref() }) else {
        return E_INVALIDARG;
    };
    look();
    let reading = READINGS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .beside(reference.state.sequence, after)
        .filter(|_| ours(asked));
    // SAFETY: as this function's contract says.
    unsafe { hand(reading, kind, out) }
}

/// `GetNextReading`.
///
/// # Safety
/// As [`beside`].
unsafe extern "system" fn next_reading(
    _this: usize,
    reference: *const Instance<Reading>,
    kind: u32,
    asked: usize,
    out: *mut usize,
) -> i32 {
    // SAFETY: as this function's contract says.
    unsafe { beside(reference, kind, asked, out, true) }
}

/// `GetPreviousReading`.
///
/// # Safety
/// As [`beside`].
unsafe extern "system" fn previous_reading(
    _this: usize,
    reference: *const Instance<Reading>,
    kind: u32,
    asked: usize,
    out: *mut usize,
) -> i32 {
    // SAFETY: as this function's contract says.
    unsafe { beside(reference, kind, asked, out, false) }
}

/// `GetTemporalReading`: `(timestamp, device, reading out)`. The reading that held at a
/// time: the last one taken at or before it, or the oldest kept.
///
/// # Safety
/// `out` is null or where a reading pointer goes.
unsafe extern "system" fn temporal_reading(
    _this: usize,
    timestamp: u64,
    asked: usize,
    out: *mut usize,
) -> i32 {
    look();
    let reading = {
        let readings = READINGS.lock().unwrap_or_else(|poison| poison.into_inner());
        readings
            .kept
            .iter()
            .rev()
            .find(|one| one.timestamp <= timestamp)
            .or(readings.kept.front())
            .copied()
            .filter(|_| ours(asked))
    };
    // SAFETY: as this function's contract says.
    unsafe { hand(reading, KIND_GAMEPAD, out) }
}

/// The next token a registration is given.
fn token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// `RegisterReadingCallback`: `(device, kinds, threshold, context, callback, token out)`.
/// The callback is never called: a title that wants readings pushed at it is told so in
/// the session log, once.
///
/// # Safety
/// `out` is null or where the token goes.
unsafe extern "system" fn register_reading_callback(
    _this: usize,
    _device: usize,
    kind: u32,
    _threshold: f32,
    _context: usize,
    _callback: usize,
    out: *mut u64,
) -> i32 {
    trace!("input: a callback for readings of kinds {kind:#x} is registered and never called");
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = token();
    }
    S_OK
}

/// What a title is called with when a device comes or goes: `(token, context, device,
/// timestamp, status now, status before)`.
type DeviceCallback = unsafe extern "system" fn(u64, usize, usize, u64, u32, u32);

/// `RegisterDeviceCallback`: `(device or null, kinds, statuses, enumeration, context,
/// callback, token out)`. The one gamepad never comes or goes, so the callback is called
/// only to list it, if the title asks for the devices already there: before this returns
/// (enumeration 2) or soon after on another thread (1).
///
/// # Safety
/// `callback` is null or a function of the title's with [`DeviceCallback`]'s signature,
/// and `out` is null or where the token goes.
unsafe extern "system" fn register_device_callback(
    _this: usize,
    asked: usize,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: usize,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> i32 {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let token = token();
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = token;
    }
    let listed = enumeration != 0
        && ours(asked)
        && kind & KIND_GAMEPAD != 0
        && status & STATUS_CONNECTED != 0;
    trace!(
        "input: a device callback is registered (kinds {kind:#x}, statuses {status:#x}, \
         enumeration {enumeration}); the gamepad is {}",
        if listed { "listed to it" } else { "not listed" }
    );
    if listed {
        let list = move || {
            // SAFETY: the title's callback, with what the library passes one.
            unsafe { callback(token, context, device(), now(), STATUS_CONNECTED, 0) };
        };
        if enumeration == 2 {
            list();
        } else {
            crate::pool::run(list);
        }
    }
    S_OK
}

/// `StopCallback`: `(token)`. No callback is called after it is registered, so there is
/// nothing to stop.
extern "system" fn stop_callback(_this: usize, _token: u64) {}

/// `UnregisterCallback`: `(token, how long to wait)`. Done at once.
extern "system" fn unregister_callback(_this: usize, _token: u64, _wait: u64) -> bool {
    true
}

/// `FindDeviceFromId`: `(id, device out)`.
///
/// # Safety
/// `id` is null or 32 readable bytes, and `out` is null or where a device pointer goes.
unsafe extern "system" fn find_device_from_id(
    _this: usize,
    id: *const [u8; 32],
    out: *mut usize,
) -> i32 {
    // SAFETY: the title's pointers; null is refused.
    let (Some(id), Some(out)) = (unsafe { id.as_ref() }, unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    if *id == ID {
        *out = device();
        S_OK
    } else {
        *out = 0;
        DEVICE_NOT_FOUND
    }
}

/// `SetFocusPolicy`: `(policy)`. Which input a title gets while it is not in front is
/// decided here: the controller's always, the keyboard's never.
extern "system" fn set_focus_policy(_this: usize, policy: u32) {
    trace!("input: the title sets focus policy {policy:#x}, which is not followed");
}

/// `GetDeviceInfo`.
extern "system" fn device_info(_this: usize) -> *const DeviceInfo {
    &raw const INFO
}

/// `GetDeviceStatus`.
extern "system" fn device_status(_this: usize) -> u32 {
    STATUS_CONNECTED
}

/// `GetBatteryState`: `(state out)`. Five words, all zero: no battery to speak of.
///
/// # Safety
/// `out` is null or where the five words go.
unsafe extern "system" fn battery_state(_this: usize, out: *mut [u32; 5]) {
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = [0; 5];
    }
}

/// `SetRumbleState`: `(four levels from 0 to 1, or null for none)`: the low and the high
/// frequency motor, then the two triggers', which the host's controllers are not asked
/// for.
///
/// # Safety
/// `levels` is null or four readable floats.
unsafe extern "system" fn set_rumble_state(_this: usize, levels: *const [f32; 4]) {
    // SAFETY: the title's pointer, null or readable.
    let levels = unsafe { levels.as_ref() }.copied().unwrap_or_default();
    host::rumble(levels[0], levels[1]);
}

/// `GetInputKind`.
extern "system" fn reading_kind(_this: usize) -> u32 {
    KIND_GAMEPAD
}

/// `GetSequenceNumber`: `(kind)`.
///
/// # Safety
/// `this` is a live reading of this library.
unsafe extern "system" fn reading_sequence(this: *const Instance<Reading>, _kind: u32) -> u64 {
    // SAFETY: as this function's contract says.
    unsafe { &*this }.state.sequence
}

/// `GetTimestamp`.
///
/// # Safety
/// `this` is a live reading of this library.
unsafe extern "system" fn reading_timestamp(this: *const Instance<Reading>) -> u64 {
    // SAFETY: as this function's contract says.
    unsafe { &*this }.state.timestamp
}

/// `GetDevice`: `(device out)`.
///
/// # Safety
/// `out` is null or where a device pointer goes.
unsafe extern "system" fn reading_device(_this: usize, out: *mut usize) {
    // SAFETY: the title's out-pointer, null or writable.
    if let Some(out) = unsafe { out.as_mut() } {
        *out = device();
    }
}

/// `GetGamepadState`: `(state out)`. Whether the reading has one, which it always has.
///
/// # Safety
/// `this` is a live reading of this library, and `out` is null or where a state goes.
unsafe extern "system" fn reading_gamepad(
    this: *const Instance<Reading>,
    out: *mut Gamepad,
) -> bool {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return false;
    };
    // SAFETY: as this function's contract says.
    *out = unsafe { &*this }.state.held;
    true
}

/// `GameInputCreate`: `(input out)`. The library's one function, which titles import by
/// its number, 1.
///
/// # Safety
/// `out` is null or where the input object's pointer goes.
unsafe extern "system" fn game_input_create(out: *mut usize) -> i32 {
    // SAFETY: the title's out-pointer; null is refused.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return E_INVALIDARG;
    };
    *out = input();
    trace!(
        "input: the title has the input object; one gamepad, from a controller, the keyboard and the script"
    );
    S_OK
}

/// Binds the library's function, by name and by number, and starts the run's clock.
pub(crate) fn register(imports: &mut Imports) {
    started();
    for symbol in [
        Symbol::Name("GameInputCreate".to_owned()),
        Symbol::Ordinal(1),
    ] {
        // SAFETY: the function has the signature of the library's.
        unsafe {
            imports.implement(
                "gameinput.dll",
                symbol,
                game_input_create as *const () as usize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_controller_from_the_host_is_the_same_buttons_and_axes_from_minus_one_to_one() {
        let pad = from_xinput(
            0x1000 | 0x0001 | 0x0200,
            [255, 0],
            [32767, -32768, 0, 16384],
        );
        assert_eq!(pad.buttons, button::A | button::UP | button::RIGHT_SHOULDER);
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 0.0));
        assert_eq!((pad.left_x, pad.left_y, pad.right_x), (1.0, -1.0, 0.0));
        assert!((pad.right_y - 0.5).abs() < 1e-4);
        assert_eq!(from_xinput(0, [0; 2], [0; 4]), Gamepad::default());
    }

    #[test]
    fn keys_held_are_the_gamepad_the_table_says() {
        let held = |keys: &'static [u8]| move |key: u8| keys.contains(&key);
        let pad = from_keys(held(&[key::ENTER, b'W', b'D', key::LEFT_SHIFT, key::DOWN]));
        assert_eq!(pad.buttons, button::A | button::DOWN);
        assert_eq!((pad.left_x, pad.left_y), (1.0, 1.0));
        assert_eq!((pad.left_trigger, pad.right_trigger), (0.0, 1.0));
        // Two keys that push a stick opposite ways leave it at rest.
        assert_eq!(from_keys(held(b"AD")).left_x, 0.0);
        assert_eq!(from_keys(held(&[])), Gamepad::default());
    }

    #[test]
    fn sources_held_together_are_every_button_and_the_axis_further_from_rest() {
        let one = Gamepad {
            buttons: button::A,
            left_x: -0.5,
            right_trigger: 0.25,
            ..Gamepad::default()
        };
        let other = Gamepad {
            buttons: button::B,
            left_x: 0.25,
            right_trigger: 1.0,
            ..Gamepad::default()
        };
        let both = one.with(other);
        assert_eq!(both.buttons, button::A | button::B);
        assert_eq!((both.left_x, both.right_trigger), (-0.5, 1.0));
    }

    #[test]
    fn a_script_holds_its_buttons_between_its_times() {
        let played = script("95:A, 100-101.5:down,").unwrap();
        assert_eq!(
            played,
            [
                Held {
                    from: 95.0,
                    to: 95.0 + TAP,
                    buttons: button::A
                },
                Held {
                    from: 100.0,
                    to: 101.5,
                    buttons: button::DOWN
                },
            ]
        );
        assert_eq!(scripted(&played, 94.9).buttons, 0);
        assert_eq!(scripted(&played, 95.1).buttons, button::A);
        assert_eq!(scripted(&played, 95.3).buttons, 0);
        assert_eq!(scripted(&played, 101.0).buttons, button::DOWN);
        assert_eq!(scripted(&played, 101.5).buttons, 0);
        // What cannot be read is named.
        assert!(script("95:jump").unwrap_err().contains("95:jump"));
        assert!(script("soon:A").is_err());
        assert!(script("A").is_err());
    }

    #[test]
    fn a_reading_is_kept_for_each_change_and_walked_in_order() {
        let mut readings = Readings::default();
        let a = Gamepad {
            buttons: button::A,
            ..Gamepad::default()
        };
        readings.note(10, Gamepad::default());
        readings.note(20, Gamepad::default());
        readings.note(30, a);
        readings.note(40, a);
        readings.note(50, Gamepad::default());
        // Three changes, counting the first reading there was.
        let sequences: Vec<u64> = readings.kept.iter().map(|one| one.sequence).collect();
        assert_eq!(sequences, [1, 2, 3]);
        assert_eq!(readings.newest().unwrap().timestamp, 50);
        assert_eq!(readings.beside(1, true).unwrap().held, a);
        assert_eq!(readings.beside(3, true), None);
        assert_eq!(readings.beside(3, false).unwrap().sequence, 2);
        assert_eq!(readings.beside(1, false), None);
        // No more than a fixed number are kept, the oldest going first.
        for time in 0..2 * KEPT as u64 {
            readings.note(
                100 + time,
                if time % 2 == 0 { a } else { Gamepad::default() },
            );
        }
        assert_eq!(readings.kept.len(), KEPT);
    }

    #[test]
    fn the_title_reads_the_gamepad_through_the_objects() {
        // SAFETY: the methods are called as a title calls them, with pointers to locals.
        unsafe {
            let mut input = 0usize;
            assert_eq!(game_input_create(&mut input), S_OK);
            assert_eq!(game_input_create(std::ptr::null_mut()), E_INVALIDARG);
            let mut reading = 0usize;
            assert_eq!(current_reading(input, KIND_GAMEPAD, 0, &mut reading), S_OK);
            let reading = reading as *mut Instance<Reading>;
            let mut state = Gamepad {
                buttons: 0xffff,
                ..Gamepad::default()
            };
            assert!(reading_gamepad(reading, &mut state));
            assert_eq!(state, (*reading).state.held);
            assert_eq!(reading_kind(reading as usize), KIND_GAMEPAD);
            let mut owner = 0usize;
            reading_device(reading as usize, &mut owner);
            assert_eq!(owner, device());
            assert_eq!(device_status(owner), STATUS_CONNECTED);
            assert_eq!((*device_info(owner)).supported_input, KIND_GAMEPAD);
            // The gamepad is found by its id, and no other device is.
            let mut found = 0usize;
            assert_eq!(find_device_from_id(input, &ID, &mut found), S_OK);
            assert_eq!(found, owner);
            assert_eq!(
                find_device_from_id(input, &[7; 32], &mut found),
                DEVICE_NOT_FOUND
            );
            // Another kind of input has no reading, and neither has another device.
            let mut other = 1usize;
            assert_eq!(
                current_reading(input, 0x10, 0, &mut other),
                READING_NOT_FOUND
            );
            assert_eq!(other, 0);
            assert_eq!(
                current_reading(input, KIND_GAMEPAD, 0x1234, &mut other),
                READING_NOT_FOUND
            );
            assert_eq!(Instance::<Reading>::release(reading), 0);
        }
    }

    #[test]
    fn a_device_callback_that_asks_for_the_devices_there_is_called_with_the_gamepad() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn seen(
            _token: u64,
            context: usize,
            device: usize,
            _timestamp: u64,
            now: u32,
            before: u32,
        ) {
            assert_eq!((context, now, before), (0x77, STATUS_CONNECTED, 0));
            SEEN.store(device, Ordering::Relaxed);
        }
        // SAFETY: the method is called as a title calls it.
        unsafe {
            let mut token = 0;
            let result = register_device_callback(
                input(),
                0,
                KIND_GAMEPAD,
                STATUS_CONNECTED,
                2,
                0x77,
                Some(seen),
                &mut token,
            );
            assert_eq!(result, S_OK);
            assert_ne!(token, 0);
            assert_eq!(SEEN.load(Ordering::Relaxed), device());
            // Without a callback there is nothing to register.
            assert_eq!(
                register_device_callback(input(), 0, KIND_GAMEPAD, 1, 2, 0, None, &mut token),
                E_INVALIDARG
            );
        }
    }
}
