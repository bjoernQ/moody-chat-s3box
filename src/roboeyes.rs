// Copyright (C) 2024 Dennis Hoelscher (FluxGarage) — original RoboEyes (GPL-3.0)
// Copyright (C) 2026 bjoernQ — Rust port for embedded-graphics
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. See <https://www.gnu.org/licenses/gpl-3.0.html>.

//! Animated robot eyes renderer for embedded-graphics.
//!
//! Rust port of [FluxGarage RoboEyes](https://github.com/FluxGarage/RoboEyes) (Arduino/C++, GPL-3.0),
//! generated automatically with Gemini CLI.
//! Drives mood, blink, idle gaze, and one-shot animations through a simple `update` + `draw` loop.

use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{CornerRadii, PrimitiveStyleBuilder, Rectangle, RoundedRectangle, Triangle},
};

// Simple Pseudo-Random Number Generator to avoid external dependencies
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.state >> 32) as u32
    }

    fn range(&mut self, min: i32, max: i32) -> i32 {
        if min >= max {
            return min;
        }
        let range = (max - min) as u32;
        min + (self.next_u32() % range) as i32
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mood {
    Tired,
    Angry,
    Happy,
    Default,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Position {
    N,
    NE,
    E,
    SE,
    S,
    SW,
    W,
    NW,
    Center,
}

struct SweatDrop {
    x_pos_initial: i32,
    x_pos: i32,
    y_pos: f32,
    y_pos_max: i32,
    height: f32,
    width: f32,
}

impl SweatDrop {
    fn new() -> Self {
        Self {
            x_pos_initial: 2,
            x_pos: 0,
            y_pos: 2.0,
            y_pos_max: 0,
            height: 2.0,
            width: 1.0,
        }
    }
}

pub struct RoboEyes {
    screen_width: i32,
    screen_height: i32,

    // Colors
    bg_color: Rgb565,
    main_color: Rgb565,

    // State
    tired: bool,
    angry: bool,
    happy: bool,
    curious: bool,
    cyclops: bool,
    eye_l_open: bool,
    eye_r_open: bool,

    // Geometry - Left Eye
    eye_l_height_default: i32,
    eye_l_width_current: i32,
    eye_l_height_current: i32,
    eye_l_width_next: i32,
    eye_l_height_next: i32,
    eye_l_height_offset: i32,
    eye_l_border_radius_current: i32,
    eye_l_border_radius_next: i32,

    // Geometry - Right Eye
    eye_r_height_default: i32,
    eye_r_width_current: i32,
    eye_r_height_current: i32,
    eye_r_width_next: i32,
    eye_r_height_next: i32,
    eye_r_height_offset: i32,
    eye_r_border_radius_current: i32,
    eye_r_border_radius_next: i32,

    // Coordinates
    eye_l_x: i32,
    eye_l_y: i32,
    eye_l_x_next: i32,
    eye_l_y_next: i32,

    eye_r_x: i32,
    eye_r_y: i32,
    eye_r_x_next: i32,
    eye_r_y_next: i32,

    // Eyelids
    eyelids_tired_height: i32,
    eyelids_tired_height_next: i32,
    eyelids_angry_height: i32,
    eyelids_angry_height_next: i32,
    eyelids_happy_bottom_offset: i32,
    eyelids_happy_bottom_offset_next: i32,

    space_between_current: i32,
    space_between_next: i32,

    // Animations
    h_flicker: bool,
    h_flicker_alternate: bool,
    h_flicker_amplitude: i32,

    v_flicker: bool,
    v_flicker_alternate: bool,
    v_flicker_amplitude: i32,

    autoblinker: bool,
    blink_interval: i32,           // seconds
    blink_interval_variation: i32, // seconds
    blink_timer: u64,

    idle: bool,
    idle_interval: i32,
    idle_interval_variation: i32,
    idle_animation_timer: u64,

    confused: bool,
    confused_animation_timer: u64,
    confused_animation_duration: u64,
    confused_toggle: bool,

    laugh: bool,
    laugh_animation_timer: u64,
    laugh_animation_duration: u64,
    laugh_toggle: bool,

    pub sweat: bool,
    sweat_border_radius: i32,

    sweat1: SweatDrop,
    sweat2: SweatDrop,
    sweat3: SweatDrop,

    rng: SimpleRng,
}

impl RoboEyes {
    pub fn new(width: i32, height: i32) -> Self {
        let eye_width = 64;
        let eye_height = 64;
        let space = 10;

        let eye_l_x_default = (width - (eye_width + space + eye_width)) / 2;
        let eye_l_y_default = (height - eye_height) / 2;
        let eye_r_x_default = eye_l_x_default + eye_width + space;
        let eye_r_y_default = eye_l_y_default;

        Self {
            screen_width: width,
            screen_height: height,
            bg_color: Rgb565::BLACK,
            main_color: Rgb565::WHITE,

            tired: false,
            angry: false,
            happy: false,
            curious: false,
            cyclops: false,
            eye_l_open: false,
            eye_r_open: false,

            eye_l_height_default: eye_height,
            eye_l_width_current: eye_width,
            eye_l_height_current: 1, // Start closed
            eye_l_width_next: eye_width,
            eye_l_height_next: eye_height,
            eye_l_height_offset: 0,
            eye_l_border_radius_current: 8,
            eye_l_border_radius_next: 8,

            eye_r_height_default: eye_height,
            eye_r_width_current: eye_width,
            eye_r_height_current: 1, // Start closed
            eye_r_width_next: eye_width,
            eye_r_height_next: eye_height,
            eye_r_height_offset: 0,
            eye_r_border_radius_current: 8,
            eye_r_border_radius_next: 8,

            eye_l_x: eye_l_x_default,
            eye_l_y: eye_l_y_default,
            eye_l_x_next: eye_l_x_default,
            eye_l_y_next: eye_l_y_default,

            eye_r_x: eye_r_x_default,
            eye_r_y: eye_r_y_default,
            eye_r_x_next: eye_r_x_default,
            eye_r_y_next: eye_r_y_default,

            eyelids_tired_height: 0,
            eyelids_tired_height_next: 0,
            eyelids_angry_height: 0,
            eyelids_angry_height_next: 0,
            eyelids_happy_bottom_offset: 0,
            eyelids_happy_bottom_offset_next: 0,

            space_between_current: space,
            space_between_next: space,

            h_flicker: false,
            h_flicker_alternate: false,
            h_flicker_amplitude: 2,

            v_flicker: false,
            v_flicker_alternate: false,
            v_flicker_amplitude: 10,

            autoblinker: false,
            blink_interval: 1,
            blink_interval_variation: 4,
            blink_timer: 0,

            idle: false,
            idle_interval: 1,
            idle_interval_variation: 3,
            idle_animation_timer: 0,

            confused: false,
            confused_animation_timer: 0,
            confused_animation_duration: 500,
            confused_toggle: true,

            laugh: false,
            laugh_animation_timer: 0,
            laugh_animation_duration: 500,
            laugh_toggle: true,

            sweat: false,
            sweat_border_radius: 3,
            sweat1: SweatDrop::new(),
            sweat2: SweatDrop::new(),
            sweat3: SweatDrop::new(),

            rng: SimpleRng::new(12345),
        }
    }

    pub fn set_colors(&mut self, bg: Rgb565, main: Rgb565) {
        self.bg_color = bg;
        self.main_color = main;
    }

    pub fn set_autoblinker(&mut self, active: bool, interval: i32, variation: i32) {
        self.autoblinker = active;
        self.blink_interval = interval;
        self.blink_interval_variation = variation;
    }

    pub fn set_idle_mode(&mut self, active: bool, interval: i32, variation: i32) {
        self.idle = active;
        self.idle_interval = interval;
        self.idle_interval_variation = variation;
    }

    pub fn mood(&self) -> Mood {
        if self.tired {
            return Mood::Tired;
        } else if self.angry {
            return Mood::Angry;
        } else if self.happy {
            return Mood::Happy;
        } else {
            return Mood::Default;
        }
    }

    pub fn set_mood(&mut self, mood: Mood) {
        match mood {
            Mood::Tired => {
                self.tired = true;
                self.angry = false;
                self.happy = false;
            }
            Mood::Angry => {
                self.tired = false;
                self.angry = true;
                self.happy = false;
            }
            Mood::Happy => {
                self.tired = false;
                self.angry = false;
                self.happy = true;
            }
            Mood::Default => {
                self.tired = false;
                self.angry = false;
                self.happy = false;
            }
        }
    }

    pub fn set_position(&mut self, position: Position) {
        let constraint_x = self.get_screen_constraint_x();
        let constraint_y = self.get_screen_constraint_y();

        match position {
            Position::N => {
                self.eye_l_x_next = constraint_x / 2;
                self.eye_l_y_next = 0;
            }
            Position::NE => {
                self.eye_l_x_next = constraint_x;
                self.eye_l_y_next = 0;
            }
            Position::E => {
                self.eye_l_x_next = constraint_x;
                self.eye_l_y_next = constraint_y / 2;
            }
            Position::SE => {
                self.eye_l_x_next = constraint_x;
                self.eye_l_y_next = constraint_y;
            }
            Position::S => {
                self.eye_l_x_next = constraint_x / 2;
                self.eye_l_y_next = constraint_y;
            }
            Position::SW => {
                self.eye_l_x_next = 0;
                self.eye_l_y_next = constraint_y;
            }
            Position::W => {
                self.eye_l_x_next = 0;
                self.eye_l_y_next = constraint_y / 2;
            }
            Position::NW => {
                self.eye_l_x_next = 0;
                self.eye_l_y_next = 0;
            }
            Position::Center => {
                self.eye_l_x_next = constraint_x / 2;
                self.eye_l_y_next = constraint_y / 2;
            }
        }
    }

    pub fn blink(&mut self) {
        self.close();
        self.open();
    }

    pub fn close(&mut self) {
        self.eye_l_height_next = 1;
        self.eye_r_height_next = 1;
        self.eye_l_open = false;
        self.eye_r_open = false;
    }

    pub fn open(&mut self) {
        self.eye_l_open = true;
        self.eye_r_open = true;
    }

    pub fn anim_confused(&mut self) {
        self.confused = true;
    }

    pub fn anim_laugh(&mut self) {
        self.laugh = true;
    }

    fn get_screen_constraint_x(&self) -> i32 {
        self.screen_width
            - self.eye_l_width_current
            - self.space_between_current
            - self.eye_r_width_current
    }

    fn get_screen_constraint_y(&self) -> i32 {
        self.screen_height - self.eye_l_height_default
    }

    pub fn update(&mut self, millis: u64) {
        // --- Pre-calculations ---

        // Curious gaze offset
        if self.curious {
            if self.eye_l_x_next <= 10 {
                self.eye_l_height_offset = 8;
            } else if self.eye_l_x_next >= (self.get_screen_constraint_x() - 10) && self.cyclops {
                self.eye_l_height_offset = 8;
            } else {
                self.eye_l_height_offset = 0;
            }

            if self.eye_r_x_next >= self.screen_width - self.eye_r_width_current - 10 {
                self.eye_r_height_offset = 8;
            } else {
                self.eye_r_height_offset = 0;
            }
        } else {
            self.eye_l_height_offset = 0;
            self.eye_r_height_offset = 0;
        }

        // Left eye height
        self.eye_l_height_current =
            (self.eye_l_height_current + self.eye_l_height_next + self.eye_l_height_offset) / 2;
        self.eye_l_y += (self.eye_l_height_default - self.eye_l_height_current) / 2;
        self.eye_l_y -= self.eye_l_height_offset / 2;

        // Right eye height
        self.eye_r_height_current =
            (self.eye_r_height_current + self.eye_r_height_next + self.eye_r_height_offset) / 2;
        self.eye_r_y += (self.eye_r_height_default - self.eye_r_height_current) / 2;
        self.eye_r_y -= self.eye_r_height_offset / 2;

        // Open eyes again check
        if self.eye_l_open {
            if self.eye_l_height_current <= 1 + self.eye_l_height_offset {
                self.eye_l_height_next = self.eye_l_height_default;
            }
        }
        if self.eye_r_open {
            if self.eye_r_height_current <= 1 + self.eye_r_height_offset {
                self.eye_r_height_next = self.eye_r_height_default;
            }
        }

        // Widths
        self.eye_l_width_current = (self.eye_l_width_current + self.eye_l_width_next) / 2;
        self.eye_r_width_current = (self.eye_r_width_current + self.eye_r_width_next) / 2;

        // Space
        self.space_between_current = (self.space_between_current + self.space_between_next) / 2;

        // Coordinates
        self.eye_l_x = (self.eye_l_x + self.eye_l_x_next) / 2;
        self.eye_l_y = (self.eye_l_y + self.eye_l_y_next) / 2;

        self.eye_r_x_next =
            self.eye_l_x_next + self.eye_l_width_current + self.space_between_current;
        self.eye_r_y_next = self.eye_l_y_next;

        self.eye_r_x = (self.eye_r_x + self.eye_r_x_next) / 2;
        self.eye_r_y = (self.eye_r_y + self.eye_r_y_next) / 2;

        // Border Radius
        self.eye_l_border_radius_current =
            (self.eye_l_border_radius_current + self.eye_l_border_radius_next) / 2;
        self.eye_r_border_radius_current =
            (self.eye_r_border_radius_current + self.eye_r_border_radius_next) / 2;

        // --- Macro Animations ---

        if self.autoblinker {
            if millis >= self.blink_timer {
                self.blink();
                let var = self.rng.range(0, self.blink_interval_variation);
                self.blink_timer =
                    millis + (self.blink_interval as u64 * 1000) + (var as u64 * 1000);
            }
        }

        if self.laugh {
            if self.laugh_toggle {
                self.v_flicker = true;
                self.v_flicker_amplitude = 5;
                self.laugh_animation_timer = millis;
                self.laugh_toggle = false;
            } else if millis >= self.laugh_animation_timer + self.laugh_animation_duration {
                self.v_flicker = false;
                self.v_flicker_amplitude = 0;
                self.laugh_toggle = true;
                self.laugh = false;
            }
        }

        if self.confused {
            if self.confused_toggle {
                self.h_flicker = true;
                self.h_flicker_amplitude = 20;
                self.confused_animation_timer = millis;
                self.confused_toggle = false;
            } else if millis >= self.confused_animation_timer + self.confused_animation_duration {
                self.h_flicker = false;
                self.h_flicker_amplitude = 0;
                self.confused_toggle = true;
                self.confused = false;
            }
        }

        if self.idle {
            if millis >= self.idle_animation_timer {
                self.eye_l_x_next = self.rng.range(0, self.get_screen_constraint_x());
                self.eye_l_y_next = self.rng.range(0, self.get_screen_constraint_y());
                let var = self.rng.range(0, self.idle_interval_variation);
                self.idle_animation_timer =
                    millis + (self.idle_interval as u64 * 1000) + (var as u64 * 1000);
            }
        }

        // Flickering
        if self.h_flicker {
            if self.h_flicker_alternate {
                self.eye_l_x += self.h_flicker_amplitude;
                self.eye_r_x += self.h_flicker_amplitude;
            } else {
                self.eye_l_x -= self.h_flicker_amplitude;
                self.eye_r_x -= self.h_flicker_amplitude;
            }
            self.h_flicker_alternate = !self.h_flicker_alternate;
        }

        if self.v_flicker {
            if self.v_flicker_alternate {
                self.eye_l_y += self.v_flicker_amplitude;
                self.eye_r_y += self.v_flicker_amplitude;
            } else {
                self.eye_l_y -= self.v_flicker_amplitude;
                self.eye_r_y -= self.v_flicker_amplitude;
            }
            self.v_flicker_alternate = !self.v_flicker_alternate;
        }

        if self.cyclops {
            self.eye_r_width_current = 0;
            self.eye_r_height_current = 0;
            self.space_between_current = 0;
        }

        // Sweat Animation Logic
        if self.sweat {
            self.update_sweat_drop(0, millis);
            self.update_sweat_drop(1, millis);
            self.update_sweat_drop(2, millis);
        }
    }

    fn update_sweat_drop(&mut self, index: usize, _millis: u64) {
        let drop = if index == 0 {
            &mut self.sweat1
        } else if index == 1 {
            &mut self.sweat2
        } else {
            &mut self.sweat3
        };

        if drop.y_pos <= drop.y_pos_max as f32 {
            drop.y_pos += 0.5;
        } else {
            // Reset
            match index {
                0 => {
                    drop.x_pos_initial = self.rng.range(0, self.screen_width - 60 + 30);
                    drop.y_pos_max = self.rng.range(10, 20);
                }
                1 => {
                    drop.x_pos_initial = self.rng.range(30, self.screen_width - 60 + 30);
                    drop.y_pos_max = self.rng.range(10, 20);
                }
                _ => {
                    drop.x_pos_initial = (self.screen_width - 30) + self.rng.range(0, 30);
                    drop.y_pos_max = self.rng.range(10, 20);
                }
            }
            drop.y_pos = 2.0;
            drop.width = 1.0;
            drop.height = 2.0;
        }

        if drop.y_pos <= (drop.y_pos_max as f32 / 2.0) {
            drop.width += 0.5;
            drop.height += 0.5;
        } else {
            drop.width -= 0.1;
            drop.height -= 0.5;
        }
        drop.x_pos = drop.x_pos_initial - (drop.width as i32 / 2);
    }

    pub fn draw<D>(&mut self, target: &mut D) -> Result<(), D::Error>
    where
        D: DrawTarget<Color = Rgb565>,
    {
        // Draw basic eyes
        let style = PrimitiveStyleBuilder::new()
            .fill_color(self.main_color)
            .build();

        RoundedRectangle::new(
            Rectangle::new(
                Point::new(self.eye_l_x, self.eye_l_y),
                Size::new(
                    self.eye_l_width_current as u32,
                    self.eye_l_height_current as u32,
                ),
            ),
            CornerRadii::new(Size::new(
                self.eye_l_border_radius_current as u32,
                self.eye_l_border_radius_current as u32,
            )),
        )
        .into_styled(style)
        .draw(target)?;

        if !self.cyclops {
            RoundedRectangle::new(
                Rectangle::new(
                    Point::new(self.eye_r_x, self.eye_r_y),
                    Size::new(
                        self.eye_r_width_current as u32,
                        self.eye_r_height_current as u32,
                    ),
                ),
                CornerRadii::new(Size::new(
                    self.eye_r_border_radius_current as u32,
                    self.eye_r_border_radius_current as u32,
                )),
            )
            .into_styled(style)
            .draw(target)?;
        }

        // Mood transitions
        if self.tired {
            self.eyelids_tired_height_next = self.eye_l_height_current / 2;
            self.eyelids_angry_height_next = 0;
        } else {
            self.eyelids_tired_height_next = 0;
        }

        if self.angry {
            self.eyelids_angry_height_next = self.eye_l_height_current / 2;
            self.eyelids_tired_height_next = 0;
        } else {
            self.eyelids_angry_height_next = 0;
        }

        if self.happy {
            self.eyelids_happy_bottom_offset_next = self.eye_l_height_current / 2;
        } else {
            self.eyelids_happy_bottom_offset_next = 0;
        }

        let bg_style = PrimitiveStyleBuilder::new()
            .fill_color(self.bg_color)
            .build();

        // Tired Eyelids
        self.eyelids_tired_height =
            (self.eyelids_tired_height + self.eyelids_tired_height_next) / 2;
        if self.eyelids_tired_height > 0 {
            if !self.cyclops {
                Triangle::new(
                    Point::new(self.eye_l_x, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + self.eye_l_width_current, self.eye_l_y - 1),
                    Point::new(self.eye_l_x, self.eye_l_y + self.eyelids_tired_height - 1),
                )
                .into_styled(bg_style)
                .draw(target)?;

                Triangle::new(
                    Point::new(self.eye_r_x, self.eye_r_y - 1),
                    Point::new(self.eye_r_x + self.eye_r_width_current, self.eye_r_y - 1),
                    Point::new(
                        self.eye_r_x + self.eye_r_width_current,
                        self.eye_r_y + self.eyelids_tired_height - 1,
                    ),
                )
                .into_styled(bg_style)
                .draw(target)?;
            } else {
                // Cyclops tired
                let half_width = self.eye_l_width_current / 2;
                Triangle::new(
                    Point::new(self.eye_l_x, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + half_width, self.eye_l_y - 1),
                    Point::new(self.eye_l_x, self.eye_l_y + self.eyelids_tired_height - 1),
                )
                .into_styled(bg_style)
                .draw(target)?;

                Triangle::new(
                    Point::new(self.eye_l_x + half_width, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + self.eye_l_width_current, self.eye_l_y - 1),
                    Point::new(
                        self.eye_l_x + self.eye_l_width_current,
                        self.eye_l_y + self.eyelids_tired_height - 1,
                    ),
                )
                .into_styled(bg_style)
                .draw(target)?;
            }
        }

        // Angry Eyelids
        self.eyelids_angry_height =
            (self.eyelids_angry_height + self.eyelids_angry_height_next) / 2;
        if self.eyelids_angry_height > 0 {
            if !self.cyclops {
                Triangle::new(
                    Point::new(self.eye_l_x, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + self.eye_l_width_current, self.eye_l_y - 1),
                    Point::new(
                        self.eye_l_x + self.eye_l_width_current,
                        self.eye_l_y + self.eyelids_angry_height - 1,
                    ),
                )
                .into_styled(bg_style)
                .draw(target)?;

                Triangle::new(
                    Point::new(self.eye_r_x, self.eye_r_y - 1),
                    Point::new(self.eye_r_x + self.eye_r_width_current, self.eye_r_y - 1),
                    Point::new(self.eye_r_x, self.eye_r_y + self.eyelids_angry_height - 1),
                )
                .into_styled(bg_style)
                .draw(target)?;
            } else {
                // Cyclops angry
                let half_width = self.eye_l_width_current / 2;
                Triangle::new(
                    Point::new(self.eye_l_x, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + half_width, self.eye_l_y - 1),
                    Point::new(
                        self.eye_l_x + half_width,
                        self.eye_l_y + self.eyelids_angry_height - 1,
                    ),
                )
                .into_styled(bg_style)
                .draw(target)?;

                Triangle::new(
                    Point::new(self.eye_l_x + half_width, self.eye_l_y - 1),
                    Point::new(self.eye_l_x + self.eye_l_width_current, self.eye_l_y - 1),
                    Point::new(
                        self.eye_l_x + half_width,
                        self.eye_l_y + self.eyelids_angry_height - 1,
                    ),
                )
                .into_styled(bg_style)
                .draw(target)?;
            }
        }

        // Happy Eyelids
        self.eyelids_happy_bottom_offset =
            (self.eyelids_happy_bottom_offset + self.eyelids_happy_bottom_offset_next) / 2;
        if self.eyelids_happy_bottom_offset > 0 {
            RoundedRectangle::new(
                Rectangle::new(
                    Point::new(
                        self.eye_l_x - 1,
                        (self.eye_l_y + self.eye_l_height_current)
                            - self.eyelids_happy_bottom_offset
                            + 1,
                    ),
                    Size::new(
                        (self.eye_l_width_current + 2) as u32,
                        self.eye_l_height_default as u32,
                    ),
                ),
                CornerRadii::new(Size::new(
                    self.eye_l_border_radius_current as u32,
                    self.eye_l_border_radius_current as u32,
                )),
            )
            .into_styled(bg_style)
            .draw(target)?;

            if !self.cyclops {
                RoundedRectangle::new(
                    Rectangle::new(
                        Point::new(
                            self.eye_r_x - 1,
                            (self.eye_r_y + self.eye_r_height_current)
                                - self.eyelids_happy_bottom_offset
                                + 1,
                        ),
                        Size::new(
                            (self.eye_r_width_current + 2) as u32,
                            self.eye_r_height_default as u32,
                        ),
                    ),
                    CornerRadii::new(Size::new(
                        self.eye_r_border_radius_current as u32,
                        self.eye_r_border_radius_current as u32,
                    )),
                )
                .into_styled(bg_style)
                .draw(target)?;
            }
        }

        // Sweat
        if self.sweat {
            let sweat_style = PrimitiveStyleBuilder::new()
                .fill_color(self.main_color)
                .build();
            let drops = [&self.sweat1, &self.sweat2, &self.sweat3];
            for drop in drops {
                if drop.width > 0.0 && drop.height > 0.0 {
                    RoundedRectangle::new(
                        Rectangle::new(
                            Point::new(drop.x_pos, drop.y_pos as i32),
                            Size::new(drop.width as u32, drop.height as u32),
                        ),
                        CornerRadii::new(Size::new(
                            self.sweat_border_radius as u32,
                            self.sweat_border_radius as u32,
                        )),
                    )
                    .into_styled(sweat_style)
                    .draw(target)?;
                }
            }
        }

        Ok(())
    }
}
