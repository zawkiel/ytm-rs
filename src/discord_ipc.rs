//! Discord's local RPC transport. One task owns the connection and only sends a
//! card on a playback/settings change or after reconnecting.

use std::io;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use super::{Card, activity};

const RETRY: Duration = Duration::from_secs(5);
const SEND_INTERVAL: Duration = Duration::from_millis(250);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_FRAME: usize = 64 * 1024;
const HANDSHAKE: u32 = 0;
const FRAME: u32 = 1;
const CLOSE: u32 = 2;
const PING: u32 = 3;
const PONG: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Waiting,
    Connected,
    Rejected,
}

pub struct Discord {
    pub id: String,
    desired: watch::Sender<Option<Card>>,
    connection: watch::Receiver<Connection>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Discord {
    pub fn spawn(id: String, backend: &crate::backend::Backend, ctx: &egui::Context) -> Self {
        let (desired, updates) = watch::channel(None);
        let (state, connection) = watch::channel(Connection::Waiting);
        let (finished, done) = std::sync::mpsc::sync_channel(1);
        let ctx = ctx.clone();
        let client_id = id.clone();
        backend.spawn(async move {
            run(&client_id, updates, |next| {
                if state.send_if_modified(|current| {
                    let changed = *current != next;
                    *current = next;
                    changed
                }) {
                    ctx.request_repaint();
                }
            })
            .await;
            let _ = finished.send(());
        });
        Self { id, desired, connection, done }
    }

    pub fn set(&self, card: Option<Card>) {
        self.desired.send_replace(card);
    }

    pub fn connection(&self) -> Connection {
        *self.connection.borrow()
    }

    /// Dropping the sender tells the worker to clear the card and close its pipe.
    pub fn close(self) {
        drop(self.desired);
        let _ = self.done.recv_timeout(Duration::from_millis(300));
    }
}

pub fn application_id(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && text.parse::<u64>().is_ok_and(|id| id != 0))
        .then_some(text)
}

#[cfg(windows)]
async fn connect() -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;

    for n in 0..10 {
        if let Ok(pipe) = ClientOptions::new().open(format!(r"\\?\pipe\discord-ipc-{n}")) {
            return Ok(pipe);
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

#[cfg(unix)]
async fn connect() -> io::Result<tokio::net::UnixStream> {
    use std::path::PathBuf;
    use tokio::net::UnixStream;

    let roots = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .chain([PathBuf::from("/tmp")]);
    for root in roots {
        for base in [root.clone(), root.join("app/com.discordapp.Discord")] {
            for n in 0..10 {
                if let Ok(socket) = UnixStream::connect(base.join(format!("discord-ipc-{n}"))).await {
                    return Ok(socket);
                }
            }
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

async fn run(
    id: &str,
    mut updates: watch::Receiver<Option<Card>>,
    report: impl Fn(Connection),
) {
    loop {
        if updates.has_changed().is_err() {
            return;
        }
        let mut rejected = false;
        if let Ok(Ok(pipe)) = timeout(IO_TIMEOUT, connect()).await {
            let (reader, writer) = tokio::io::split(pipe);
            let mut reader = tokio::io::BufReader::new(reader);
            let mut writer = writer;
            match timeout(IO_TIMEOUT, handshake(id, &mut reader, &mut writer)).await {
                Ok(Ok(true)) => {
                    report(Connection::Connected);
                    match session(&mut updates, reader, writer, &report).await {
                        Ok(()) => return,
                        Err(e) => log::debug!("discord: IPC disconnected: {e}"),
                    }
                }
                Ok(Ok(false)) => rejected = true,
                _ => {}
            }
        }
        report(if rejected { Connection::Rejected } else { Connection::Waiting });
        // Changes while Discord is closed are coalesced without restarting the retry timer.
        let retry = sleep(RETRY);
        tokio::pin!(retry);
        loop {
            tokio::select! {
                _ = &mut retry => break,
                change = updates.changed() => {
                    if change.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

async fn handshake(
    id: &str,
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> io::Result<bool> {
    write_frame(writer, HANDSHAKE, &serde_json::to_vec(&json!({ "v": 1, "client_id": id }))?).await?;
    loop {
        let (opcode, data) = read_frame(reader).await?;
        match opcode {
            FRAME => return Ok(serde_json::from_slice::<Value>(&data)?["evt"] == "READY"),
            CLOSE => return Ok(false),
            PING => write_frame(writer, PONG, &data).await?,
            _ => {}
        }
    }
}

async fn session(
    updates: &mut watch::Receiver<Option<Card>>,
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    report: &impl Fn(Connection),
) -> io::Result<()> {
    let mut dirty = updates.borrow_and_update().is_some();
    let mut sent_activity = false;
    let mut next_send = Instant::now();
    let mut nonce = 0_u64;
    loop {
        // Keep the read future alive across updates, including a partially read frame.
        let frame = read_frame(&mut reader);
        tokio::pin!(frame);
        let (opcode, data) = loop {
            tokio::select! {
                result = &mut frame => break result?,
                change = updates.changed() => {
                    if change.is_err() {
                        if sent_activity {
                            let _ = send_activity(&mut writer, None, nonce + 1).await;
                        }
                        return Ok(());
                    }
                    dirty = true;
                }
                _ = sleep_until(next_send), if dirty => {
                    let desired = updates.borrow_and_update().clone();
                    if desired.is_some() || sent_activity {
                        nonce += 1;
                        send_activity(&mut writer, desired.as_ref(), nonce).await?;
                        sent_activity = desired.is_some();
                        next_send = Instant::now() + SEND_INTERVAL;
                    }
                    dirty = false;
                }
            }
        };
        match opcode {
            FRAME => {
                let reply: Value = serde_json::from_slice(&data)?;
                if reply["cmd"] == "SET_ACTIVITY" && reply["nonce"].as_str() == Some(nonce.to_string().as_str()) {
                    let rejected = reply["evt"] == "ERROR";
                    if rejected {
                        log::warn!("discord: SET_ACTIVITY rejected (code {})", reply["data"]["code"]);
                    }
                    report(if rejected { Connection::Rejected } else { Connection::Connected });
                }
            }
            CLOSE => return Err(io::ErrorKind::ConnectionAborted.into()),
            PING => timeout(IO_TIMEOUT, write_frame(&mut writer, PONG, &data)).await??,
            _ => {}
        }
    }
}

async fn send_activity(
    writer: &mut (impl AsyncWrite + Unpin),
    card: Option<&Card>,
    nonce: u64,
) -> io::Result<()> {
    let data = serde_json::to_vec(&json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": std::process::id(), "activity": card.map(activity) },
        "nonce": nonce.to_string(),
    }))?;
    timeout(IO_TIMEOUT, write_frame(writer, FRAME, &data)).await?
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), opcode: u32, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(8 + data.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
    frame.extend_from_slice(data);
    writer.write_all(&frame).await
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<(u32, Vec<u8>)> {
    let opcode = reader.read_u32_le().await?;
    let len = reader.read_u32_le().await? as usize;
    if len > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut data = vec![0; len];
    reader.read_exact(&mut data).await?;
    Ok((opcode, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_ids_are_nonzero_decimal_snowflakes() {
        assert_eq!(application_id(" 1521756469433991299 "), Some("1521756469433991299"));
        for bad in ["", "0", "+1234", "12 34", "18446744073709551616"] {
            assert_eq!(application_id(bad), None);
        }
    }

    #[tokio::test]
    async fn publishes_and_clears_on_shutdown() {
        let (app, mut server) = tokio::io::duplex(4096);
        let (tx, mut rx) = watch::channel(Some(Card::default()));
        let (read, write) = tokio::io::split(app);
        let verify = async move {
            let (opcode, data) = read_frame(&mut server).await.unwrap();
            assert_eq!(opcode, FRAME);
            assert_eq!(serde_json::from_slice::<Value>(&data).unwrap()["args"]["activity"]["type"], 2);
            drop(tx);
            let (_, data) = read_frame(&mut server).await.unwrap();
            assert!(serde_json::from_slice::<Value>(&data).unwrap()["args"]["activity"].is_null());
        };
        let (result, ()) = timeout(IO_TIMEOUT, async {
            tokio::join!(session(&mut rx, read, write, &|_| {}), verify)
        }).await.unwrap();
        result.unwrap();
    }
}
