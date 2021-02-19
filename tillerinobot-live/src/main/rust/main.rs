#![feature(drain_filter)]

#[macro_use]
extern crate lazy_static;

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use lapin::{Connection, ConnectionProperties, Consumer, ExchangeKind, message::Delivery, options::*, Result, types::FieldTable};
use rand::{ChaChaRng, FromEntropy, RngCore};
use serde::{Deserialize, Serialize};
use serde_json;
use sha2::Digest;
use tokio_amqp::LapinTokioExt;
use tungstenite::{error::Error::SendQueueFull, Message, protocol::WebSocketConfig, server::accept_with_config, WebSocket};

use futures_util::stream::StreamExt;

struct Conn {
    web: WebSocket<TcpStream>,
    salt: u64,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "@type")]
enum LiveActivityMessage {
    #[serde(rename="RECEIVED")] Received { #[serde(rename="eventId")] event_id: u64, #[serde(rename="ircUserName")] irc_user_name: String },
    #[serde(rename="SENT")] Sent { #[serde(rename="eventId")] event_id: u64, #[serde(rename="ircUserName")] irc_user_name: String, ping: Option<i32> },
    #[serde(rename="RECEIVED_DETAILS")] ReceivedDetails { #[serde(rename="eventId")] event_id: u64, text: String }
}

#[derive(Serialize, Debug)]
enum FrontendMessage {
    #[serde(rename="received")] Received { #[serde(rename="eventId")] event_id: u64, user: i32 },
    #[serde(rename="sent")] Sent { #[serde(rename="eventId")] event_id: u64, user: i32, ping: Option<i32> },
    #[serde(rename="messageDetails")] MessageDetails { #[serde(rename="eventId")] event_id: u64, message: String }
}


lazy_static! {
    static ref CONNECTIONS: Mutex<Vec<Conn>> = Mutex::new(vec![]);
}

static WEBSOCKET_CONFIG: WebSocketConfig = WebSocketConfig { max_send_queue: None, max_message_size: None, max_frame_size: Some(1000), accept_unmasked_frames: false };

#[tokio::main]
async fn main() -> Result<()> {

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    tokio::spawn(run_rabbit());

    let server = TcpListener::bind("0.0.0.0:8080").unwrap();
    let rnd = Arc::new(Mutex::new(ChaChaRng::from_entropy()));
    for stream in server.incoming() {
        let r = Arc::clone(&rnd);
        spawn(move || {
            let web = accept_with_config(stream.unwrap(), Some(WEBSOCKET_CONFIG)).unwrap();

            let salt = {
                let mut rnd = r.lock().unwrap();
                rnd.next_u64()
            };

            let mut all = CONNECTIONS.lock().unwrap();
            all.push(Conn { web, salt })
        });
    }
    Ok(())
}

async fn run_rabbit() {
    loop {
        match consume().await {
            Ok(_) => println!("rabbit body succeeded. what the hell?"),
            Err(e) => println!("Rabbit error: {}", e)
        }
    }
}

async fn connect() -> Result<Consumer> {
    let rabbit_host = std::env::var("RABBIT_HOST").unwrap_or_else(|_| "rabbitmq".into());
    let rabbit_port: u16 = std::env::var("RABBIT_PORT").ok().and_then(|s| str::parse(s.as_str()).ok()).unwrap_or(5672);
    let addr = format!("amqp://{}:{}/%2f", rabbit_host, rabbit_port);

    println!("Connecting to {}", addr);
    let conn = Connection::connect(&addr, ConnectionProperties::default().with_tokio()).await?;
    println!("Connected to {}", addr);
    let channel_a = conn.create_channel().await?;

    channel_a.exchange_declare("live-activity", ExchangeKind::Fanout, ExchangeDeclareOptions::default(), FieldTable::default()).await?;

    let options = QueueDeclareOptions { passive: false, durable: false, exclusive: true, auto_delete: true, nowait: false };
    let q = channel_a.queue_declare("", options, FieldTable::default()).await?;
    channel_a.queue_bind(q.name().as_str(), "live-activity", "", QueueBindOptions::default(), FieldTable::default()).await?;

    channel_a.basic_consume(q.name().as_str(), "live-activity", BasicConsumeOptions { no_local: false, no_ack: false, exclusive: false, nowait: false }, FieldTable::default()).await
}

async fn consume() -> Result<()> {
    let mut consumer = connect().await?;
    println!("Listening to events");
    while let Some(Ok((_, delivery))) = consumer.next().await {
        delivery.ack(BasicAckOptions::default()).await?;
        if let Err(e) = consume_single(delivery) {
            println!("Error consuming event: {}", e);
        }
    }
    Ok(())
}

fn consume_single(delivery: Delivery) -> Result<()> {
    let s: serde_json::Result<LiveActivityMessage>  = serde_json::from_slice(delivery.data.as_ref());
    match s {
        Ok(str) => {
            CONNECTIONS.lock().unwrap().drain_filter(|c| {
                match c.web.write_message(Message::text(serde_json::to_string(&convert_message(&c, &str)).unwrap())) {
                    Ok(_) => false,
                    Err(SendQueueFull(_)) => false,
                    Err(_) => { println!("Dropping connection {}", c.salt); true }
                }
            });
            Ok(())
        }
        Err(e) => {
            println!("oof {}", e);
            Ok(())
        }
    }
}

fn convert_message(conn: &Conn, msg: &LiveActivityMessage) -> FrontendMessage {
    match msg {
        LiveActivityMessage::Received { event_id, irc_user_name } => FrontendMessage::Received { event_id: *event_id, user: user_id(&conn, irc_user_name) },
        LiveActivityMessage::Sent { event_id, irc_user_name, ping} => FrontendMessage::Sent { event_id: *event_id, user: user_id(&conn, irc_user_name), ping: *ping },
        LiveActivityMessage::ReceivedDetails { event_id, text } => FrontendMessage::MessageDetails { event_id: *event_id, message: text.to_string() }
    }
}

fn user_id(conn: &Conn, name: &String) -> i32 {
    let mut hash = sha2::Sha512::new();
    hash.update(name);
    hash.update(conn.salt.to_be_bytes());
    let mut bytes: [u8; 4] = [ 0, 0, 0, 0 ];
    bytes.clone_from_slice(&hash.finalize()[0..4]);
    i32::from_le_bytes(bytes)
}