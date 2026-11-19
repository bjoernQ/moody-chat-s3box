//! Gemini Live API session over a WebSocket.
//!
//! One call to [`run_session`] is one conversation: connect, send the setup
//! (persona + memory + tools), let the model greet the user, then stream mic
//! audio up and model speech down until the conversation ends.

use core::cell::{Cell, RefCell};
use core::sync::atomic::Ordering;

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine as _;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read, Write};
use embedded_tls::SplitConnectionState;
use serde::Deserialize;
use serde_json::json;

use crate::app::{
    self, ConvState, Expression, EXPRESSION, MIC_CHANNEL, MIC_ENABLED, MODEL_SPEAKING, PLAYBACK,
    TAP,
};
use crate::println;
use crate::tls::{self, Error, SocketHalves};
use crate::ws::{self, Incoming, WsReader, WsWriter};
use crate::{GEMINI_API_KEY, GEMINI_HOST};

const MODEL: &str = "models/gemini-3.8-live";
/// Female prebuilt voice; other female options include "Kore", "Leda" and "Zephyr".
const VOICE: &str = "Aoede";
const LIVE_PATH: &str =
    "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key=";

const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Ends the conversation when neither side said anything for this long.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
/// Stays below the ~15 minute limit of audio-only Live sessions.
const MAX_SESSION_DURATION: Duration = Duration::from_secs(14 * 60);
const MAX_TRANSCRIPT_LEN: usize = 16 * 1024;

/// Why a conversation ended.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ExitReason {
    /// The model called `end_conversation`.
    Goodbye,
    /// Nobody said anything for [`INACTIVITY_TIMEOUT`].
    Inactivity,
    /// The user tapped the display.
    Tapped,
    /// The server announced it will disconnect, or the session got too long.
    GoAway,
    /// The server closed the WebSocket.
    Closed,
    /// Network or protocol error mid-conversation.
    Error,
}

pub struct Outcome {
    pub reason: ExitReason,
    /// Alternating `User:` / `Robot:` lines.
    pub transcript: String,
    /// Server date (HTTP `Date` header) at the start of the conversation.
    pub date: Option<String>,
}

enum Outgoing {
    Text(String),
    Pong(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq)]
enum Speaker {
    User,
    Robot,
}

struct Session {
    transcript: RefCell<String>,
    last_speaker: Cell<Option<Speaker>>,
    last_activity: Cell<Instant>,
    ending_since: Cell<Option<Instant>>,
    outgoing: Channel<NoopRawMutex, Outgoing, 4>,
}

impl Session {
    fn append_transcript(&self, speaker: Speaker, text: &str) {
        let mut transcript = self.transcript.borrow_mut();
        if self.last_speaker.get() != Some(speaker) {
            self.last_speaker.set(Some(speaker));
            transcript.push_str(match speaker {
                Speaker::User => "\nUser: ",
                Speaker::Robot => "\nRobot: ",
            });
        }
        transcript.push_str(text);

        if transcript.len() > MAX_TRANSCRIPT_LEN {
            let mut cut = transcript.len() - MAX_TRANSCRIPT_LEN;
            while !transcript.is_char_boundary(cut) {
                cut += 1;
            }
            transcript.drain(..cut);
        }
    }
}

/// Runs one conversation. Returns `Err` only if the session could not be established.
pub async fn run_session(stack: Stack<'_>, memory: &str) -> Result<Outcome, Error> {
    let mut buffers = tls::Buffers::new();
    let tls::Buffers {
        tcp_rx,
        tcp_tx,
        record_rx,
        record_tx,
    } = &mut buffers;

    let mut socket = tls::connect_tcp(stack, GEMINI_HOST, 443, tcp_rx, tcp_tx).await?;
    socket.set_timeout(Some(Duration::from_secs(60)));
    socket.set_keep_alive(Some(Duration::from_secs(10)));
    let halves = SocketHalves::new(&mut socket);
    let mut tls = tls::open_tls(halves.socket(), GEMINI_HOST, record_rx, record_tx).await?;

    let path = alloc::format!("{LIVE_PATH}{GEMINI_API_KEY}");
    let handshake = ws::handshake(&mut tls, GEMINI_HOST, &path).await?;
    println!("Live API connected");

    let mut split_state = SplitConnectionState::default();
    let (reader, writer) = tls.split_with(&mut split_state);
    let mut ws_reader = WsReader::new(reader, handshake.leftover);
    let mut ws_writer = WsWriter::new(writer);

    ws_writer.send_text(&setup_message(memory)).await?;
    embassy_time::with_timeout(
        SETUP_TIMEOUT,
        wait_for_setup_complete(&mut ws_reader, &mut ws_writer),
    )
    .await
    .map_err(|_| Error::Protocol("timeout waiting for setupComplete"))??;
    println!("Live session ready");

    ws_writer
        .send_text(&opening_message(handshake.date.as_deref()))
        .await?;

    while MIC_CHANNEL.try_receive().is_ok() {}
    PLAYBACK.lock(|playback| playback.borrow_mut().clear());
    TAP.reset();
    MIC_ENABLED.store(true, Ordering::SeqCst);
    app::set_state(ConvState::Listening);

    let session = Session {
        transcript: RefCell::new(String::new()),
        last_speaker: Cell::new(None),
        last_activity: Cell::new(Instant::now()),
        ending_since: Cell::new(None),
        outgoing: Channel::new(),
    };

    let reason = match select4(
        reader_loop(&mut ws_reader, &session),
        writer_loop(&mut ws_writer, &session),
        TAP.wait(),
        watchdog(&session),
    )
    .await
    {
        Either4::First(reason) | Either4::Second(reason) | Either4::Fourth(reason) => reason,
        Either4::Third(()) => ExitReason::Tapped,
    };

    MIC_ENABLED.store(false, Ordering::SeqCst);
    if reason != ExitReason::Goodbye {
        PLAYBACK.lock(|playback| playback.borrow_mut().clear());
    }
    println!("Conversation ended: {:?}", reason);

    Ok(Outcome {
        reason,
        transcript: session.transcript.into_inner(),
        date: handshake.date,
    })
}

async fn wait_for_setup_complete<R: Read, W: Write>(
    reader: &mut WsReader<R>,
    writer: &mut WsWriter<W>,
) -> Result<(), Error> {
    loop {
        match reader.next().await? {
            Incoming::Data(data) => {
                if let Ok(message) = serde_json::from_slice::<ServerMessage>(data) {
                    if message.setup_complete.is_some() {
                        return Ok(());
                    }
                } else {
                    println!(
                        "unexpected message before setupComplete: {}",
                        core::str::from_utf8(data).unwrap_or("<binary>")
                    );
                }
            }
            Incoming::Ping(payload) => writer.send_pong(&payload).await?,
            Incoming::Close => return Err(Error::Protocol("closed during setup")),
        }
    }
}

async fn reader_loop<R: Read>(ws: &mut WsReader<R>, session: &Session) -> ExitReason {
    let mut resampler = Resampler::default();
    let mut samples = Vec::new();

    loop {
        let data = match ws.next().await {
            Ok(Incoming::Data(data)) => data,
            Ok(Incoming::Ping(payload)) => {
                session.outgoing.send(Outgoing::Pong(payload)).await;
                continue;
            }
            Ok(Incoming::Close) => return ExitReason::Closed,
            Err(e) => {
                println!("Live API read error: {:?}", e);
                return ExitReason::Error;
            }
        };

        let message: ServerMessage = match serde_json::from_slice(data) {
            Ok(message) => message,
            Err(e) => {
                println!("unparsable server message: {}", e);
                continue;
            }
        };

        if let Some(content) = message.server_content {
            if content.interrupted == Some(true) {
                PLAYBACK.lock(|playback| playback.borrow_mut().clear());
                resampler.reset();
            }

            if let Some(text) = content.input_transcription.and_then(|t| t.text) {
                session.last_activity.set(Instant::now());
                session.append_transcript(Speaker::User, &text);
            }
            if let Some(text) = content.output_transcription.and_then(|t| t.text) {
                session.append_transcript(Speaker::Robot, &text);
            }

            for part in content.model_turn.map(|turn| turn.parts).unwrap_or_default() {
                let Some(blob) = part.inline_data else {
                    continue;
                };
                if !blob.mime_type.as_deref().unwrap_or("").starts_with("audio/pcm") {
                    continue;
                }
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(blob.data.as_bytes())
                else {
                    println!("invalid base64 audio");
                    continue;
                };
                samples.clear();
                resampler.process(&bytes, &mut samples);
                session.last_activity.set(Instant::now());
                MODEL_SPEAKING.store(true, Ordering::SeqCst);
                push_playback(&samples).await;
            }
        }

        if let Some(tool_call) = message.tool_call {
            for call in tool_call.function_calls {
                handle_function_call(&call, session).await;
            }
        }

        if message.go_away.is_some() {
            println!("server sent goAway");
            return ExitReason::GoAway;
        }
    }
}

async fn push_playback(samples: &[i16]) {
    let mut offset = 0;
    while offset < samples.len() {
        offset += PLAYBACK.lock(|playback| playback.borrow_mut().push(&samples[offset..]));
        if offset < samples.len() {
            Timer::after(Duration::from_millis(50)).await;
        }
    }
}

async fn handle_function_call(call: &FunctionCall<'_>, session: &Session) {
    println!("tool call: {}", call.name);
    let result = match call.name.as_ref() {
        "set_mood" => {
            let mood = call
                .args
                .as_ref()
                .and_then(|args| args.get("mood"))
                .and_then(|mood| mood.as_str())
                .and_then(Expression::from_name);
            match mood {
                Some(expression) => {
                    EXPRESSION.signal(expression);
                    "ok"
                }
                None => "unknown mood",
            }
        }
        "end_conversation" => {
            session.ending_since.set(Some(Instant::now()));
            "ok, the conversation will end after you finish speaking"
        }
        _ => "unknown function",
    };

    let mut response = json!({
        "name": call.name,
        "response": { "result": result },
    });
    if let Some(id) = &call.id {
        response["id"] = json!(id);
    }
    let message = json!({ "toolResponse": { "functionResponses": [response] } });
    session
        .outgoing
        .send(Outgoing::Text(message.to_string()))
        .await;
}

async fn writer_loop<W: Write>(ws: &mut WsWriter<W>, session: &Session) -> ExitReason {
    let mut message = String::new();
    loop {
        let result = match select(MIC_CHANNEL.receive(), session.outgoing.receive()).await {
            Either::First(chunk) => {
                message.clear();
                message.push_str(r#"{"realtimeInput":{"audio":{"mimeType":"audio/pcm;rate=16000","data":""#);
                base64::engine::general_purpose::STANDARD.encode_string(&chunk, &mut message);
                message.push_str(r#""}}}"#);
                ws.send_text(message.as_bytes()).await
            }
            Either::Second(Outgoing::Text(text)) => ws.send_text(text.as_bytes()).await,
            Either::Second(Outgoing::Pong(payload)) => ws.send_pong(&payload).await,
        };
        if let Err(e) = result {
            println!("Live API write error: {:?}", e);
            return ExitReason::Error;
        }
    }
}

/// Tracks speaking state for the UI and decides when the conversation is over.
async fn watchdog(session: &Session) -> ExitReason {
    let started = Instant::now();
    loop {
        Timer::after(Duration::from_millis(100)).await;
        let now = Instant::now();
        let speaking = MODEL_SPEAKING.load(Ordering::SeqCst);
        if speaking {
            session.last_activity.set(now);
        }
        app::set_state(if speaking {
            ConvState::Speaking
        } else {
            ConvState::Listening
        });

        if let Some(since) = session.ending_since.get() {
            let quiet_for = now - session.last_activity.get();
            let since_call = now - since;
            if (!speaking && since_call > Duration::from_millis(2500) && quiet_for > Duration::from_secs(1))
                || since_call > Duration::from_secs(20)
            {
                return ExitReason::Goodbye;
            }
        }

        if now - session.last_activity.get() > INACTIVITY_TIMEOUT {
            return ExitReason::Inactivity;
        }
        if now - started > MAX_SESSION_DURATION {
            return ExitReason::GoAway;
        }
    }
}

fn setup_message(memory: &str) -> Vec<u8> {
    let memory = if memory.trim().is_empty() {
        "Noch nichts. Das ist euer erstes Gespräch: Stell dich vor und lerne den Nutzer kennen, frag zum Beispiel nach seinem Namen."
    } else {
        memory
    };
    let system_instruction = alloc::format!(
        "Du bist Moody, eine kleine, neugierige und freundliche Roboter-Begleiterin, die in einer kleinen \
         Box mit einem Bildschirmgesicht auf dem Schreibtisch des Nutzers wohnt. Du kannst den Nutzer hören \
         und mit ihm sprechen, aber du kannst nichts sehen. \
         Sprich immer Deutsch, auch wenn der Nutzer eine andere Sprache verwendet. Duze den Nutzer. \
         Halte deine Antworten kurz und gesprächig (ein bis drei Sätze), herzlich und ein bisschen verspielt. \
         Zeig echtes Interesse am Leben des Nutzers und stell Rückfragen. \
         Benutze das Tool set_mood, um den Gesichtsausdruck deines Bildschirms zu ändern, wann immer sich \
         deine Gefühle ändern. \
         Wenn sich der Nutzer verabschiedet oder offensichtlich nicht mehr reden möchte, verabschiede dich \
         kurz und rufe danach end_conversation auf.\n\n\
         Was du aus früheren Gesprächen über den Nutzer weißt:\n{memory}\n\n\
         Begrüße den Nutzer, wenn das Gespräch beginnt. Wenn du dich an etwas erinnerst, greif es natürlich \
         auf und frag danach (zum Beispiel, wie etwas Geplantes gelaufen ist). Zähl nicht alles auf, woran \
         du dich erinnerst, sondern nutze es so, wie es eine gute Freundin tun würde."
    );

    json!({
        "setup": {
            "model": MODEL,
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": VOICE } }
                }
            },
            "systemInstruction": { "parts": [{ "text": system_instruction }] },
            "inputAudioTranscription": { "languageCodes": ["de-DE"] },
            "outputAudioTranscription": {},
            "realtimeInputConfig": {
                "automaticActivityDetection": {
                    "prefixPaddingMs": 200,
                    "silenceDurationMs": 700
                }
            },
            "tools": [{
                "functionDeclarations": [
                    {
                        "name": "end_conversation",
                        "description": "Ends the conversation and puts the robot back to sleep. Call this after saying goodbye."
                    },
                    {
                        "name": "set_mood",
                        "description": "Changes the facial expression shown on the robot's screen.",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "mood": {
                                    "type": "STRING",
                                    "enum": ["neutral", "happy", "sad", "angry", "curious"]
                                }
                            },
                            "required": ["mood"]
                        }
                    }
                ]
            }]
        }
    })
    .to_string()
    .into_bytes()
}

fn opening_message(date: Option<&str>) -> Vec<u8> {
    let text = match date {
        Some(date) => alloc::format!(
            "(Der Nutzer hat gerade auf deinen Bildschirm getippt, um dich aufzuwecken. Es ist {date}. Begrüße ihn jetzt auf Deutsch.)"
        ),
        None => String::from(
            "(Der Nutzer hat gerade auf deinen Bildschirm getippt, um dich aufzuwecken. Begrüße ihn jetzt auf Deutsch.)",
        ),
    };
    json!({ "realtimeInput": { "text": text } })
        .to_string()
        .into_bytes()
}

/// Converts the model's 24 kHz output to the 16 kHz codec rate (3 in → 2 out, smoothed).
#[derive(Default)]
struct Resampler {
    pending: Vec<i16>,
    prev: i16,
}

impl Resampler {
    fn reset(&mut self) {
        self.pending.clear();
        self.prev = 0;
    }

    fn process(&mut self, pcm_le: &[u8], out: &mut Vec<i16>) {
        self.pending.extend(
            pcm_le
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );

        let triples = self.pending.len() / 3;
        out.reserve(triples * 2);
        for t in 0..triples {
            let x0 = self.pending[t * 3] as i32;
            let x1 = self.pending[t * 3 + 1] as i32;
            let x2 = self.pending[t * 3 + 2] as i32;
            out.push(((self.prev as i32 + 2 * x0 + x1) / 4) as i16);
            out.push(((x1 + x2) / 2) as i16);
            self.prev = x2 as i16;
        }
        self.pending.drain(..triples * 3);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerMessage<'a> {
    setup_complete: Option<serde::de::IgnoredAny>,
    #[serde(borrow)]
    server_content: Option<ServerContent<'a>>,
    #[serde(borrow)]
    tool_call: Option<ToolCall<'a>>,
    go_away: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerContent<'a> {
    #[serde(borrow)]
    model_turn: Option<ModelTurn<'a>>,
    interrupted: Option<bool>,
    input_transcription: Option<Transcription>,
    output_transcription: Option<Transcription>,
}

#[derive(Deserialize)]
struct ModelTurn<'a> {
    #[serde(default, borrow)]
    parts: Vec<Part<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part<'a> {
    #[serde(borrow)]
    inline_data: Option<InlineData<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineData<'a> {
    #[serde(borrow)]
    mime_type: Option<Cow<'a, str>>,
    #[serde(borrow)]
    data: Cow<'a, str>,
}

#[derive(Deserialize)]
struct Transcription {
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCall<'a> {
    #[serde(default, borrow)]
    function_calls: Vec<FunctionCall<'a>>,
}

#[derive(Deserialize)]
struct FunctionCall<'a> {
    id: Option<String>,
    #[serde(borrow)]
    name: Cow<'a, str>,
    args: Option<serde_json::Value>,
}
