//! Local WebSocket server that receives RTVI/Pipecat events. Strictly an
//! optional overlay: the app is fully functional when nothing connects.
//! Events are stamped with the app clock on arrival (sub-ms on localhost).

use super::protocol::{parse, Parsed};
use crate::clock;
use crate::session::{BridgeEvent, Phase, SharedState};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Join the accept thread (it polls `stop` every ≤250 ms) so the
        // listener socket is closed before a restart rebinds the port.
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn start(port: u16, shared: SharedState) -> Result<BridgeHandle, String> {
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("bind port {port}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut sh = shared.lock();
        sh.bridge.running = true;
        sh.bridge.port = port;
        sh.bridge.clients = 0;
        sh.bridge.error = None;
    }

    let stop2 = stop.clone();
    let shared2 = shared.clone();
    let join = std::thread::Builder::new()
        .name("bridge-accept".into())
        .spawn(move || {
            accept_loop(listener, shared2, stop2);
        })
        .map_err(|e| format!("spawn bridge thread: {e}"))?;

    Ok(BridgeHandle {
        stop,
        join: Some(join),
    })
}

fn accept_loop(listener: TcpListener, shared: SharedState, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let shared = shared.clone();
                let stop = stop.clone();
                let _ = std::thread::Builder::new()
                    .name("bridge-conn".into())
                    .spawn(move || handle_conn(stream, shared, stop));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
    let mut sh = shared.lock();
    sh.bridge.running = false;
    sh.bridge.clients = 0;
}

fn handle_conn(stream: TcpStream, shared: SharedState, stop: Arc<AtomicBool>) {
    stream.set_nonblocking(false).ok();
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok();
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    shared.lock().bridge.clients += 1;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match ws.read() {
            Ok(msg) => {
                let recv_ns = clock::now_ns();
                match msg {
                    tungstenite::Message::Text(text) => {
                        handle_text(&text, recv_ns, &shared, &mut ws);
                    }
                    tungstenite::Message::Ping(p) => {
                        let _ = ws.send(tungstenite::Message::Pong(p));
                    }
                    tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let mut sh = shared.lock();
    sh.bridge.clients = sh.bridge.clients.saturating_sub(1);
}

fn handle_text(
    text: &str,
    recv_ns: u64,
    shared: &SharedState,
    ws: &mut tungstenite::WebSocket<TcpStream>,
) {
    match parse(text) {
        Parsed::Event { name, source, meta } => {
            let mut sh = shared.lock();
            sh.bridge.last_event = Some((name.clone(), recv_ns));
            if sh.phase == Phase::Running {
                sh.events.push(BridgeEvent {
                    t_ns: recv_ns,
                    name,
                    source,
                    meta,
                });
            }
        }
        Parsed::Ping => {
            let pong = format!(
                r#"{{"type":"pong","t_recv_ms":{:.3}}}"#,
                recv_ns as f64 / 1e6
            );
            let _ = ws.send(tungstenite::Message::Text(pong));
        }
        Parsed::Hello => {
            let _ = ws.send(tungstenite::Message::Text(
                r#"{"type":"hello","app":"pipecat-audio-metrics","v":1}"#.into(),
            ));
        }
        Parsed::Ignored => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::session;

    #[test]
    fn websocket_event_roundtrip() {
        let shared = session::new_shared(Config::default());
        shared.lock().phase = Phase::Running;

        // Find a free port, then start the bridge on it.
        let port = {
            let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let _handle = start(port, shared.clone()).expect("bridge start");

        let (mut ws, _resp) = tungstenite::connect(format!("ws://127.0.0.1:{port}"))
            .expect("client connect");
        ws.send(tungstenite::Message::Text(
            r#"{"v":1,"type":"event","name":"user-stopped-speaking","source":"test","meta":{"x":1}}"#
                .into(),
        ))
        .unwrap();
        ws.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into()))
            .unwrap();
        // Pong proves the event (sent first) was processed.
        let reply = ws.read().expect("pong");
        assert!(reply.to_string().contains("pong"), "got {reply:?}");

        let sh = shared.lock();
        assert_eq!(sh.bridge.clients, 1);
        assert_eq!(sh.events.len(), 1);
        assert_eq!(sh.events[0].name, "user_stopped_speaking");
        assert_eq!(sh.events[0].source, "test");
        assert!(sh.bridge.last_event.is_some());
    }
}
