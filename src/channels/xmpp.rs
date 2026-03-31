//! XMPP channel — native Prosody integration via STARTTLS.
//!
//! Implements the [`Channel`] trait for XMPP, connecting directly to an XMPP
//! server without external bridge processes. Supports direct messages,
//! MUC groupchat with mention detection, chat state notifications (XEP-0085),
//! and OOB media handling (XEP-0066).
//!
//! Protocol handling is done manually over `tokio-rustls` TLS streams,
//! matching the approach used by the IRC channel.

use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex as TokioMutex;
use tokio_rustls::rustls;

// ── Types ────────────────────────────────────────────────────────────────────

type TlsStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
type XmppWriteHalf = tokio::io::WriteHalf<TlsStream>;

// ── Constants ────────────────────────────────────────────────────────────────

/// Read timeout — if no data arrives within this duration, the connection
/// is considered dead. XMPP servers send whitespace keepalives.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Timeout for individual negotiation steps (STARTTLS, SASL, bind).
const NEGOTIATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Monotonic counter for unique message IDs.
static MSG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Global XMPP writer handle — shared between the channel listener and XMPP tools.
/// Initialised to `Some(writer)` when the channel connects; `None` before or after disconnect.
static XMPP_WRITER: OnceLock<Arc<TokioMutex<Option<XmppWriteHalf>>>> = OnceLock::new();

/// Style instruction prepended to every XMPP message before it reaches the LLM.
const XMPP_STYLE_PREFIX: &str = "\
[context: you are responding over XMPP. \
Keep responses concise. Plain text preferred but light formatting is OK. \
In MUC (group chat), address the person by name.]\n";

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct XmppConfig {
    /// Full JID for the bot (e.g. `sid@example.com`).
    pub jid: String,
    /// XMPP account password (injected from secret at activation time).
    pub password: String,
    /// Server hostname to connect to. Defaults to the JID domain.
    #[serde(default)]
    pub server: Option<String>,
    /// Server port. Default: 5222 (STARTTLS).
    #[serde(default = "default_xmpp_port")]
    pub port: u16,
    /// Whether to verify TLS certificates. Default: true.
    #[serde(default = "default_ssl_verify")]
    pub ssl_verify: bool,
    /// MUC room JIDs to auto-join on startup.
    #[serde(default)]
    pub muc_rooms: Vec<String>,
    /// Nick to use in MUC rooms. Defaults to capitalised JID local part.
    #[serde(default)]
    pub muc_nick: Option<String>,
}

fn default_xmpp_port() -> u16 {
    5222
}
fn default_ssl_verify() -> bool {
    true
}

// ── XmppChannel ──────────────────────────────────────────────────────────────

pub struct XmppChannel {
    jid: String,
    local_part: String,
    domain: String,
    password: String,
    server: String,
    port: u16,
    ssl_verify: bool,
    muc_rooms: Vec<String>,
    muc_nick: String,
    writer: Arc<TokioMutex<Option<XmppWriteHalf>>>,
    /// Maps reply_target (room bare JID) → last sender nick, for MUC response prefixing.
    muc_senders: Arc<parking_lot::Mutex<HashMap<String, String>>>,
}

impl XmppChannel {
    pub fn new(config: &XmppConfig) -> Self {
        let (local_part, domain) = config
            .jid
            .split_once('@')
            .unwrap_or(("sid", "localhost"));
        let server = config
            .server
            .clone()
            .unwrap_or_else(|| domain.to_string());
        let muc_nick = config.muc_nick.clone().unwrap_or_else(|| {
            let mut c = local_part.chars();
            match c.next() {
                None => "Bot".to_string(),
                Some(f) => f.to_uppercase().to_string() + c.as_str(),
            }
        });

        let writer = XMPP_WRITER
            .get_or_init(|| Arc::new(TokioMutex::new(None)))
            .clone();

        Self {
            jid: config.jid.clone(),
            local_part: local_part.to_string(),
            domain: domain.to_string(),
            password: config.password.clone(),
            server,
            port: config.port,
            ssl_verify: config.ssl_verify,
            muc_rooms: config.muc_rooms.clone(),
            muc_nick,
            writer,
            muc_senders: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    // ── Connection ───────────────────────────────────────────────────────

    /// Full connection sequence: TCP → STARTTLS → SASL PLAIN → resource bind → presence → MUC join.
    /// Returns the authenticated TLS stream ready for splitting.
    async fn connect_and_setup(&self) -> anyhow::Result<TlsStream> {
        let addr = format!("{}:{}", self.server, self.port);
        tracing::info!("XMPP connecting to {} as {}...", addr, self.jid);

        // ── Phase 1: TCP + STARTTLS ──────────────────────────────────────

        let mut tcp = tokio::net::TcpStream::connect(&addr).await?;

        let stream_open = format!(
            "<?xml version='1.0'?>\
             <stream:stream to='{}' xmlns='jabber:client' \
             xmlns:stream='http://etherx.jabber.org/streams' version='1.0'>",
            self.domain
        );
        tcp.write_all(stream_open.as_bytes()).await?;
        tcp.flush().await?;

        let response =
            read_until_contains(&mut tcp, "</stream:features>", NEGOTIATE_TIMEOUT).await?;
        if !response.contains("starttls") {
            anyhow::bail!("Server does not support STARTTLS: {response}");
        }

        tcp.write_all(b"<starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>")
            .await?;
        tcp.flush().await?;

        let response = read_until_contains(&mut tcp, "proceed", NEGOTIATE_TIMEOUT).await?;
        if response.contains("failure") {
            anyhow::bail!("STARTTLS rejected: {response}");
        }

        // ── TLS handshake ────────────────────────────────────────────────

        let tls_config = if self.ssl_verify {
            let root_store: rustls::RootCertStore =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        } else {
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth()
        };

        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
        // Use the JID domain for TLS SNI (server may be localhost/IP)
        let server_name = rustls::pki_types::ServerName::try_from(self.domain.clone())?;
        let mut tls = connector.connect(server_name, tcp).await?;
        tracing::info!("XMPP STARTTLS handshake complete");

        // ── Phase 2: SASL PLAIN over TLS ────────────────────────────────

        tls.write_all(stream_open.as_bytes()).await?;
        tls.flush().await?;

        let response =
            read_until_contains(&mut tls, "</stream:features>", NEGOTIATE_TIMEOUT).await?;
        if !response.contains("PLAIN") {
            anyhow::bail!("Server does not support SASL PLAIN: {response}");
        }

        // SASL PLAIN: \0<localpart>\0<password>
        let auth_str = format!("\0{}\0{}", self.local_part, self.password);
        let encoded = base64_encode(auth_str.as_bytes());
        let auth_xml = format!(
            "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>{encoded}</auth>"
        );
        tls.write_all(auth_xml.as_bytes()).await?;
        tls.flush().await?;

        // Read until we see success or failure (both contain the SASL namespace closing)
        let response = read_until_contains(&mut tls, "/>", NEGOTIATE_TIMEOUT).await?;
        if response.contains("failure") || response.contains("not-authorized") {
            anyhow::bail!("SASL authentication failed: {response}");
        }
        if !response.contains("success") {
            // Might be <success>...</success> instead of self-closing
            let response2 =
                read_until_contains(&mut tls, "success", NEGOTIATE_TIMEOUT).await?;
            if response2.contains("failure") {
                anyhow::bail!("SASL authentication failed: {response}{response2}");
            }
        }
        tracing::info!("XMPP SASL auth succeeded");

        // ── Phase 3: post-auth stream + resource bind ───────────────────

        tls.write_all(stream_open.as_bytes()).await?;
        tls.flush().await?;

        let _features =
            read_until_contains(&mut tls, "</stream:features>", NEGOTIATE_TIMEOUT).await?;

        let bind_xml = "<iq type='set' id='bind1'>\
                         <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'>\
                         <resource>zeroclaw</resource></bind></iq>";
        tls.write_all(bind_xml.as_bytes()).await?;
        tls.flush().await?;

        let response = read_until_contains(&mut tls, "</iq>", NEGOTIATE_TIMEOUT).await?;
        let bound_jid = extract_element_content(&response, "jid")
            .unwrap_or_else(|| format!("{}/zeroclaw", self.jid));
        tracing::info!("XMPP bound as {bound_jid}");

        // ── Phase 4: initial presence + MUC join ────────────────────────

        let presence = format!(
            "<presence><status>{}</status></presence>",
            xml_escape("Sid here - what's up?")
        );
        tls.write_all(presence.as_bytes()).await?;

        for room in &self.muc_rooms {
            let muc_presence = format!(
                "<presence to='{}/{}'>\
                 <x xmlns='http://jabber.org/protocol/muc'>\
                 <history maxstanzas='0'/>\
                 </x></presence>",
                room,
                xml_escape(&self.muc_nick)
            );
            tls.write_all(muc_presence.as_bytes()).await?;
            tracing::info!("XMPP joining MUC: {room}");
        }
        tls.flush().await?;

        Ok(tls)
    }

    // ── XML helpers ──────────────────────────────────────────────────────

    /// Send raw XML over the XMPP connection.
    async fn send_xml(writer: &mut XmppWriteHalf, xml: &str) -> anyhow::Result<()> {
        writer.write_all(xml.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    // ── Message handling ─────────────────────────────────────────────────

    async fn handle_message(
        &self,
        stanza: &str,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        let msg_type = extract_attr(stanza, "type").unwrap_or_default();
        let from = match extract_attr(stanza, "from") {
            Some(f) => f,
            None => return Ok(()),
        };

        // Skip MUC history replay (delayed messages from before we joined)
        if stanza.contains("urn:xmpp:delay") || stanza.contains("jabber:x:delay") {
            tracing::debug!("XMPP skipping delayed/history message");
            return Ok(());
        }

        let body = match extract_element_content(stanza, "body") {
            Some(b) => xml_unescape(&b),
            None => return Ok(()), // no body = chat state notification, skip
        };

        match msg_type.as_str() {
            "chat" => {
                // ── Direct message ───────────────────────────────────
                let bare_jid = from.split('/').next().unwrap_or(&from).to_string();

                let content = self.maybe_attach_oob(stanza, &body).await;
                let content = format!("{XMPP_STYLE_PREFIX}{content}");

                let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
                let channel_msg = ChannelMessage {
                    id: format!("xmpp_{}_{seq}", chrono::Utc::now().timestamp_millis()),
                    sender: bare_jid.clone(),
                    reply_target: bare_jid,
                    content,
                    channel: "xmpp".to_string(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                };
                let _ = tx.send(channel_msg).await;
            }

            "groupchat" => {
                // ── MUC message ──────────────────────────────────────
                let (room_jid, sender_nick) = match from.rsplit_once('/') {
                    Some((room, nick)) => (room.to_string(), nick.to_string()),
                    None => return Ok(()),
                };

                // Skip own reflected messages
                if sender_nick.eq_ignore_ascii_case(&self.muc_nick) {
                    return Ok(());
                }

                // Mention detection — only process if bot is mentioned
                let stripped_body = match detect_and_strip_mention(&body, &self.muc_nick) {
                    Some(stripped) => stripped,
                    None => return Ok(()),
                };

                let content = self.maybe_attach_oob(stanza, &stripped_body).await;
                let content = format!("{XMPP_STYLE_PREFIX}<{sender_nick}> {content}");

                // Store sender for response prefixing
                self.muc_senders
                    .lock()
                    .insert(room_jid.clone(), sender_nick.clone());

                let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
                let channel_msg = ChannelMessage {
                    id: format!("xmpp_muc_{}_{seq}", chrono::Utc::now().timestamp_millis()),
                    sender: sender_nick,
                    reply_target: room_jid,
                    content,
                    channel: "xmpp".to_string(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    thread_ts: None,
                    interruption_scope_id: None,
                    attachments: vec![],
                };
                let _ = tx.send(channel_msg).await;
            }

            _ => {} // ignore other types (error, headline, etc.)
        }

        Ok(())
    }

    // ── OOB media ────────────────────────────────────────────────────────

    /// If the stanza contains an OOB URL (XEP-0066), download supported media
    /// and return the body with an attachment note appended.
    async fn maybe_attach_oob(&self, stanza: &str, body: &str) -> String {
        if !stanza.contains("jabber:x:oob") {
            return body.to_string();
        }
        let url = match extract_element_content(stanza, "url") {
            Some(u) => u,
            None => return body.to_string(),
        };
        match self.download_oob_media(&url).await {
            Ok(path) => {
                let lower = path.to_lowercase();
                if lower.ends_with(".jpg")
                    || lower.ends_with(".jpeg")
                    || lower.ends_with(".png")
                    || lower.ends_with(".gif")
                    || lower.ends_with(".webp")
                {
                    format!("{body}\n[IMAGE:{path}]")
                } else {
                    format!("{body}\n[Attached file: {path}]")
                }
            }
            Err(e) => {
                tracing::warn!("OOB download failed for {url}: {e}");
                format!("{body}\n[Attached media: {url} — download failed: {e}]")
            }
        }
    }

    async fn download_oob_media(&self, url: &str) -> anyhow::Result<String> {
        let url_lower = url.to_lowercase();
        let (supported, max_size) =
            if url_lower.ends_with(".jpg")
                || url_lower.ends_with(".jpeg")
                || url_lower.ends_with(".png")
                || url_lower.ends_with(".gif")
                || url_lower.ends_with(".webp")
            {
                (true, 3_750_000u64)
            } else if url_lower.ends_with(".pdf") {
                (true, 32_000_000u64)
            } else {
                (false, 0u64)
            };

        if !supported {
            anyhow::bail!("unsupported file type");
        }

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!self.ssl_verify)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let response = client.get(url).send().await?;
        if let Some(len) = response.content_length() {
            if len > max_size {
                anyhow::bail!("file too large: {len} bytes (max {max_size})");
            }
        }

        let bytes = response.bytes().await?;
        if bytes.len() as u64 > max_size {
            anyhow::bail!(
                "file too large: {} bytes (max {max_size})",
                bytes.len()
            );
        }

        let extension = url.rsplit('.').next().unwrap_or("bin");
        let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xmpp_media_{}_{seq}.{extension}",
            chrono::Utc::now().timestamp_millis()
        );

        std::fs::write(&path, &bytes)?;
        Ok(path)
    }
}

// ── Channel trait ────────────────────────────────────────────────────────────

#[async_trait]
impl Channel for XmppChannel {
    fn name(&self) -> &str {
        "xmpp"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("XMPP not connected"))?;

        // Determine if recipient is a MUC room
        let is_muc = self
            .muc_rooms
            .iter()
            .any(|r| message.recipient.starts_with(r.split('/').next().unwrap_or(r)));

        // For MUC, prefix response with sender's nick
        let body = if is_muc {
            let prefix = self
                .muc_senders
                .lock()
                .get(&message.recipient)
                .cloned()
                .map(|nick| format!("{nick}: "))
                .unwrap_or_default();
            format!("{prefix}{}", message.content)
        } else {
            message.content.clone()
        };

        let msg_type = if is_muc { "groupchat" } else { "chat" };
        let stanza = format!(
            "<message type='{msg_type}' to='{}'>\
             <body>{}</body>\
             <active xmlns='http://jabber.org/protocol/chatstates'/>\
             </message>",
            xml_escape(&message.recipient),
            xml_escape(&body)
        );
        Self::send_xml(writer, &stanza).await
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let tls = self.connect_and_setup().await?;
        let (mut reader, writer) = tokio::io::split(tls);

        // Store writer for send() / tools
        {
            let mut guard = self.writer.lock().await;
            *guard = Some(writer);
        }

        // ── Message loop ─────────────────────────────────────────────────
        let mut stanza_buf = String::new();
        let mut read_buf = vec![0u8; 8192];

        loop {
            let n = tokio::time::timeout(READ_TIMEOUT, reader.read(&mut read_buf))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("XMPP read timed out (no data for {READ_TIMEOUT:?})")
                })??;

            if n == 0 {
                anyhow::bail!("XMPP connection closed by server");
            }

            stanza_buf.push_str(&String::from_utf8_lossy(&read_buf[..n]));

            let (stanzas, consumed) = extract_stanzas(&stanza_buf);
            if consumed > 0 {
                stanza_buf = stanza_buf[consumed..].to_string();
            }

            for stanza in stanzas {
                if stanza.contains("</stream:stream") || stanza.contains("stream:error") {
                    anyhow::bail!("XMPP stream closed: {stanza}");
                }

                if stanza.starts_with("<message") {
                    if let Err(e) = self.handle_message(&stanza, &tx).await {
                        tracing::warn!("Error handling XMPP message: {e}");
                    }
                }

                if stanza.starts_with("<presence") && stanza.contains("type=\"error\"") {
                    tracing::warn!("XMPP presence error: {stanza}");
                }
            }
        }
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().await;
        if let Some(ref mut writer) = *guard {
            let is_muc = self
                .muc_rooms
                .iter()
                .any(|r| recipient.starts_with(r.split('/').next().unwrap_or(r)));
            let msg_type = if is_muc { "groupchat" } else { "chat" };
            let stanza = format!(
                "<message type='{msg_type}' to='{}'>\
                 <composing xmlns='http://jabber.org/protocol/chatstates'/>\
                 </message>",
                xml_escape(recipient)
            );
            Self::send_xml(writer, &stanza).await?;
        }
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().await;
        if let Some(ref mut writer) = *guard {
            let is_muc = self
                .muc_rooms
                .iter()
                .any(|r| recipient.starts_with(r.split('/').next().unwrap_or(r)));
            let msg_type = if is_muc { "groupchat" } else { "chat" };
            let stanza = format!(
                "<message type='{msg_type}' to='{}'>\
                 <active xmlns='http://jabber.org/protocol/chatstates'/>\
                 </message>",
                xml_escape(recipient)
            );
            Self::send_xml(writer, &stanza).await?;
        }
        Ok(())
    }

    async fn health_check(&self) -> bool {
        let guard = self.writer.lock().await;
        guard.is_some()
    }
}

// ── TLS cert verifier (ssl_verify=false) ─────────────────────────────────────

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ── XML helpers ──────────────────────────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Extract the value of an XML attribute: `attr="value"` → `value`.
fn extract_attr(xml: &str, attr: &str) -> Option<String> {
    // Try both quote styles: attr="value" and attr='value'
    for quote in ['"', '\''] {
        let pattern = format!("{attr}={quote}");
        if let Some(start) = xml.find(&pattern) {
            let val_start = start + pattern.len();
            if let Some(end) = xml[val_start..].find(quote) {
                return Some(xml[val_start..val_start + end].to_string());
            }
        }
    }
    None
}

/// Extract text content of an XML element: `<tag>content</tag>` → `content`.
fn extract_element_content(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let tag_start = xml.find(&open)?;
    let content_start = xml[tag_start..].find('>')? + tag_start + 1;
    let content_end = xml[content_start..].find(&close)? + content_start;
    Some(xml[content_start..content_end].to_string())
}

/// Read from an async stream until the buffer contains `needle`.
async fn read_until_contains<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    needle: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    let mut buf = String::new();
    let mut tmp = vec![0u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Timeout waiting for '{needle}' in XMPP stream");
        }

        let n = tokio::time::timeout(remaining, stream.read(&mut tmp))
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for '{needle}'"))??;

        if n == 0 {
            anyhow::bail!("Connection closed while waiting for '{needle}'");
        }

        buf.push_str(&String::from_utf8_lossy(&tmp[..n]));

        if buf.contains(needle) {
            return Ok(buf);
        }
    }
}

/// Extract complete XML stanzas from a buffer.
/// Returns `(stanzas, bytes_consumed)`.
fn extract_stanzas(buf: &str) -> (Vec<String>, usize) {
    let mut stanzas = Vec::new();
    let mut consumed = 0;
    let mut remaining = buf;

    loop {
        let trimmed = remaining.trim_start();
        if trimmed.is_empty() || !trimmed.starts_with('<') {
            break;
        }
        let offset = remaining.len() - trimmed.len();

        // Skip XML declarations
        if trimmed.starts_with("<?xml") {
            if let Some(end) = trimmed.find("?>") {
                consumed += offset + end + 2;
                remaining = &buf[consumed..];
                continue;
            }
            break;
        }

        // Skip stream opening (not a complete element)
        if trimmed.starts_with("<stream:stream") {
            if let Some(end) = trimmed.find('>') {
                consumed += offset + end + 1;
                remaining = &buf[consumed..];
                continue;
            }
            break;
        }

        // Stream close
        if trimmed.starts_with("</stream:stream") {
            if let Some(end) = trimmed.find('>') {
                let len = offset + end + 1;
                stanzas.push(trimmed[..end + 1].to_string());
                consumed += len;
                remaining = &buf[consumed..];
                continue;
            }
            break;
        }

        // Find tag name
        let tag_end = match trimmed[1..].find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        {
            Some(i) => i + 1,
            None => break,
        };
        let tag_name = &trimmed[1..tag_end];

        // Find first '>'
        let first_gt = match trimmed.find('>') {
            Some(p) => p,
            None => break,
        };

        // Self-closing: <tag ... />
        if first_gt > 0 && trimmed.as_bytes()[first_gt - 1] == b'/' {
            let len = offset + first_gt + 1;
            stanzas.push(trimmed[..first_gt + 1].to_string());
            consumed += len;
            remaining = &buf[consumed..];
            continue;
        }

        // Find closing tag </tag>
        let closing = format!("</{tag_name}>");
        if let Some(close_pos) = trimmed.find(&closing) {
            let end = close_pos + closing.len();
            stanzas.push(trimmed[..end].to_string());
            consumed += offset + end;
            remaining = &buf[consumed..];
            continue;
        }

        // Incomplete stanza — wait for more data
        break;
    }

    (stanzas, consumed)
}

// ── Mention detection ────────────────────────────────────────────────────────

/// Detect and strip a mention of `nick` from the message body.
/// Returns `Some(stripped_body)` if a mention was found, `None` otherwise.
///
/// Matches (case-insensitive):
/// - `@nick` anywhere
/// - `nick:` at start or after whitespace
/// - bare `nick` at a word boundary
fn detect_and_strip_mention(body: &str, nick: &str) -> Option<String> {
    let nick_lower = nick.to_lowercase();
    let body_lower = body.to_lowercase();

    // Pattern 1: @nick
    let at_pattern = format!("@{nick_lower}");
    if let Some(pos) = body_lower.find(&at_pattern) {
        let end = pos + at_pattern.len();
        // Check word boundary after mention
        if end >= body.len() || !body.as_bytes()[end].is_ascii_alphanumeric() {
            let before = &body[..pos];
            let after = if end < body.len() { &body[end..] } else { "" };
            let result = format!("{before}{after}").trim().to_string();
            return Some(if result.is_empty() {
                body.to_string()
            } else {
                result
            });
        }
    }

    // Pattern 2: nick: (at start or after whitespace)
    let colon_pattern = format!("{nick_lower}:");
    if let Some(pos) = body_lower.find(&colon_pattern) {
        if pos == 0 || body.as_bytes()[pos - 1].is_ascii_whitespace() {
            let end = pos + colon_pattern.len();
            let before = &body[..pos];
            let after = if end < body.len() { &body[end..] } else { "" };
            let result = format!("{before}{after}").trim().to_string();
            return Some(if result.is_empty() {
                body.to_string()
            } else {
                result
            });
        }
    }

    // Pattern 3: bare nick at word boundary
    let pattern = format!(r"(?i)\b{}\b", regex::escape(&nick));
    if let Ok(re) = regex::Regex::new(&pattern) {
        if re.is_match(body) {
            let stripped = re.replace(body, "").trim().to_string();
            return Some(if stripped.is_empty() {
                body.to_string()
            } else {
                stripped
            });
        }
    }

    None
}

// ── Base64 (inline, avoids import style issues across base64 versions) ───────

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(CHARS[(triple >> 18 & 0x3F) as usize] as char);
        out.push(CHARS[(triple >> 12 & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(CHARS[(triple >> 6 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

// ── XMPP Tools ───────────────────────────────────────────────────────────────

pub struct XmppSendMessageTool;

#[async_trait]
impl Tool for XmppSendMessageTool {
    fn name(&self) -> &str {
        "xmpp_send_message"
    }
    fn description(&self) -> &str {
        "Send an XMPP message to a JID or MUC room"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Recipient JID or room JID" },
                "body": { "type": "string", "description": "Message text" },
                "type": { "type": "string", "enum": ["chat", "groupchat"],
                           "description": "Message type (auto-detected if omitted)" }
            },
            "required": ["to", "body"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let to = args["to"].as_str().unwrap_or("");
        let body = args["body"].as_str().unwrap_or("");
        let msg_type = args
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_else(|| {
                if to.contains("conference") || to.contains("muc") {
                    "groupchat"
                } else {
                    "chat"
                }
            });

        let writer_arc = match XMPP_WRITER.get() {
            Some(w) => w.clone(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("XMPP not connected".into()),
                })
            }
        };

        let mut guard = writer_arc.lock().await;
        match guard.as_mut() {
            Some(writer) => {
                let stanza = format!(
                    "<message type='{msg_type}' to='{}'><body>{}</body></message>",
                    xml_escape(to),
                    xml_escape(body)
                );
                XmppChannel::send_xml(writer, &stanza).await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Message sent to {to}"),
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("XMPP not connected".into()),
            }),
        }
    }
}

pub struct XmppJoinRoomTool;

#[async_trait]
impl Tool for XmppJoinRoomTool {
    fn name(&self) -> &str {
        "xmpp_join_room"
    }
    fn description(&self) -> &str {
        "Join an XMPP MUC room"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "room": { "type": "string", "description": "Room JID (e.g. room@conference.example.com)" },
                "nick": { "type": "string", "description": "Nickname to use (defaults to configured muc_nick)" }
            },
            "required": ["room"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let room = args["room"].as_str().unwrap_or("");
        let nick = args.get("nick").and_then(|n| n.as_str()).unwrap_or("Sid");

        let writer_arc = match XMPP_WRITER.get() {
            Some(w) => w.clone(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("XMPP not connected".into()),
                })
            }
        };

        let mut guard = writer_arc.lock().await;
        match guard.as_mut() {
            Some(writer) => {
                let stanza = format!(
                    "<presence to='{}/{}'>\
                     <x xmlns='http://jabber.org/protocol/muc'>\
                     <history maxstanzas='0'/>\
                     </x></presence>",
                    xml_escape(room),
                    xml_escape(nick)
                );
                XmppChannel::send_xml(writer, &stanza).await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Joined room {room} as {nick}"),
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("XMPP not connected".into()),
            }),
        }
    }
}

pub struct XmppLeaveRoomTool;

#[async_trait]
impl Tool for XmppLeaveRoomTool {
    fn name(&self) -> &str {
        "xmpp_leave_room"
    }
    fn description(&self) -> &str {
        "Leave an XMPP MUC room"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "room": { "type": "string", "description": "Room JID to leave" }
            },
            "required": ["room"]
        })
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let room = args["room"].as_str().unwrap_or("");

        let writer_arc = match XMPP_WRITER.get() {
            Some(w) => w.clone(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("XMPP not connected".into()),
                })
            }
        };

        let mut guard = writer_arc.lock().await;
        match guard.as_mut() {
            Some(writer) => {
                let stanza = format!(
                    "<presence to='{}' type='unavailable'/>",
                    xml_escape(room)
                );
                XmppChannel::send_xml(writer, &stanza).await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Left room {room}"),
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("XMPP not connected".into()),
            }),
        }
    }
}

pub struct XmppListRoomsTool;

#[async_trait]
impl Tool for XmppListRoomsTool {
    fn name(&self) -> &str {
        "xmpp_list_rooms"
    }
    fn description(&self) -> &str {
        "List available XMPP MUC rooms via service discovery (disco#items)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "service": { "type": "string",
                             "description": "MUC service JID (defaults to conference.<domain>)" }
            }
        })
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // disco#items requires sending an IQ and waiting for the response, which
        // needs bidirectional access (send on writer, receive on reader). The
        // reader is owned by the listen loop. This would require a response
        // channel or shared IQ tracking — deferred to a future iteration.
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(
                "xmpp_list_rooms requires IQ query support (not yet implemented). \
                 Use xmpp_join_room with a known room JID instead."
                    .into(),
            ),
        })
    }
}

pub struct XmppSetPresenceTool;

#[async_trait]
impl Tool for XmppSetPresenceTool {
    fn name(&self) -> &str {
        "xmpp_set_presence"
    }
    fn description(&self) -> &str {
        "Update XMPP presence status"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "description": "Status text" },
                "show": { "type": "string", "enum": ["available", "away", "dnd", "xa"],
                           "description": "Availability (default: available)" }
            }
        })
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let status = args.get("status").and_then(|s| s.as_str()).unwrap_or("");
        let show = args
            .get("show")
            .and_then(|s| s.as_str())
            .unwrap_or("available");

        let writer_arc = match XMPP_WRITER.get() {
            Some(w) => w.clone(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("XMPP not connected".into()),
                })
            }
        };

        let mut guard = writer_arc.lock().await;
        match guard.as_mut() {
            Some(writer) => {
                let show_elem = if show != "available" {
                    format!("<show>{}</show>", xml_escape(show))
                } else {
                    String::new()
                };
                let status_elem = if !status.is_empty() {
                    format!("<status>{}</status>", xml_escape(status))
                } else {
                    String::new()
                };
                let stanza = format!("<presence>{show_elem}{status_elem}</presence>");
                XmppChannel::send_xml(writer, &stanza).await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Presence updated: show={show}, status={status}"),
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("XMPP not connected".into()),
            }),
        }
    }
}

/// Create XMPP tool instances for the tool registry.
pub fn xmpp_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(XmppSendMessageTool),
        Arc::new(XmppJoinRoomTool),
        Arc::new(XmppLeaveRoomTool),
        Arc::new(XmppListRoomsTool),
        Arc::new(XmppSetPresenceTool),
    ]
}
