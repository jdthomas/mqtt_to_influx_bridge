use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use clap_derive::Parser;
use futures::stream::StreamExt;
use influxdb::InfluxDbWriteable;
use signal_hook::consts::signal::SIGINT;
use signal_hook::consts::signal::SIGTERM;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use tracing::warn;
use tracing_glog::Glog;
use tracing_glog::GlogFields;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;

struct MQTTConfig {
    host: String,
    port: String,
    topic: String,
}

struct InfluxConfig {
    host: String,
    port: String,
    db_name: String,
}

struct Config {
    mqtt: MQTTConfig,
    influx: InfluxConfig,
}
impl Config {
    fn new() -> Self {
        Self {
            mqtt: MQTTConfig {
                host: std::env::var("MQTT_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string()),
                port: std::env::var("MQTT_PORT").unwrap_or_else(|_| "1883".to_string()),
                topic: std::env::var("MQTT_TOPIC")
                    .unwrap_or_else(|_| "test/+/+/+".to_string()),
            },
            influx: InfluxConfig {
                host: std::env::var("INFLUXDB_ADDRESS").unwrap_or_else(|_| "127.0.0.1".to_string()),
                port: std::env::var("INFLUXDB_PORT").unwrap_or_else(|_| "8086".to_string()),
                db_name: std::env::var("INFLUXDB_DB")
                    .unwrap_or_else(|_| "test_db".to_string()),
            },
        }
    }
}

#[derive(InfluxDbWriteable, Debug)]
struct DataPoint {
    time: DateTime<Utc>,
    value: f32,
}

struct Message {
    topic: String,
    data: DataPoint,
}

async fn mqtt_task(
    config: MQTTConfig,
    tx: UnboundedSender<Message>,
    exit_token: CancellationToken,
) -> Result<()> {
    // This is almost verbatium the example code: https://github.com/eclipse/paho.mqtt.rust#example
    let create_opts = paho_mqtt::CreateOptionsBuilder::new()
        .server_uri(&format!("mqtt://{}:{}", config.host, config.port))
        .client_id("mqtt_to_influx_bridge")
        .finalize();

    let mut mqtt_client =
        paho_mqtt::AsyncClient::new(create_opts).context("Error creating the client: {:?}")?;

    let mut strm = mqtt_client.get_stream(25);
    let lwt = paho_mqtt::Message::new("test", "Async subscriber lost connection", paho_mqtt::QOS_1);
    let conn_opts = paho_mqtt::ConnectOptionsBuilder::with_mqtt_version(paho_mqtt::MQTT_VERSION_5)
        .clean_start(false)
        .properties(paho_mqtt::properties![paho_mqtt::PropertyCode::SessionExpiryInterval => 3600])
        .will_message(lwt)
        .finalize();

    info!("Connecting to the MQTT server...");
    mqtt_client.connect(conn_opts).await?;

    let topics = &[config.topic];
    const QOS: &[i32] = &[1, 1];

    info!("Subscribing to topics: {:?}", topics);
    let sub_opts = vec![paho_mqtt::SubscribeOptions::with_retain_as_published(); topics.len()];
    mqtt_client
        .subscribe_many_with_options(topics, QOS, &sub_opts, None)
        .await?;

    info!("Waiting for messages...");

    loop {
        tokio::select! {
            Some(msg_opt) = strm.next() => {
                if let Some(msg) = msg_opt {
                    info!("{} {}",if msg.retained() {
                        "(R) "
                    } else {""},  msg);
                    if let Ok(payload_val) = msg.payload_str().parse::<f32>() {
                        info!("Topic: {}, payload: {}", msg.topic(), payload_val);
                        // FIXME: handle send errors
                        let _ = tx.send(Message {
                            topic: msg.topic().to_string(),
                            data: DataPoint {
                                time: Utc::now(),
                                value: payload_val,
                            }
                        });
                    }
                } else {
                    // A "None" means we were disconnected. Try to reconnect...
                    warn!("Lost connection. Attempting reconnect.");
                    while let Err(err) = mqtt_client.reconnect().await {
                        error!("Error reconnecting: {}", err);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            _ = exit_token.cancelled() => { break; }
        }
    }
    Ok(())
}

async fn infuxdb_task(
    config: InfluxConfig,
    mut rx: UnboundedReceiver<Message>,
    exit_token: CancellationToken,
) -> Result<()> {
    let influxdb_client = influxdb::Client::new(
        format!("http://{}:{}", config.host, config.port),
        config.db_name,
    );

    loop {
        tokio::select! {
            Some(datapoint) = rx.recv() => {
                let write_result = influxdb_client.query(datapoint.data.into_query(datapoint.topic)).await;
                // FIXME: Handle errors
                match write_result{
                    Ok(_) => (),
                    Err(e) => {error!("Problem writting to db: {:?}", e);}
                }
            }
            _ = exit_token.cancelled() => {break;}
        }
    }
    Ok(())
}
fn init_logging(directives: Vec<Directive>) {
    let fmt = tracing_subscriber::fmt::Layer::default()
        .event_format(Glog::default())
        .fmt_fields(GlogFields::default());

    let filter = directives
        .into_iter()
        .fold(EnvFilter::from_default_env(), |filter, directive| {
            filter.add_directive(directive)
        });

    let subscriber = Registry::default().with(filter).with(fmt);
    tracing::subscriber::set_global_default(subscriber).expect("to set global subscriber");
}

async fn listen_for_signals(exit_token: CancellationToken) -> Result<()> {
    let mut signals = signal_hook_tokio::Signals::new(&[SIGTERM, SIGINT])?;
    tokio::select! {
        _signal = signals.next() => {
            println!("Shutting down...");
            exit_token.cancel();
        },
        _ = exit_token.cancelled() => (),
    }
    signals.handle().close();
    Ok(())
}

#[derive(Debug, Parser)]
#[clap(name = "Takes MQTT messages and writes them out to influxdb")]
struct Arguments {
    #[clap(short, long, default_value = "info")]
    log: Vec<Directive>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Arguments::parse();

    init_logging(args.log);
    let config = Config::new();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let exit_token = CancellationToken::new();

    let (_, _, _) = tokio::join!(
        tokio::spawn(listen_for_signals(exit_token.clone())),
        infuxdb_task(config.influx, rx, exit_token.clone()),
        mqtt_task(config.mqtt, tx, exit_token)
    );
    Ok(())
}
