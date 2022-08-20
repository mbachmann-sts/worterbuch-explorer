use crate::{config::Config, Connection};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::{future::Future, io};
use tokio::{net::TcpStream, spawn, sync::broadcast, sync::mpsc};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
    MaybeTlsStream, WebSocketStream,
};
use worterbuch_common::{
    encode_message,
    error::{ConnectionError, ConnectionResult},
    nonblocking::read_server_message,
    ClientMessage as CM, Handshake, ServerMessage as SM,
};

pub async fn connect_with_default_config<F: Future<Output = ()> + Send + 'static>(
    on_disconnect: F,
) -> ConnectionResult<(Connection, Config)> {
    let config = Config::new_ws()?;
    Ok((
        connect(&config.proto, &config.host_addr, config.port, on_disconnect).await?,
        config,
    ))
}

pub async fn connect<F: Future<Output = ()> + Send + 'static>(
    proto: &str,
    host_addr: &str,
    port: u16,
    on_disconnect: F,
) -> ConnectionResult<Connection> {
    let url = format!("{proto}://{host_addr}:{port}/ws");
    let (server, _) = connect_async(url).await?;
    let (ws_tx, mut ws_rx) = server.split();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (result_tx, result_rx) = broadcast::channel(1_000);

    match ws_rx.next().await {
        Some(Ok(msg)) => {
            let data = msg.into_data();
            match read_server_message(&*data).await? {
                Some(SM::Handshake(handshake)) => connected(
                    ws_tx,
                    ws_rx,
                    cmd_tx,
                    cmd_rx,
                    result_tx,
                    result_rx,
                    on_disconnect,
                    handshake,
                ),
                Some(other) => Err(ConnectionError::IoError(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("server sendt invalid handshake message: {other:?}"),
                ))),
                None => Err(ConnectionError::IoError(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "connection closed before handshake",
                ))),
            }
        }
        Some(Err(e)) => Err(e.into()),
        None => Err(ConnectionError::IoError(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "connection closed before handshake",
        ))),
    }
}

fn connected<F: Future<Output = ()> + Send + 'static>(
    mut ws_tx: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    mut ws_rx: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    cmd_tx: mpsc::UnboundedSender<CM>,
    mut cmd_rx: mpsc::UnboundedReceiver<CM>,
    result_tx: broadcast::Sender<SM>,
    result_rx: broadcast::Receiver<SM>,
    on_disconnect: F,
    handshake: Handshake,
) -> Result<Connection, ConnectionError> {
    let result_tx_recv = result_tx.clone();

    spawn(async move {
        while let Some(msg) = cmd_rx.recv().await {
            if let Ok(Some(data)) = encode_message(&msg).map(Some) {
                let msg = tungstenite::Message::Binary(data);
                if let Err(e) = ws_tx.send(msg).await {
                    log::error!("failed to send tcp message: {e}");
                    break;
                }
            } else {
                break;
            }
        }
        // make sure initial rx is not dropped as long as stdin is read
        drop(result_rx);
    });

    spawn(async move {
        loop {
            if let Some(Ok(incoming_msg)) = ws_rx.next().await {
                if incoming_msg.is_binary() {
                    let data = incoming_msg.into_data();
                    match read_server_message(&*data).await {
                        Ok(Some(msg)) => {
                            if let Err(e) = result_tx_recv.send(msg) {
                                log::error!("Error forwarding server message: {e}");
                            }
                        }
                        Ok(None) => {
                            log::error!("Connection to server lost.");
                            on_disconnect.await;
                            break;
                        }
                        Err(e) => {
                            log::error!("Error decoding message: {e}");
                        }
                    }
                }
            }
        }
    });

    let separator = handshake.separator;
    let wildcard = handshake.wildcard;
    let multi_wildcard = handshake.multi_wildcard;

    Ok(Connection::new(
        cmd_tx,
        result_tx,
        separator,
        wildcard,
        multi_wildcard,
    ))
}
