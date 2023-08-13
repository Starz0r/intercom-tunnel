use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::Write,
    net::IpAddr,
};

use cpal::traits::{HostTrait, StreamTrait};
use tokio::{
    net::{TcpListener, TcpStream},
    task::yield_now,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use {
    anyhow::{anyhow, bail, Error, Result},
    clap::{Parser, ValueEnum},
    serde_derive::Deserialize,
    tracing::{debug, error, info},
    tracing_subscriber::FmtSubscriber,
    tungstenite::protocol::Message,
};

#[derive(ValueEnum, Clone, Debug)]
enum OpMode {
    Transmit,
    Receive,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    mode: OpMode,
}

#[derive(Deserialize, Debug)]
enum Transport {
    Websocket,
}

#[derive(Deserialize, Debug)]
#[serde(rename = "transmit")]
struct TransmitConfig<'a> {
    #[serde(borrow)]
    device: Option<Cow<'a, str>>,
    addr: IpAddr,
    port: u16,
    transport: Option<Transport>,
}

#[derive(Deserialize, Debug)]
enum AuthMethod {
    Null,
}

#[derive(Deserialize, Debug)]
#[serde(rename = "receiver")]
struct ReceiverConfig<'a> {
    port: u16,
    auth: AuthMethod,
    #[serde(borrow)]
    devfile: Cow<'a, str>,
    transport: Option<Transport>,
}

#[derive(Deserialize, Debug)]
struct Config<'a> {
    #[serde(borrow)]
    receiver: Option<ReceiverConfig<'a>>,
    #[serde(borrow)]
    transmit: Option<TransmitConfig<'a>>,
}

async fn transmit_loop<'a>(cfg: TransmitConfig<'a>) -> Result<(), Error> {
    use cpal::traits::DeviceTrait;

    let audio_host = cpal::default_host();

    let dev = match cfg.device {
        Some(name) => audio_host
            .input_devices()?
            .find(|item| {
                item.name()
                    .map(|dev_name| dev_name == name)
                    .unwrap_or(false)
            })
            .expect({
                error!("specified input device could not be found");
                ""
            }),
        None => audio_host.default_input_device().expect({
            error!("system has no default input device");
            ""
        }),
    };

    let dev_cfg = dev.default_input_config()?;

    let (mut conn, _) =
        tokio_tungstenite::connect_async(&format!("ws://{}:{}", cfg.addr, cfg.port)).await?;

    let (p, mut c) = tokio::sync::mpsc::channel(1024);

    tokio::task::spawn(async move {
        use futures_util::sink::SinkExt;
        'f: loop {
            info!("transmitting message");
            match c.recv().await.unwrap() {
                Some(data) => conn.send(data).await.unwrap(),
                None => break 'f,
            }
        }
    });

    let dev_stream = dev.build_input_stream(
        &dev_cfg.into(),
        move |data, _: &_| {
            let data = data as &[f32];
            let mut msg: Vec<u8> = Vec::new();
            for s in data {
                msg.extend_from_slice(&s.to_le_bytes());
            }
            p.try_send(Some(Message::Binary(msg))).ok();
        },
        move |err| error!("device malfunction: {err}"),
        None,
    )?;

    dev_stream.play()?;

    tokio::signal::ctrl_c().await.unwrap_or_else(|_| {
        error!("failed to hook into the interrupt signal");
        panic!();
    });

    Ok(())
}

async fn receiver_loop<'a>(cfg: ReceiverConfig<'a>) -> Result<(), Error> {
    let try_socket = TcpListener::bind(format!("127.0.0.1:{}", cfg.port)).await;

    let listener = try_socket.unwrap_or_else(|_| {
        error!("socket was in use or unbindable");
        panic!();
    });
    info!("listening on: {}:{}", "127.0.0.1", cfg.port);

    while let Ok((stream, addr)) = listener.accept().await {
        let conn = tokio_tungstenite::accept_async(stream)
            .await
            .unwrap_or_else(|_| {
                error!("malformed websocket handshake");
                panic!();
            });

        let mut f = {
            if !std::path::Path::new(&*cfg.devfile).exists() {
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .append(true)
                    .open(&*cfg.devfile)?
            } else {
                std::fs::File::create(&*cfg.devfile)?
            }
        };

        use futures_util::stream::TryStreamExt;
        let read_msgs = conn.try_for_each(|msg| match msg {
            Message::Binary(bin) => {
                f.write_all(&bin).unwrap();
                futures_util::future::ready(Ok(()))
            }
            _ => futures_util::future::ready(Ok(())),
        });

        tokio::select! {
                _ = read_msgs => {},
                _ = tokio::signal::ctrl_c() => {f.flush(); return Ok(())}
        };
    }

    Ok(())
}

pub fn main() -> Result<(), Error> {
    let logging = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(logging)?;

    let argv = Args::parse();

    let mut cfg_dir = dirs::config_local_dir().unwrap_or_else(|| {
        error!("no configuration directory");
        panic!();
    });
    cfg_dir.push("intercom-tunnel.toml");

    let cfg_data = &fs::read(cfg_dir)?;
    let cfg: Config = toml::from_slice(cfg_data).unwrap();

    match argv.mode {
        OpMode::Transmit => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;

            rt.block_on(async {
                transmit_loop(cfg.transmit.unwrap_or_else(|| {
                    error!("transmit section in config is undefined");
                    panic!();
                }))
                .await?;

                Ok(())
            })
        }
        OpMode::Receive => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            rt.block_on(async {
                receiver_loop(cfg.receiver.unwrap_or_else(|| {
                    error!("receiver section in config is undefined");
                    panic!();
                }))
                .await?;

                Ok(())
            })
        }
    }
}
