//! Shared application state and inter-task communication channels.
//!
//! The display loop runs on the APP core and only reads atomics / takes signals,
//! everything else runs on the Embassy executor on the PRO core.

use core::cell::RefCell;
use core::sync::atomic::Ordering;

use alloc::vec::Vec;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use portable_atomic::{AtomicBool, AtomicU32};

/// Sample rate of the codecs and of the audio exchanged with the playback buffer.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Display mode: boot-time console log view or animated visuals.
#[repr(u32)]
pub enum Mode {
    Console = 0,
    Visuals = 1,
}

/// Conversation state, drives the UI.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(u32)]
pub enum ConvState {
    Sleeping = 0,
    Connecting = 1,
    Listening = 2,
    Speaking = 3,
    Remembering = 4,
    Error = 5,
}

impl ConvState {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Connecting,
            2 => Self::Listening,
            3 => Self::Speaking,
            4 => Self::Remembering,
            5 => Self::Error,
            _ => Self::Sleeping,
        }
    }
}

/// Facial expression requested by the model via the `set_mood` tool.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Expression {
    Neutral,
    Happy,
    Sad,
    Angry,
    Curious,
}

impl Expression {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "neutral" => Self::Neutral,
            "happy" => Self::Happy,
            "sad" => Self::Sad,
            "angry" => Self::Angry,
            "curious" => Self::Curious,
            _ => return None,
        })
    }
}

pub static MODE: AtomicU32 = AtomicU32::new(Mode::Console as u32);

static CONV_STATE: AtomicU32 = AtomicU32::new(ConvState::Sleeping as u32);

pub fn set_state(state: ConvState) {
    CONV_STATE.store(state as u32, Ordering::SeqCst);
}

pub fn state() -> ConvState {
    ConvState::from_u32(CONV_STATE.load(Ordering::SeqCst))
}

/// Fired by the touch task on every (debounced) tap on the display.
pub static TAP: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// While true, the audio task streams microphone chunks into [`MIC_CHANNEL`].
pub static MIC_ENABLED: AtomicBool = AtomicBool::new(false);

/// 100 ms chunks of 16 kHz mono little-endian PCM from the microphone.
pub static MIC_CHANNEL: Channel<CriticalSectionRawMutex, Vec<u8>, 8> = Channel::new();

/// True while there is model speech left in the playback buffer.
pub static MODEL_SPEAKING: AtomicBool = AtomicBool::new(false);

/// Peak level (0..=32767) of the audio currently played, for the talking animation.
pub static SPEECH_LEVEL: AtomicU32 = AtomicU32::new(0);

/// Expression changes requested by the model.
pub static EXPRESSION: Signal<CriticalSectionRawMutex, Expression> = Signal::new();

/// Capacity of the playback buffer in samples (20 s).
pub const PLAYBACK_CAPACITY: usize = SAMPLE_RATE_HZ as usize * 20;

/// Ring buffer of 16 kHz mono samples waiting to be played.
pub struct PlaybackBuffer {
    buf: Vec<i16>,
    read: usize,
    len: usize,
}

impl PlaybackBuffer {
    const fn new() -> Self {
        Self {
            buf: Vec::new(),
            read: 0,
            len: 0,
        }
    }

    fn ensure_allocated(&mut self) {
        if self.buf.is_empty() {
            self.buf = alloc::vec![0; PLAYBACK_CAPACITY];
        }
    }

    /// Appends as many samples as fit and returns how many were taken.
    pub fn push(&mut self, samples: &[i16]) -> usize {
        self.ensure_allocated();
        let cap = self.buf.len();
        let count = samples.len().min(cap - self.len);
        for (i, sample) in samples[..count].iter().enumerate() {
            self.buf[(self.read + self.len + i) % cap] = *sample;
        }
        self.len += count;
        count
    }

    /// Fills `out` with buffered samples and returns how many were written.
    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        if self.len == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let count = out.len().min(self.len);
        for (i, sample) in out[..count].iter_mut().enumerate() {
            *sample = self.buf[(self.read + i) % cap];
        }
        self.read = (self.read + count) % cap;
        self.len -= count;
        count
    }

    pub fn clear(&mut self) {
        self.read = 0;
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn free(&self) -> usize {
        PLAYBACK_CAPACITY - self.len
    }
}

pub static PLAYBACK: Mutex<CriticalSectionRawMutex, RefCell<PlaybackBuffer>> =
    Mutex::new(RefCell::new(PlaybackBuffer::new()));

/// Allocates the playback buffer up front instead of on first use inside a critical section.
pub fn init_playback() {
    let mut buf = alloc::vec![0; PLAYBACK_CAPACITY];
    PLAYBACK.lock(|playback| core::mem::swap(&mut playback.borrow_mut().buf, &mut buf));
}
