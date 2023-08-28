use std::{
    borrow::Cow, io::Write, net::IpAddr, ops::ControlFlow, os::windows::prelude::AsRawHandle,
    time::Duration,
};

use cpal::traits::{HostTrait, StreamTrait};
use tokio::{
    fs::{self, OpenOptions},
    io::BufWriter,
    net::{TcpListener, TcpStream},
    process,
    sync::mpsc::{Receiver, Sender},
    task::yield_now,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use {
    anyhow::{anyhow, bail, Error, Result},
    clap::{Parser, ValueEnum},
    either::{Left, Right},
    ffmpeg_sidecar::command::{ffmpeg_is_installed, FfmpegCommand},
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
    #[serde(borrow)]
    addr: Cow<'a, str>,
    port: u16,
    auth: AuthMethod,
    #[serde(borrow)]
    devfile: Cow<'a, str>,
    transport: Option<Transport>,
    #[serde(borrow)]
    reencode: Option<Cow<'a, str>>,
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
    let try_socket = TcpListener::bind(format!("{}:{}", cfg.addr, cfg.port)).await;

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

        // if we're reencoding, optionally spawn ffmpeg
        let mut reencoder = match cfg.reencode {
            Some(ref codec) => {
                /*let mut cmd = std::process::Command::new("ffmpeg");
                cmd.args([
                    "-ac",
                    "2",
                    "-ar",
                    "48000",
                    "-f",
                    "f32le",
                    "-i",
                    "pipe:0",
                    "-f",
                    codec,
                    &cfg.devfile,
                ]);
                cmd.stdin(std::process::Stdio::piped());

                let mut proc = cmd.spawn().unwrap_or_else(|e| {
                    error!("reencoder process failed to spawn, {e:?}");
                    panic!();
                });*/

                //Some(BufWriter::new(proc.stdin.take().unwrap()))
                //Some(proc.stdin.take().unwrap())

                if !ffmpeg_is_installed() {
                    error!("ffmpeg not detected, aborting...");
                    panic!();
                }
                info!("ffmpeg detected!");

                let mut cmd = FfmpegCommand::new()
                    .args(["-ac", "2", "-ar", "48000", "-f", "f32le"])
                    .input("pipe:0")
                    .args(["-f", codec, &cfg.devfile])
                    .spawn()
                    .unwrap_or_else(move |e| {
                        error!("ffmpeg sidecar failed to run: {e}");
                        panic!();
                    });

                let writer = cmd.take_stdin().unwrap();

                Left((cmd, writer))
            }
            None => Right({
                let mut open_opts = std::fs::OpenOptions::new();
                open_opts.write(true).append(true);
                if !std::path::Path::new(&*cfg.devfile).exists() {
                    open_opts.create_new(true).open(&*cfg.devfile)?
                } else {
                    open_opts.create_new(false).open(&*cfg.devfile)?
                }
            }),
        };

        use futures_util::stream::TryStreamExt;
        use tokio::io::AsyncWriteExt;
        let read_msgs = conn.try_for_each(move |ref msg| match msg {
            Message::Binary(bin) => {
                match reencoder {
                    Left((_, ref mut proc_in)) => {
                        proc_in.write_all(bin).unwrap();
                        proc_in.flush().unwrap();
                    }
                    Right(ref mut f) => {
                        f.write_all(&bin).unwrap();
                        f.sync_data().unwrap();
                    }
                };
                futures_util::future::ready(Ok(()))
            }
            _ => futures_util::future::ready(Ok(())),
        });

        tokio::select! {
                _ = read_msgs => {},
                _ = tokio::signal::ctrl_c() => {return Ok(())}
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

    let cfg_data = &std::fs::read(cfg_dir)?;
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
