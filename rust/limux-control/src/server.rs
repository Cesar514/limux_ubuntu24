// summary: Serve Limux control requests over Unix sockets.
// purpose: Bind authenticated local control sockets and dispatch bounded JSON requests.
// inputs: Socket path/mode, dispatcher state, request frames, and socket authorization policy.
// returns/effects: Runs an async accept loop, rejects unauthorized clients, and writes JSON responses.

use std::io;
use std::path::Path;
use std::sync::Arc;

use limux_protocol::{parse_v1_command_envelope, V2Request, V2Response};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use crate::auth::SocketControlMode;
use crate::request_io::{read_request_frame_async, MAX_CONNECTIONS};
use crate::socket_path::{bind_tokio_listener, SocketMode};
use crate::{auth, Dispatcher};

pub async fn run_server<P: AsRef<Path>>(
    socket_path: P,
    socket_mode: SocketMode,
    dispatcher: Dispatcher,
) -> io::Result<()> {
    let socket_path = socket_path.as_ref();
    let control_mode = auth::socket_control_mode_from_env()?;
    let listener = bind_tokio_listener(
        socket_path,
        socket_mode,
        control_mode.requires_owner_only_socket(),
    )?;
    serve_with_mode(listener, dispatcher, control_mode).await
}

pub async fn serve(listener: UnixListener, dispatcher: Dispatcher) -> io::Result<()> {
    let control_mode = auth::socket_control_mode_from_env()?;
    serve_with_mode(listener, dispatcher, control_mode).await
}

async fn serve_with_mode(
    listener: UnixListener,
    dispatcher: Dispatcher,
    control_mode: SocketControlMode,
) -> io::Result<()> {
    if control_mode.requires_password() {
        auth::configured_socket_password()?;
    }
    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        let (stream, _) = listener.accept().await?;
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                eprintln!("limux-control: rejecting client, too many active connections");
                continue;
            }
        };
        let peer = match auth::authorize_peer(&stream, control_mode) {
            Ok(peer) => peer,
            Err(error) => {
                eprintln!("limux-control: rejected client: {error}");
                continue;
            }
        };
        let dispatcher = dispatcher.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_connection(stream, dispatcher, control_mode).await {
                eprintln!(
                    "limux-control: connection error for pid={} uid={}: {error}",
                    peer.pid, peer.uid
                );
            }
        });
    }
}

pub async fn handle_connection(
    stream: UnixStream,
    dispatcher: Dispatcher,
    control_mode: SocketControlMode,
) -> io::Result<()> {
    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);
    let mut line_buf = Vec::with_capacity(4096);
    let mut authenticated = !control_mode.requires_password();

    loop {
        if !read_request_frame_async(&mut reader, &mut line_buf).await? {
            return Ok(());
        }

        let incoming = std::str::from_utf8(&line_buf)
            .map(|line| line.trim_end_matches(['\n', '\r']))
            .unwrap_or("");
        if incoming.is_empty() {
            continue;
        }

        let response = match parse_request(incoming) {
            Ok(request) => {
                if request.method == "auth.login" {
                    handle_auth_login(request, &mut authenticated)
                } else if !authenticated {
                    V2Response::error(None, -32001, "unauthorized: authentication required", None)
                } else {
                    dispatcher.dispatch(request).await
                }
            }
            Err(message) => V2Response::error(None, -32700, message, None),
        };

        let mut payload = serde_json::to_string(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        payload.push('\n');

        writer_half.write_all(payload.as_bytes()).await?;
        writer_half.flush().await?;
    }
}

fn handle_auth_login(request: V2Request, authenticated: &mut bool) -> V2Response {
    let Some(password) = request
        .params
        .get("password")
        .and_then(serde_json::Value::as_str)
    else {
        return V2Response::error(request.id, -32602, "auth.login requires password", None);
    };
    match auth::configured_socket_password() {
        Ok(expected) if auth::password_matches(password, &expected) => {
            *authenticated = true;
            V2Response::success(
                request.id,
                serde_json::json!({"ok": true, "authenticated": true}),
            )
        }
        Ok(_) => V2Response::error(request.id, -32001, "unauthorized: bad password", None),
        Err(error) => V2Response::error(
            request.id,
            -32002,
            format!("socket password is not configured: {error}"),
            None,
        ),
    }
}

fn parse_request(incoming: &str) -> Result<V2Request, String> {
    if let Ok(request) = serde_json::from_str::<V2Request>(incoming) {
        return Ok(request);
    }

    match parse_v1_command_envelope(incoming) {
        Ok(v1) => Ok(v1.into_v2_request(None)),
        Err(error) => Err(format!("invalid request payload: {error}")),
    }
}
