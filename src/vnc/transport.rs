//! Byte streams to a VM's VNC server:
//! - `qm vncproxy` (local or over ssh) with a one-time password via LC_PVE_TICKET
//! - the PVE API websocket (vncproxy + vncwebsocket), for API-only sessions
//! - a raw unix socket (debugging)

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::api::Api;
use crate::exec::Exec;
use crate::util::{js, urlenc};

pub struct Conn {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub password: Option<String>,
    pub child: Option<ChildGuard>,
    pub via: String,
}

/// Kills the proxy process when the console closes.
pub struct ChildGuard(pub Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `qm vncproxy` sets the VNC password from LC_PVE_TICKET (valid for 30 s) and proxies stdio.
pub fn qm_vncproxy(exec: &Exec, vmid: u32) -> Result<Conn> {
    let pw = crate::util::random_token(8);
    let ticket = format!("LC_PVE_TICKET={pw}");
    let id = vmid.to_string();
    let mut cmd = match exec {
        Exec::Local => {
            let mut c = exec.command("qm", &["vncproxy", &id]);
            c.env("LC_PVE_TICKET", &pw);
            c
        }
        _ => exec.command("env", &[&ticket, "qm", "vncproxy", &id]),
    };
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("starting qm vncproxy")?;
    let reader = Box::new(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?);
    let writer = Box::new(child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?);
    Ok(Conn { reader, writer, password: Some(pw), child: Some(ChildGuard(child)), via: format!("qm vncproxy ({})", exec.label()) })
}

pub fn unix_socket(path: &str) -> Result<Conn> {
    let s = std::os::unix::net::UnixStream::connect(path).with_context(|| format!("connecting {path}"))?;
    let r = s.try_clone()?;
    Ok(Conn { reader: Box::new(r), writer: Box::new(s), password: None, child: None, via: format!("unix:{path}") })
}

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct ChanWriter {
    tx: Sender<Vec<u8>>,
}

impl Write for ChanWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.tx.send(b.to_vec()).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "websocket closed"))?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// VNC over the PVE API websocket (works for any user with VM.Console, also remotely).
pub fn api_websocket(api: &Arc<Api>, node: &str, vmid: u32) -> Result<Conn> {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::Message;

    let auth = api.auth().ok_or_else(|| anyhow!("graphical console over the API needs an API session"))?;
    let base = format!("/nodes/{node}/qemu/{vmid}");
    let v = api.post(&format!("{base}/vncproxy"), &[("websocket", "1".into())])?;
    let port = js(&v, "port");
    let ticket = js(&v, "ticket");
    if port.is_empty() || ticket.is_empty() {
        bail!("vncproxy returned no ticket");
    }

    let host = api.host.clone();
    let tcp = TcpStream::connect((host.as_str(), api.port)).with_context(|| format!("connecting {host}:{}", api.port))?;
    tcp.set_nodelay(true).ok();
    let sname = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|e| anyhow!("{e}"))?;
    let conn = rustls::ClientConnection::new(api.tls.clone(), sname)?;
    let tls = rustls::StreamOwned::new(conn, tcp);

    let url = format!(
        "wss://{}:{}/api2/json{base}/vncwebsocket?port={port}&vncticket={}",
        if host.contains(':') { format!("[{host}]") } else { host.clone() },
        api.port,
        urlenc(&ticket)
    );
    let mut req = url.into_client_request()?;
    req.headers_mut().insert("Cookie", format!("PVEAuthCookie={}", auth.ticket).parse()?);
    req.headers_mut().insert("Sec-WebSocket-Protocol", "binary".parse()?);
    let (mut ws, _) = tungstenite::client::client(req, tls).map_err(|e| anyhow!("websocket: {e}"))?;
    ws.get_mut().sock.set_read_timeout(Some(Duration::from_millis(20)))?;

    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || loop {
        loop {
            match out_rx.try_recv() {
                Ok(d) => {
                    if ws.send(Message::Binary(d.into())).is_err() {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    return;
                }
            }
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                if in_tx.send(b.to_vec()).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                let _ = ws.flush();
            }
            Err(_) => return,
        }
    });
    Ok(Conn {
        reader: Box::new(ChanReader { rx: in_rx, buf: Vec::new(), pos: 0 }),
        writer: Box::new(ChanWriter { tx: out_tx }),
        password: Some(ticket),
        child: None,
        via: "API websocket".into(),
    })
}
