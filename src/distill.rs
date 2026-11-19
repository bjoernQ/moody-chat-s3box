//! Distills a finished conversation into the robot's long-term memory.
//!
//! Native-audio Live models can only answer with audio, so this uses a regular
//! text model through the REST `generateContent` endpoint.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use embassy_net::Stack;
use embedded_io_async::Write;
use serde_json::json;

use crate::println;
use crate::tls::{self, Error, SocketHalves};
use crate::{GEMINI_API_KEY, GEMINI_HOST};

const MODEL: &str = "gemini-3.1-flash-lite-preview";
/// Only the end of long conversations is sent.
const MAX_TRANSCRIPT_LEN: usize = 8 * 1024;
const MAX_RESPONSE_LEN: usize = 64 * 1024;
const MAX_MEMORY_CHARS: usize = 1500;

/// Returns the updated memory for `old_memory` after the given conversation.
pub async fn distill(
    stack: Stack<'_>,
    old_memory: &str,
    transcript: &str,
    date: Option<&str>,
) -> Result<String, Error> {
    let body = request_body(old_memory, tail(transcript, MAX_TRANSCRIPT_LEN), date);

    let mut buffers = tls::Buffers::new();
    let tls::Buffers {
        tcp_rx,
        tcp_tx,
        record_rx,
        record_tx,
    } = &mut buffers;
    let mut socket = tls::connect_tcp(stack, GEMINI_HOST, 443, tcp_rx, tcp_tx).await?;
    let halves = SocketHalves::new(&mut socket);
    let mut tls = tls::open_tls(halves.socket(), GEMINI_HOST, record_rx, record_tx).await?;

    let header = alloc::format!(
        "POST /v1beta/models/{MODEL}:generateContent HTTP/1.0\r\n\
         Host: {GEMINI_HOST}\r\n\
         x-goog-api-key: {GEMINI_API_KEY}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    );
    tls.write_all(header.as_bytes()).await?;
    tls.write_all(&body).await?;
    tls.flush().await?;

    let mut response = Vec::new();
    loop {
        let len = response.len();
        if len >= MAX_RESPONSE_LEN {
            break;
        }
        response.resize(len + 4096, 0);
        match tls.read(&mut response[len..]).await {
            Ok(0) => {
                response.truncate(len);
                break;
            }
            Ok(n) => response.truncate(len + n),
            Err(e) => {
                response.truncate(len);
                // Servers often end HTTP/1.0 responses without a TLS close_notify.
                if response.is_empty() {
                    return Err(e.into());
                }
                break;
            }
        }
    }

    parse_response(&response)
}

fn tail(text: &str, max_len: usize) -> &str {
    if text.len() <= max_len {
        return text;
    }
    let mut start = text.len() - max_len;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn request_body(old_memory: &str, transcript: &str, date: Option<&str>) -> Vec<u8> {
    let old_memory = if old_memory.trim().is_empty() {
        "(empty, this was the first conversation)"
    } else {
        old_memory
    };
    let date = date.unwrap_or("unknown");
    let prompt = alloc::format!(
        "You maintain the long-term memory of a small robot companion that chats with its user by voice.\n\n\
         Current memory:\n<memory>\n{old_memory}\n</memory>\n\n\
         Conversation that just happened (date: {date}). \"Robot\" is the companion, \"User\" is the user; \
         the text comes from speech recognition and may contain errors:\n<transcript>{transcript}\n</transcript>\n\n\
         Write the updated memory. Rules:\n\
         - Write it in German.\n\
         - Plain text, bullet points starting with \"- \", at most {MAX_MEMORY_CHARS} characters in total.\n\
         - First line: \"Letztes Gespräch: <date>\".\n\
         - Keep durable facts: the user's name, people and pets in their life, preferences, hobbies, work, important dates.\n\
         - Keep upcoming or ongoing plans and events with absolute dates (convert words like \"tomorrow\" using the \
           conversation date) so the robot can ask about them later.\n\
         - End with a \"Nachfragen:\" section listing one to three things the robot should ask about next time.\n\
         - Note how the user likes to be talked to, if that became apparent.\n\
         - Drop things that are outdated, resolved or trivial, and merge duplicates.\n\
         - If nothing new was learned, keep the memory and only update the first line.\n\
         Output only the memory text, nothing else."
    );

    json!({
        "contents": [{ "role": "user", "parts": [{ "text": prompt }] }],
        "generationConfig": { "temperature": 0.3, "responseMimeType": "text/plain" }
    })
    .to_string()
    .into_bytes()
}

fn parse_response(response: &[u8]) -> Result<String, Error> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(Error::Protocol("incomplete HTTP response"))?
        + 4;
    let status_line = response
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| core::str::from_utf8(line).ok())
        .unwrap_or("");
    let body = &response[header_end..];

    if !status_line.contains(" 200 ") {
        println!(
            "distill request failed: {} {}",
            status_line.trim(),
            core::str::from_utf8(body).unwrap_or("")
        );
        return Err(Error::Protocol("distill request failed"));
    }

    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| Error::Protocol("invalid distill JSON"))?;
    let mut text = String::new();
    if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if part["thought"].as_bool() == Some(true) {
                continue;
            }
            if let Some(t) = part["text"].as_str() {
                text.push_str(t);
            }
        }
    }

    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Protocol("empty distill response"));
    }
    Ok(String::from(text))
}
