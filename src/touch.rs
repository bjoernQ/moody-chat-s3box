//! GT911 capacitive touch controller of the ESP32-S3-BOX-3.
//!
//! The controller shares the I2C bus with the audio codecs, which are only
//! configured once at boot, so the touch task takes over the bus afterwards.
//! It polls the status register and fires [`crate::app::TAP`] on touch-down.

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::i2c::master::I2c;

use crate::app::TAP;
use crate::println;

const ADDRESSES: [u8; 2] = [0x5D, 0x14];
const REG_PRODUCT_ID: u16 = 0x8140;
const REG_STATUS: u16 = 0x814E;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEBOUNCE: Duration = Duration::from_millis(400);

pub fn spawn(spawner: &Spawner, i2c: I2c<'static, esp_hal::Blocking>) {
    spawner.spawn(touch_task(i2c).unwrap());
}

#[embassy_executor::task]
async fn touch_task(mut i2c: I2c<'static, esp_hal::Blocking>) {
    let address = loop {
        if let Some(address) = probe(&mut i2c) {
            break address;
        }
        println!("GT911 touch controller not found, retrying");
        Timer::after(Duration::from_secs(2)).await;
    };

    let mut touching = false;
    let mut last_tap = Instant::MIN;

    loop {
        Timer::after(POLL_INTERVAL).await;

        let mut status = [0u8; 1];
        if read_register(&mut i2c, address, REG_STATUS, &mut status).is_err() {
            continue;
        }
        let status = status[0];
        if status & 0x80 == 0 {
            continue;
        }

        let points = status & 0x0F;
        // The status register has to be cleared to receive the next report.
        let _ = i2c.write(address, &[(REG_STATUS >> 8) as u8, REG_STATUS as u8, 0]);

        let now_touching = points > 0;
        if now_touching && !touching && Instant::now() - last_tap > DEBOUNCE {
            last_tap = Instant::now();
            TAP.signal(());
        }
        touching = now_touching;
    }
}

fn probe(i2c: &mut I2c<'static, esp_hal::Blocking>) -> Option<u8> {
    ADDRESSES.into_iter().find(|&address| {
        let mut id = [0u8; 4];
        let found = read_register(i2c, address, REG_PRODUCT_ID, &mut id).is_ok();
        if found {
            println!(
                "Touch controller GT{} at {:#04x}",
                core::str::from_utf8(&id).unwrap_or("?").trim_end_matches('\0'),
                address
            );
        }
        found
    })
}

fn read_register(
    i2c: &mut I2c<'static, esp_hal::Blocking>,
    address: u8,
    register: u16,
    buffer: &mut [u8],
) -> Result<(), esp_hal::i2c::master::Error> {
    i2c.write_read(address, &register.to_be_bytes(), buffer)
}
