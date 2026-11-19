//! I2S audio streaming for the ESP32-S3-BOX-3 codec chain.
//!
//! Capture: ES7210 ADC → I2S RX → 100 ms mono chunks → [`MIC_CHANNEL`].
//! Playback: [`PLAYBACK`] ring buffer → I2S TX → ES8311 DAC → speaker.
//!
//! The box has no acoustic echo cancellation, so the microphone is replaced by
//! silence while the model speaks (plus a short tail) to keep the model from
//! hearing and interrupting itself.

use core::sync::atomic::Ordering;

use alloc::vec::Vec;
use embassy_executor::Spawner;
use esp_hal::gpio::Output;
use esp_hal::i2c::master::I2c;
use esp_hal::i2s::master::{I2s, I2sRx, I2sTx};
use esp_hal::time::{Duration, Instant, Rate};

use crate::app::{
    MIC_CHANNEL, MIC_ENABLED, MODEL_SPEAKING, PLAYBACK, SAMPLE_RATE_HZ, SPEECH_LEVEL,
};
use crate::println;

const DMA_CHUNK_SIZE: usize = 4092 * 4;
/// 100 ms of 16-bit mono audio.
const MIC_CHUNK_BYTES: usize = SAMPLE_RATE_HZ as usize / 10 * 2;
/// Keeps the mic muted after playback ends; covers the DMA buffer latency and room echo.
const ECHO_TAIL: Duration = Duration::from_millis(500);

/// Configures the ES7210 (mics) and ES8311 (speaker) codecs.
pub fn init_codecs(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    init_es7210(i2c);
    init_es8311(i2c);
}

/// Spawns the I2S capture and playback task.
pub fn spawn(spawner: &Spawner) {
    spawner.spawn(handle_i2s().unwrap());
}

#[embassy_executor::task]
async fn handle_i2s() {
    let mut pa_ctrl = Output::new(
        unsafe { esp_hal::peripherals::GPIO46::steal() },
        esp_hal::gpio::Level::Low,
        esp_hal::gpio::OutputConfig::default(),
    );
    pa_ctrl.set_high();

    let dma_channel = unsafe { esp_hal::peripherals::DMA_CH1::steal() };

    let (rx_buffer, rx_descriptors, _, _) = esp_hal::dma_buffers!(DMA_CHUNK_SIZE, 0);
    let (tx_buffer, tx_descriptors, _, _) = esp_hal::dma_buffers!(DMA_CHUNK_SIZE, 0);

    let i2s = I2s::new(
        unsafe { esp_hal::peripherals::I2S0::steal() },
        dma_channel,
        esp_hal::i2s::master::Config::new_tdm_philips()
            .with_signal_loopback(true)
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE_HZ))
            .with_data_format(esp_hal::i2s::master::DataFormat::Data16Channel16)
            .with_channels(esp_hal::i2s::master::Channels::STEREO),
    )
    .unwrap()
    .into_async()
    .with_mclk(unsafe { esp_hal::peripherals::GPIO2::steal() });

    let i2s_tx = i2s
        .i2s_tx
        .with_bclk(unsafe { esp_hal::peripherals::GPIO17::steal() })
        .with_ws(unsafe { esp_hal::peripherals::GPIO45::steal() })
        .with_dout(unsafe { esp_hal::peripherals::GPIO15::steal() })
        .build(tx_descriptors);

    let i2s_rx = i2s
        .i2s_rx
        .with_din(unsafe { esp_hal::peripherals::GPIO16::steal() })
        .build(rx_descriptors);

    embassy_futures::join::join(
        handle_i2s_rx(rx_buffer, i2s_rx),
        handle_i2s_tx(tx_buffer, i2s_tx),
    )
    .await;
}

fn init_es7210(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    let es7210 = es7210::Es7210::new(0x40);
    let codec_cfg = es7210::CodecConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        mclk_ratio: 256,
        i2s_format: es7210::I2sFormat::I2S,
        bit_width: es7210::I2sBits::Bits16,
        mic_bias: es7210::MicBias::V2_87,
        mic_gain: es7210::MicGain::Gain30dB,
        tdm_enable: false,
    };
    es7210.config_codec(i2c, &codec_cfg).unwrap();
    es7210.gain_set(i2c, 20).unwrap();
    es7210.set_mute(i2c, false).unwrap();
}

fn init_es8311(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    let es8311 = es8311::Es8311::new(0x18);
    let clock_cfg = es8311::ClockConfig {
        mclk_inverted: false,
        sclk_inverted: false,
        mclk_from_mclk_pin: true,
        mclk_frequency: 256 * SAMPLE_RATE_HZ,
        sample_frequency: SAMPLE_RATE_HZ,
    };
    let mut delay = esp_hal::delay::Delay::new();
    es8311
        .init(
            i2c,
            &clock_cfg,
            es8311::Resolution::Bits16,
            es8311::Resolution::Bits16,
            &mut delay,
        )
        .unwrap();
    es8311.volume_set(i2c, 80, None).unwrap();
    es8311.mute(i2c, false).unwrap();
}

async fn handle_i2s_rx(rx_buffer: &'static mut [u8], i2s_rx: I2sRx<'static, esp_hal::Async>) {
    let mut data = [0u8; 4092 * 3];
    let mut transaction = i2s_rx.read_dma_circular_async(rx_buffer).unwrap();
    let mut chunk: Vec<u8> = Vec::with_capacity(MIC_CHUNK_BYTES);
    let mut muted_until = Instant::EPOCH;

    loop {
        let count = transaction.pop(&mut data).await.unwrap();

        if !MIC_ENABLED.load(Ordering::SeqCst) {
            chunk.clear();
            continue;
        }

        let now = Instant::now();
        if MODEL_SPEAKING.load(Ordering::SeqCst) {
            muted_until = now + ECHO_TAIL;
        }
        let muted = now < muted_until;

        // Stereo frames; the left channel carries MIC1.
        for frame in data[..count].chunks_exact(4) {
            if muted {
                chunk.extend_from_slice(&[0, 0]);
            } else {
                chunk.extend_from_slice(&frame[..2]);
            }

            if chunk.len() >= MIC_CHUNK_BYTES {
                let full = core::mem::replace(&mut chunk, Vec::with_capacity(MIC_CHUNK_BYTES));
                if MIC_CHANNEL.try_send(full).is_err() {
                    println!("mic chunk dropped");
                }
            }
        }
    }
}

async fn handle_i2s_tx(tx_buffer: &'static mut [u8], i2s_tx: I2sTx<'static, esp_hal::Async>) {
    let mut transaction = i2s_tx.write_dma_circular_async(tx_buffer).unwrap();
    let mut mono = alloc::vec![0i16; DMA_CHUNK_SIZE / 4];

    loop {
        transaction
            .push_with(|out| {
                let frames = (out.len() / 4).min(mono.len());
                let (played, remaining) = PLAYBACK.lock(|playback| {
                    let mut playback = playback.borrow_mut();
                    let played = playback.pop(&mut mono[..frames]);
                    (played, playback.len())
                });

                let mut peak = 0u32;
                for (i, frame) in out[..frames * 4].chunks_exact_mut(4).enumerate() {
                    let sample = if i < played { mono[i] } else { 0 };
                    peak = peak.max(sample.unsigned_abs() as u32);
                    let bytes = sample.to_le_bytes();
                    frame.copy_from_slice(&[bytes[0], bytes[1], bytes[0], bytes[1]]);
                }

                MODEL_SPEAKING.store(played > 0 || remaining > 0, Ordering::SeqCst);
                SPEECH_LEVEL.store(peak, Ordering::Relaxed);

                frames * 4
            })
            .await
            .unwrap();
    }
}
