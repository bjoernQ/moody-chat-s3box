//! Gemini Live companion for the ESP32-S3-BOX-3.
//!
//! Sleeps with a snooze animation until the display is tapped, then holds a
//! voice conversation through the Gemini Live API. Afterwards the conversation
//! is distilled into a persistent memory that personalizes the next one.

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use core::ptr::addr_of_mut;
use core::sync::atomic::Ordering;

use alloc::string::String;
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::i2c::master::I2c;
use esp_hal::ram;
use esp_hal::system::{CpuControl, Stack};
use esp_hal::timer::systimer::SystemTimer;
use live_chat_example::app::{self, ConvState, Mode, MODEL_SPEAKING, TAP};
use live_chat_example::memory::MemoryStore;
use live_chat_example::{audio, distill, live, net, println, touch};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

static mut APP_CORE_STACK: Stack<8192> = Stack::new();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);
    esp_alloc::heap_allocator!(size: 32 * 1024);
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);

    let timer0 = SystemTimer::new(peripherals.SYSTIMER);
    let sw_int =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timer0.alarm0, sw_int.software_interrupt0);

    println!("Embassy initialized");
    app::init_playback();

    let mut memory_store = MemoryStore::new(peripherals.FLASH);
    let mut memory = memory_store
        .as_mut()
        .map(|store| store.load())
        .unwrap_or_default();
    match &memory_store {
        Some(_) if memory.is_empty() => println!("No memory stored yet"),
        Some(_) => println!("Loaded memory ({} bytes)", memory.len()),
        None => println!("No nvs partition found, memory will not persist"),
    }

    let mut cpu_control = CpuControl::new(peripherals.CPU_CTRL);
    let _ui_core = cpu_control
        .start_app_core(unsafe { &mut *addr_of_mut!(APP_CORE_STACK) }, move || {
            live_chat_example::ui::run_ui();
        })
        .unwrap();

    let mut i2c = I2c::new(peripherals.I2C0, esp_hal::i2c::master::Config::default())
        .unwrap()
        .with_sda(peripherals.GPIO8)
        .with_scl(peripherals.GPIO18);
    audio::init_codecs(&mut i2c);
    touch::spawn(&spawner, i2c);

    let rng = esp_hal::rng::Rng::new();
    let station_config = esp_radio::wifi::Config::Station(
        esp_radio::wifi::sta::StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.into()),
    );
    let (wifi_controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap();

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        net_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );
    net::spawn(&spawner, wifi_controller, runner);

    println!("Waiting for network...");
    stack.wait_link_up().await;
    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        println!("Got IP: {}", config.address);
    }

    audio::spawn(&spawner);

    Timer::after(Duration::from_millis(1500)).await;
    app::set_state(ConvState::Sleeping);
    app::MODE.store(Mode::Visuals as u32, Ordering::SeqCst);
    println!("Tap the display to talk");

    loop {
        TAP.reset();
        TAP.wait().await;
        app::set_state(ConvState::Connecting);

        match live::run_session(stack, &memory).await {
            Ok(outcome) => {
                finish_playback().await;

                if outcome.transcript.contains("User:") {
                    app::set_state(ConvState::Remembering);
                    println!("Transcript:{}", outcome.transcript);
                    remember(stack, &mut memory, &mut memory_store, &outcome).await;
                }
            }
            Err(e) => {
                println!("Could not start conversation: {:?}", e);
                app::set_state(ConvState::Error);
                Timer::after(Duration::from_secs(4)).await;
            }
        }

        app::set_state(ConvState::Sleeping);
    }
}

/// Lets a goodbye that is still in the playback buffer finish.
async fn finish_playback() {
    let deadline = Instant::now() + Duration::from_secs(8);
    while MODEL_SPEAKING.load(Ordering::SeqCst) && Instant::now() < deadline {
        Timer::after(Duration::from_millis(100)).await;
    }
}

async fn remember(
    stack: embassy_net::Stack<'static>,
    memory: &mut String,
    memory_store: &mut Option<MemoryStore>,
    outcome: &live::Outcome,
) {
    match distill::distill(stack, memory, &outcome.transcript, outcome.date.as_deref()).await {
        Ok(updated) => {
            println!("New memory:\n{}", updated);
            if let Some(store) = memory_store {
                if let Err(e) = store.store(&updated) {
                    println!("Saving memory failed: {:?}", e);
                }
            }
            *memory = updated;
        }
        Err(e) => println!("Distilling memory failed, keeping the old one: {:?}", e),
    }
}

#[no_mangle]
fn custom_halt() {
    loop {}
}
