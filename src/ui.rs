//! Display loop running on the second CPU core.
//!
//! Initializes the ILI9341 panel over SPI+DMA and renders either a scrolling
//! log console (during boot) or the companion's face depending on
//! [`crate::app::ConvState`]: a snoozing face while asleep and animated
//! [`RoboEyes`] during a conversation.

use core::cell::RefCell;

use crate::app::{self, ConvState, Expression, Mode, EXPRESSION, MODE, SPEECH_LEVEL};
use crate::roboeyes::{Mood, Position, RoboEyes};
use crate::spi_dma_displayinterface;
use alloc::string::String;
use alloc::vec::Vec;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embedded_graphics::draw_target::{DrawTarget, DrawTargetExt};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, RgbColor};
use embedded_graphics::primitives::{Line, Polyline, PrimitiveStyle};
use embedded_graphics::text::Text;
use embedded_graphics::Drawable;
use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::{DriveMode, Level, Output, OutputConfig};
use esp_hal::spi::master::Spi;
use esp_hal::time::{Duration, Instant, Rate};
use mipidsi::Builder;

const DISPLAY_WIDTH: usize = 320;
const DISPLAY_HEIGHT: usize = 240;

const EYE_COLOR: Rgb565 = Rgb565::new(18, 45, 25);
const SLEEP_COLOR: Rgb565 = Rgb565::new(5, 22, 14);
/// Expressions requested by the model fade back to neutral after this long.
const EXPRESSION_DURATION: Duration = Duration::from_secs(8);

pub static LOG_BUFFER: Mutex<CriticalSectionRawMutex, RefCell<Vec<String>>> =
    Mutex::new(RefCell::new(Vec::new()));

/// Runs the display loop. Intended to be started on the second CPU core.
pub fn run_ui() {
    let sclk = unsafe { esp_hal::peripherals::GPIO7::steal() };
    let mosi = unsafe { esp_hal::peripherals::GPIO6::steal() };
    let cs = unsafe { esp_hal::peripherals::GPIO5::steal() };
    let dc = Output::new(
        unsafe { esp_hal::peripherals::GPIO4::steal() },
        Level::Low,
        OutputConfig::default(),
    );
    let mut gpio_backlight = Output::new(
        unsafe { esp_hal::peripherals::GPIO47::steal() },
        Level::Low,
        OutputConfig::default(),
    );
    let rst = Output::new(
        unsafe { esp_hal::peripherals::GPIO48::steal() },
        Level::High,
        OutputConfig::default().with_drive_mode(DriveMode::OpenDrain),
    );

    let spi = Spi::new(
        unsafe { esp_hal::peripherals::SPI2::steal() },
        esp_hal::spi::master::Config::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(esp_hal::spi::Mode::_0),
    )
    .unwrap()
    .with_sck(sclk)
    .with_mosi(mosi)
    .with_cs(cs)
    .with_dma(unsafe { esp_hal::peripherals::DMA_CH0::steal() });

    let mut delay = Delay::new();
    gpio_backlight.set_high();

    let di = spi_dma_displayinterface::new_no_cs(DISPLAY_WIDTH * DISPLAY_HEIGHT * 2, spi, dc);

    // ESP32-S3-BOX display power-up delay: too short → fuzzy image, none → blank panel.
    delay.delay_millis(500);

    let mut display = Builder::new(mipidsi::models::ILI9341Rgb565, di)
        .display_size(240, 320)
        .orientation(
            mipidsi::options::Orientation::new()
                .flip_vertical()
                .flip_horizontal(),
        )
        .color_order(mipidsi::options::ColorOrder::Bgr)
        .reset_pin(rst)
        .init(&mut delay)
        .unwrap();

    let mut buffer = alloc::boxed::Box::new([Rgb565::new(0, 0, 0); DISPLAY_WIDTH * DISPLAY_HEIGHT]);
    let mut fbuf =
        embedded_graphics_framebuf::FrameBuf::new(buffer.as_mut(), DISPLAY_WIDTH, DISPLAY_HEIGHT);

    let text_style = MonoTextStyle::new(
        &embedded_graphics::mono_font::ascii::FONT_6X10,
        Rgb565::WHITE,
    );

    let mut robo_eyes = RoboEyes::new(DISPLAY_WIDTH as i32 - 10, DISPLAY_HEIGHT as i32);
    robo_eyes.set_colors(Rgb565::BLACK, EYE_COLOR);

    let time = Instant::now();
    let mut effect_frame = 0u32;
    let mut prev_state: Option<ConvState> = None;
    let mut expression_set_at: Option<Instant> = None;
    let mut talk_offset = 0i32;

    loop {
        let state = app::state();
        let millis = time.elapsed().as_millis();

        if prev_state != Some(state) {
            enter_state(&mut robo_eyes, state);
            expression_set_at = None;
            prev_state = Some(state);
        }

        if let Some(expression) = EXPRESSION.try_take() {
            if matches!(state, ConvState::Listening | ConvState::Speaking) {
                apply_expression(&mut robo_eyes, expression);
                expression_set_at = Some(Instant::now());
            }
        }
        if expression_set_at.is_some_and(|at| at.elapsed() > EXPRESSION_DURATION) {
            apply_expression(&mut robo_eyes, Expression::Neutral);
            expression_set_at = None;
        }

        if MODE.load(core::sync::atomic::Ordering::SeqCst) == Mode::Visuals as u32 {
            fbuf.clear(Rgb565::BLACK).unwrap();

            if state == ConvState::Sleeping {
                draw_snooze(&mut fbuf, millis);
            } else {
                let target_offset = if state == ConvState::Speaking {
                    let level = SPEECH_LEVEL.load(core::sync::atomic::Ordering::Relaxed) as i32;
                    -(level.min(16_000) * 10 / 16_000)
                } else {
                    0
                };
                talk_offset = (talk_offset * 2 + target_offset) / 3;

                robo_eyes.update(millis);
                robo_eyes
                    .draw(&mut fbuf.translated(Point::new(0, talk_offset)))
                    .unwrap();

                let caption = match state {
                    ConvState::Connecting => Some("> waking up..."),
                    ConvState::Remembering => Some("> remembering..."),
                    ConvState::Error => Some("> something went wrong :("),
                    _ => None,
                };
                if let Some(caption) = caption {
                    Text::new(caption, Point::new(30, 230), text_style)
                        .draw(&mut fbuf)
                        .unwrap();
                }
            }

            scanline_effects(fbuf.data.as_mut(), effect_frame);
            effect_frame = effect_frame.wrapping_add(1);
        } else {
            fbuf.clear(Rgb565::BLACK).unwrap();
            LOG_BUFFER.lock(|buf| {
                for (i, line) in buf.borrow().iter().enumerate() {
                    Text::new(line, Point::new(0, 8 + 10 * i as i32), text_style)
                        .draw(&mut fbuf)
                        .unwrap();
                }
            });
        }

        display
            .set_pixels(
                0,
                0,
                DISPLAY_WIDTH as u16,
                DISPLAY_HEIGHT as u16,
                fbuf.data.as_mut().iter().cloned(),
            )
            .unwrap();
    }
}

fn enter_state(robo_eyes: &mut RoboEyes, state: ConvState) {
    match state {
        ConvState::Sleeping => {
            robo_eyes.set_autoblinker(false, 2, 2);
            robo_eyes.set_idle_mode(false, 3, 2);
            robo_eyes.set_mood(Mood::Default);
            robo_eyes.set_colors(Rgb565::BLACK, EYE_COLOR);
            robo_eyes.set_position(Position::Center);
            robo_eyes.close();
        }
        ConvState::Connecting => {
            robo_eyes.set_autoblinker(false, 2, 2);
            robo_eyes.set_idle_mode(false, 3, 2);
            robo_eyes.set_position(Position::Center);
            robo_eyes.open();
            robo_eyes.anim_confused();
        }
        ConvState::Listening => {
            robo_eyes.open();
            robo_eyes.set_autoblinker(true, 2, 3);
            robo_eyes.set_idle_mode(true, 3, 3);
        }
        ConvState::Speaking => {
            robo_eyes.open();
            robo_eyes.set_autoblinker(true, 3, 3);
            robo_eyes.set_idle_mode(false, 3, 2);
            robo_eyes.set_position(Position::Center);
        }
        ConvState::Remembering => {
            robo_eyes.set_autoblinker(true, 3, 2);
            robo_eyes.set_idle_mode(false, 3, 2);
            robo_eyes.set_mood(Mood::Default);
            robo_eyes.set_colors(Rgb565::BLACK, EYE_COLOR);
            robo_eyes.set_position(Position::NE);
        }
        ConvState::Error => {
            robo_eyes.set_idle_mode(false, 3, 2);
            robo_eyes.set_colors(Rgb565::BLACK, Rgb565::new(29, 18, 2));
            robo_eyes.set_mood(Mood::Tired);
            robo_eyes.set_position(Position::S);
        }
    }
}

fn apply_expression(robo_eyes: &mut RoboEyes, expression: Expression) {
    match expression {
        Expression::Neutral => {
            robo_eyes.set_colors(Rgb565::BLACK, EYE_COLOR);
            robo_eyes.set_mood(Mood::Default);
        }
        Expression::Happy => {
            robo_eyes.set_colors(Rgb565::BLACK, Rgb565::new(23, 59, 7));
            robo_eyes.set_mood(Mood::Happy);
            robo_eyes.anim_laugh();
        }
        Expression::Sad => {
            robo_eyes.set_colors(Rgb565::BLACK, Rgb565::new(14, 5, 29));
            robo_eyes.set_mood(Mood::Tired);
        }
        Expression::Angry => {
            robo_eyes.set_colors(Rgb565::BLACK, Rgb565::new(29, 18, 2));
            robo_eyes.set_mood(Mood::Angry);
        }
        Expression::Curious => {
            robo_eyes.set_colors(Rgb565::BLACK, Rgb565::new(10, 50, 31));
            robo_eyes.set_mood(Mood::Default);
            robo_eyes.anim_confused();
        }
    }
}

/// Sleeping face: closed eyes that slowly "breathe" and Zs drifting up.
fn draw_snooze<D: DrawTarget<Color = Rgb565>>(target: &mut D, millis: u64) {
    const BREATH_PERIOD_MS: u64 = 4000;
    const Z_PERIOD_MS: u64 = 3600;

    let phase = (millis % BREATH_PERIOD_MS) as f32 / BREATH_PERIOD_MS as f32;
    let breath = sin_turns(phase);
    let bob = (breath * 5.0) as i32;
    let brightness = 0.75 + 0.25 * breath;

    let eye_style = PrimitiveStyle::with_stroke(scale_color(SLEEP_COLOR, brightness), 7);
    let center_y = DISPLAY_HEIGHT as i32 / 2 + 15 + bob;
    for center_x in [DISPLAY_WIDTH as i32 / 2 - 55, DISPLAY_WIDTH as i32 / 2 + 45] {
        let mut points = [Point::zero(); 9];
        for (i, point) in points.iter_mut().enumerate() {
            let dx = (i as i32 - 4) * 8;
            let depth = 12 - 12 * dx * dx / (32 * 32);
            *point = Point::new(center_x + dx, center_y + depth);
        }
        Polyline::new(&points)
            .into_styled(eye_style)
            .draw(target)
            .ok();
    }

    for i in 0..3u64 {
        let t = ((millis + i * Z_PERIOD_MS / 3) % Z_PERIOD_MS) as f32 / Z_PERIOD_MS as f32;
        let size = 8 + (t * 18.0) as i32;
        let sway = (sin_turns(t * 1.5) * 6.0) as i32;
        let x = DISPLAY_WIDTH as i32 / 2 + 75 + (t * 45.0) as i32 + sway;
        let y = DISPLAY_HEIGHT as i32 / 2 - 15 - (t * 85.0) as i32;
        let fade = if t < 0.15 { t / 0.15 } else { 1.0 - (t - 0.15) / 0.85 };
        draw_z(target, Point::new(x, y), size, scale_color(EYE_COLOR, fade));
    }
}

fn draw_z<D: DrawTarget<Color = Rgb565>>(target: &mut D, top_left: Point, size: i32, color: Rgb565) {
    let style = PrimitiveStyle::with_stroke(color, (size / 8).max(2) as u32);
    let tl = top_left;
    let tr = top_left + Point::new(size, 0);
    let bl = top_left + Point::new(0, size);
    let br = top_left + Point::new(size, size);
    for (start, end) in [(tl, tr), (tr, bl), (bl, br)] {
        Line::new(start, end).into_styled(style).draw(target).ok();
    }
}

fn scale_color(color: Rgb565, factor: f32) -> Rgb565 {
    let factor = factor.clamp(0.0, 1.0);
    Rgb565::new(
        (color.r() as f32 * factor) as u8,
        (color.g() as f32 * factor) as u8,
        (color.b() as f32 * factor) as u8,
    )
}

/// Approximates `sin(2π · turns)` (Bhaskara I), good enough for animations.
fn sin_turns(turns: f32) -> f32 {
    let t = turns - (turns as i32) as f32;
    let (x, sign) = if t < 0.5 { (t * 2.0, 1.0) } else { ((t - 0.5) * 2.0, -1.0) };
    let p = x * (1.0 - x);
    sign * 16.0 * p / (5.0 - 4.0 * p)
}

/// CRT-style scanline dimming and occasional horizontal glitch bands.
fn scanline_effects(buffer: &mut [Rgb565], frame: u32) {
    let rng = esp_hal::rng::Rng::new();
    let glitch_start = (rng.random() % 450) as usize;
    let frame = frame as usize;

    for y in 0..DISPLAY_HEIGHT {
        if (y + frame / 4) % 3 == 0 {
            for px in &mut buffer[y * DISPLAY_WIDTH..][..DISPLAY_WIDTH] {
                let c = *px;
                *px = Rgb565::new(c.r() / 2, c.g() / 2, c.b() / 2);
            }
        }

        if y >= glitch_start && y <= glitch_start + 40 {
            let shift = rng.random() % 6;
            let line = &mut buffer[y * DISPLAY_WIDTH..][..DISPLAY_WIDTH];
            line.rotate_left(shift as usize);
        }
    }
}
